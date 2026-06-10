//! Adaptive bitrate (M2): the load-bearing congestion-control piece.
//!
//! Every ~1s we poll `webrtcbin`'s RTCP stats for the receiver-reported packet loss
//! and adjust the encoder's bitrate with a simple **AIMD** controller (additive
//! increase, multiplicative decrease) plus hysteresis:
//!
//!   * loss > 5%  -> cut bitrate to 85% (back off fast when the link is congested)
//!   * loss < 2%  -> add 500 kbps      (probe for more headroom when it's clean)
//!   * otherwise  -> hold
//!
//! clamped to `[BITRATE_MIN .. BITRATE]`. This trades sharpness for smoothness under
//! congestion (the "laggy over a real network" case) and recovers quality when the
//! network frees up. Disable with `ABR=0` to pin a fixed bitrate.
//!
//! Loss fraction is derived from deltas of the sender's `packets-sent` and the
//! remote receiver report's cumulative `packets-lost`, so it needs no `fraction-lost`
//! field (which isn't always exposed). Stats access is via the synchronous
//! `get-stats` + `Promise::wait`, which is safe to call from any thread.

use gstreamer as gst;
use gstreamer::prelude::*;

pub struct AbrState {
    enabled: bool,
    /// Encoder element factory name (decides the bitrate property's UNIT).
    enc_name: String,
    target_kbps: u32,
    min_kbps: u32,
    max_kbps: u32,
    // Previous cumulative counters, for per-interval deltas.
    prev_sent: u64,
    prev_lost: i64,
    primed: bool,
}

impl AbrState {
    /// Start at the configured `BITRATE` (also the ceiling); floor at `BITRATE_MIN`
    /// (default 1500 kbps). `ABR=0` disables adaptation entirely.
    pub fn new(enc_name: String) -> Self {
        let max = std::env::var("BITRATE")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(12000);
        let min = std::env::var("BITRATE_MIN")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1500);
        let enabled = std::env::var("ABR").as_deref() != Ok("0");
        if !enabled {
            tracing::info!("ABR=0 -> adaptive bitrate disabled (fixed {max} kbps)");
        } else {
            tracing::info!("adaptive bitrate ON (range {min}..{max} kbps, AIMD on RTCP loss)");
        }
        Self {
            enabled,
            enc_name,
            target_kbps: max,
            min_kbps: min.min(max),
            max_kbps: max,
            prev_sent: 0,
            prev_lost: 0,
            primed: false,
        }
    }

    /// One control tick: read stats, run AIMD, apply the new bitrate to `venc`.
    pub fn tick(&mut self, webrtcbin: &gst::Element, venc: &gst::Element) {
        if !self.enabled {
            return;
        }
        let Some((sent, lost, rtt)) = read_loss(webrtcbin) else {
            return; // no sender/receiver-report stats yet
        };

        if !self.primed {
            self.prev_sent = sent;
            self.prev_lost = lost;
            self.primed = true;
            return; // need two samples for a delta
        }

        let d_sent = sent.saturating_sub(self.prev_sent);
        let d_lost = (lost - self.prev_lost).max(0) as u64;
        self.prev_sent = sent;
        self.prev_lost = lost;

        if d_sent == 0 {
            return; // nothing sent this interval; don't react to noise
        }
        let frac = d_lost as f64 / (d_sent + d_lost) as f64;

        let before = self.target_kbps;
        self.target_kbps = aimd_step(self.target_kbps, frac, self.min_kbps, self.max_kbps);

        if self.target_kbps != before {
            set_encoder_bitrate(venc, &self.enc_name, self.target_kbps);
        }
        tracing::info!(
            "abr: loss={:.1}% rtt={:.0}ms bitrate={}kbps{}",
            frac * 100.0,
            rtt * 1000.0,
            self.target_kbps,
            if self.target_kbps != before { " (changed)" } else { "" },
        );
    }
}

/// Pull (packets_sent, packets_lost_cumulative, rtt_seconds) out of webrtcbin stats.
/// Returns None until both a sender outbound-rtp and a remote-inbound report exist.
fn read_loss(webrtcbin: &gst::Element) -> Option<(u64, i64, f64)> {
    // Synchronous stats fetch: emit get-stats with a fresh promise and wait for it.
    let promise = gst::Promise::new();
    webrtcbin.emit_by_name::<()>("get-stats", &[&None::<gst::Pad>, &promise]);
    promise.wait();
    let reply = promise.get_reply()?;

    let mut sent: Option<u64> = None;
    let mut lost: Option<i64> = None;
    let mut rtt: f64 = 0.0;

    // The reply is a flat dict whose every VALUE is a per-stat sub-structure. We
    // identify stat kinds by which fields they carry (avoids enum-binding fragility).
    for (_field, value) in reply.iter() {
        let Ok(s) = value.get::<gst::Structure>() else {
            continue;
        };
        // Sender side: total packets we've sent (u64).
        if let Ok(ps) = s.get::<u64>("packets-sent") {
            sent = Some(sent.unwrap_or(0).max(ps));
        }
        // Receiver report: cumulative packets lost (i32) + RTT (seconds, f64).
        if let Ok(pl) = s.get::<i32>("packets-lost") {
            lost = Some(pl as i64);
            if let Ok(r) = s.get::<f64>("round-trip-time") {
                rtt = r;
            }
        }
    }

    match (sent, lost) {
        (Some(s), Some(l)) => Some((s, l, rtt)),
        _ => None,
    }
}

/// One AIMD step: multiplicative decrease (×0.85) above 5% loss, additive increase
/// (+500) below 2%, hold in between — clamped to `[min, max]`. Pure, so it's unit-tested.
fn aimd_step(target: u32, loss_frac: f64, min: u32, max: u32) -> u32 {
    let next = if loss_frac > 0.05 {
        ((target as f64) * 0.85) as u32
    } else if loss_frac < 0.02 {
        target + 500
    } else {
        target
    };
    next.clamp(min, max)
}

/// Set the encoder bitrate live. The property is named "bitrate" on every encoder in
/// our ladder, but the UNIT differs: openh264enc is bits/s; the rest are kbit/s.
fn set_encoder_bitrate(venc: &gst::Element, enc_name: &str, kbps: u32) {
    let value: u32 = if enc_name.contains("openh264") {
        kbps.saturating_mul(1000) // bits/s
    } else {
        kbps // kbit/s (mf/nv/qsv/amf/x264)
    };
    venc.set_property("bitrate", value);
}

#[cfg(test)]
mod tests {
    use super::aimd_step;

    #[test]
    fn aimd_decrease_increase_hold_and_clamp() {
        // > 5% loss -> ×0.85 (multiplicative decrease)
        assert_eq!(aimd_step(10_000, 0.10, 1500, 12_000), 8500);
        // < 2% loss -> +500 (additive increase)
        assert_eq!(aimd_step(8000, 0.0, 1500, 12_000), 8500);
        // 2%..5% -> hold
        assert_eq!(aimd_step(8000, 0.03, 1500, 12_000), 8000);
        // clamps to the ceiling and the floor
        assert_eq!(aimd_step(11_800, 0.0, 1500, 12_000), 12_000);
        assert_eq!(aimd_step(1600, 0.5, 1500, 12_000), 1500);
    }
}
