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

use std::sync::atomic::{AtomicU32, Ordering};

use gstreamer as gst;
use gstreamer::prelude::*;

/// Hard sanity bounds for a client-requested ceiling (kbps).
const CEILING_MIN_KBPS: u32 = 500;
const CEILING_MAX_KBPS: u32 = 100_000;

/// Client-requested bitrate ceiling (kbps), persisted ACROSS session rebuilds (e.g. a
/// monitor switch tears the session down; the user's quality choice must survive it).
/// 0 = no override (use the `BITRATE` env ceiling).
static CEILING_OVERRIDE_KBPS: AtomicU32 = AtomicU32::new(0);

/// One tick's stats, surfaced to the client HUD over the "control" DataChannel.
#[derive(Clone, Copy, Debug)]
pub struct AbrStats {
    /// The encoder bitrate currently in force (kbps).
    pub kbps: u32,
    /// Packet loss over the last interval, in PERCENT (0..100).
    pub loss_pct: f64,
    /// RTCP round-trip time in milliseconds.
    pub rtt_ms: f64,
}

fn env_ceiling_kbps() -> u32 {
    crate::env::parse_or("BITRATE", 12000)
}

fn env_floor_kbps() -> u32 {
    crate::env::parse_or("BITRATE_MIN", 1500)
}

fn env_adaptive() -> bool {
    crate::env::on("ABR")
}

/// The effective ceiling: the client override when set, else the `BITRATE` env.
fn effective_ceiling_kbps() -> u32 {
    match CEILING_OVERRIDE_KBPS.load(Ordering::Relaxed) {
        0 => env_ceiling_kbps(),
        o => o,
    }
}

/// Forget the client's ceiling override. Called when the peer that set it LEAVES —
/// the override should survive internal session rebuilds (monitor switch) but must
/// not silently cap the next client's session.
pub fn clear_override() {
    if CEILING_OVERRIDE_KBPS.swap(0, Ordering::Relaxed) != 0 {
        tracing::info!("client bitrate override cleared (peer left)");
    }
}

/// `(floor_kbps, ceiling_kbps, adaptive)` — the config reported in the control
/// channel's `hello` (readable before/without a live `AbrState`).
pub fn current_config() -> (u32, u32, bool) {
    let ceiling = effective_ceiling_kbps();
    (env_floor_kbps().min(ceiling), ceiling, env_adaptive())
}

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
    /// Start at the effective ceiling (client override, else `BITRATE`); floor at
    /// `BITRATE_MIN` (default 1500 kbps). `ABR=0` disables adaptation entirely.
    pub fn new(enc_name: String) -> Self {
        let max = effective_ceiling_kbps();
        let min = env_floor_kbps();
        let enabled = env_adaptive();
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

    /// Push the current target to the encoder UNCONDITIONALLY. Called once at session
    /// start: a rebuilt pipeline's encoder comes up at its tuning-string default, and
    /// neither AIMD (which only writes on a CHANGE) nor `ABR=0` (which never writes)
    /// would otherwise ever apply a persisted client ceiling to the new element.
    pub fn apply(&self, venc: &gst::Element) {
        set_encoder_bitrate(venc, &self.enc_name, self.target_kbps);
    }

    /// Apply a client-requested bitrate ceiling LIVE (control message `set-bitrate`).
    /// `kbps == 0` clears the override back to the `BITRATE` env default. With ABR on,
    /// this moves the AIMD ceiling (the target is re-clamped and keeps adapting below
    /// it); with `ABR=0` the fixed bitrate is set directly. Persists across rebuilds.
    pub fn set_ceiling(&mut self, venc: &gst::Element, kbps: u32) {
        let clamped = if kbps == 0 { 0 } else { kbps.clamp(CEILING_MIN_KBPS, CEILING_MAX_KBPS) };
        CEILING_OVERRIDE_KBPS.store(clamped, Ordering::Relaxed);

        self.max_kbps = effective_ceiling_kbps();
        self.min_kbps = env_floor_kbps().min(self.max_kbps);
        let before = self.target_kbps;
        self.target_kbps = if self.enabled {
            // Adaptive: drop under a lowered ceiling immediately; climb via AIMD.
            self.target_kbps.clamp(self.min_kbps, self.max_kbps)
        } else {
            // Fixed mode: the ceiling IS the bitrate.
            self.max_kbps
        };
        if self.target_kbps != before {
            set_encoder_bitrate(venc, &self.enc_name, self.target_kbps);
        }
        tracing::info!(
            "bitrate ceiling -> {} kbps (client request {kbps}; target {} kbps)",
            self.max_kbps,
            self.target_kbps
        );
    }

    /// One control tick: read stats, run AIMD (when enabled), apply the new bitrate to
    /// `venc`. Returns the interval's stats for the client HUD (also when `ABR=0` —
    /// stats reporting works with adaptation pinned). `stats_wanted` = a consumer (the
    /// control channel) exists; with `ABR=0` AND no consumer this never touches
    /// webrtcbin at all — preserving the original escape-hatch guarantee that a
    /// pinned-bitrate host does zero per-second stats work.
    pub fn tick(
        &mut self,
        webrtcbin: &gst::Element,
        venc: &gst::Element,
        stats_wanted: bool,
    ) -> Option<AbrStats> {
        if !self.enabled && !stats_wanted {
            return None;
        }
        let (sent, lost, rtt) = read_loss(webrtcbin)?;

        if !self.primed {
            self.prev_sent = sent;
            self.prev_lost = lost;
            self.primed = true;
            return None; // need two samples for a delta
        }

        let d_sent = sent.saturating_sub(self.prev_sent);
        let d_lost = (lost - self.prev_lost).max(0) as u64;
        self.prev_sent = sent;
        self.prev_lost = lost;

        if d_sent == 0 {
            return None; // nothing sent this interval; don't react to noise
        }
        let frac = d_lost as f64 / (d_sent + d_lost) as f64;

        if self.enabled {
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

        Some(AbrStats {
            kbps: self.target_kbps,
            loss_pct: frac * 100.0,
            rtt_ms: rtt * 1000.0,
        })
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
