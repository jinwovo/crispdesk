//! Pipeline construction shared by the preview and streaming modes.
//!
//! Both consumers build on the runtime-chosen encoder + capture source (see
//! `probe.rs`) so the SAME host binary works on NVIDIA / AMD / Intel / Adreno and
//! falls back to software encode when no hardware encoder is usable:
//!   * `run_preview()`        — self-contained, no-networking sanity pipeline.
//!   * `capture_to_rtp_chain()` — the capture->encode->parse->pay string that
//!     `webrtc.rs` splices in front of `webrtcbin`.
//!
//! UNIVERSAL conversion strategy: we deliberately route every capture source down to
//! SYSTEM-memory NV12 (`... ! videoconvert ! video/x-raw,format=NV12 ! <enc>`) before
//! the encoder. Every encoder in the ladder (HW and software) accepts system NV12, so
//! this is the maximally-compatible path. It costs one GPU->system copy per frame for
//! the d3d11 capture path; TODO(perf): add a zero-copy D3D11Memory fast-path for known
//! d3d11-native encoders (nvd3d11h264enc / qsv d3d11 / amf) once the base path is proven.

use anyhow::{Context, Result};
use gstreamer as gst;
use gstreamer::prelude::*;

use crate::probe::{CaptureKind, Probed};

/// The conversion segment that turns a capture source's output into system NV12.
/// (Used by preview; the streaming path uses `convert_and_scale` so it can scale.)
fn conversion(kind: CaptureKind) -> &'static str {
    match kind {
        // d3d11 capture delivers D3D11Memory: convert on-GPU, then download to system.
        CaptureKind::D3d11 => "d3d11convert ! d3d11download ! videoconvert",
        // gdi/software capture is already system memory.
        CaptureKind::System => "videoconvert",
    }
}

/// The resolution CAP from the `RES` env var (the encoded frame is fit WITHIN this
/// while preserving the desktop's aspect ratio — see `scaled_dims`):
///   unset       -> 1080p cap (good sharpness/latency balance)
///   "native"    -> no scaling (full desktop resolution; heaviest)
///   "WxH"       -> explicit cap, e.g. "1280x720"
fn res_cap() -> Option<(u32, u32)> {
    match std::env::var("RES").ok().as_deref() {
        Some("native") => None,
        Some(res) if res.contains('x') => {
            let mut it = res.split('x');
            let w = it.next().and_then(|s| s.parse().ok()).unwrap_or(1920);
            let h = it.next().and_then(|s| s.parse().ok()).unwrap_or(1080);
            Some((w, h))
        }
        _ => Some((1920, 1080)),
    }
}

/// Compute the EXACT encode dimensions: the desktop fit within the cap, preserving
/// aspect ratio (NEVER upscaling), rounded to even numbers (H.264 needs even dims).
///
/// Matching the desktop's aspect ratio exactly means the encoded frame IS the
/// desktop with NO letterbox bars. That is essential for input: the client sends
/// mouse positions normalized to the video content (0..1) and the host maps them
/// across the full desktop — baked-in black bars would offset every coordinate and
/// make clicks miss (cursor "moves but doesn't land"). Returns None for "native".
fn scaled_dims() -> Option<(u32, u32)> {
    let cap = res_cap()?; // None => native, no scaling
    // Use the SELECTED monitor's size (multi-monitor) else the primary. This keeps the
    // encode aspect ratio matched to whichever display is actually being captured.
    let src = match crate::monitors::selected() {
        Some(m) => (m.width.max(1) as u32, m.height.max(1) as u32),
        None => crate::input::primary_resolution().unwrap_or(cap),
    };
    Some(fit_within(src, cap))
}

/// Fit `src` within `cap` preserving aspect ratio, never upscaling, with even dims
/// (H.264 requires even width/height). Pure, so it's unit-tested.
fn fit_within(src: (u32, u32), cap: (u32, u32)) -> (u32, u32) {
    let (sw, sh) = (src.0.max(1), src.1.max(1));
    let scale = (cap.0 as f64 / sw as f64)
        .min(cap.1 as f64 / sh as f64)
        .min(1.0); // never upscale
    let even = |v: f64| ((v.round() as u32) & !1).max(2);
    (even(sw as f64 * scale), even(sh as f64 * scale))
}

/// The capture source description with the selected monitor pinned, when applicable.
/// For d3d11 capture we set `monitor-handle=<HMONITOR>` so capture targets exactly the
/// monitor whose rect we use for input mapping (DXGI index ordering is not guaranteed
/// to match GDI enumeration, so we bind by handle, not index).
fn capture_src(probed: &Probed) -> String {
    match (probed.capture.kind, crate::monitors::selected()) {
        (CaptureKind::D3d11, Some(m)) => {
            format!("{} monitor-handle={}", probed.capture.desc, m.handle as u64)
        }
        _ => probed.capture.desc.clone(),
    }
}

/// Convert a capture source's output to system NV12, scaling to exact `dims` if given.
/// Scaling is done WHERE IT'S CHEAPEST: on the GPU (d3d11convert) for d3d11 capture
/// — so only the already-downscaled frame is copied GPU->system and there is NO
/// per-frame CPU videoscale. NO `add-borders`: `dims` already matches the desktop
/// aspect ratio, so the frame fills exactly with no bars (see `scaled_dims`).
fn convert_and_scale(kind: CaptureKind, dims: Option<(u32, u32)>) -> String {
    match (kind, dims) {
        (CaptureKind::D3d11, Some((w, h))) => format!(
            "d3d11convert ! \
             video/x-raw(memory:D3D11Memory),width={w},height={h} ! \
             d3d11download ! videoconvert"
        ),
        (CaptureKind::D3d11, None) => "d3d11convert ! d3d11download ! videoconvert".to_string(),
        (CaptureKind::System, Some((w, h))) => format!(
            "videoconvert ! videoscale ! video/x-raw,width={w},height={h}"
        ),
        (CaptureKind::System, None) => "videoconvert".to_string(),
    }
}

/// Build the capture + encode + parse + RTP-payload chain as a `gst-launch`-style string.
///
/// Front half of the STREAMING pipeline; `webrtc.rs` appends `webrtcbin name=<name>`
/// and the payloader targets it. Ends exactly as pinned by PROTOCOL.md:
///   `... ! h264parse config-interval=-1 ! rtph264pay aggregate-mode=zero-latency pt=96`
pub fn capture_to_rtp_chain(probed: &Probed, webrtcbin_name: &str) -> String {
    // Scale to exact aspect-correct dimensions on the GPU (see convert_and_scale /
    // scaled_dims). Default fits within 1080p; RES=native streams the full desktop.
    let conv = convert_and_scale(probed.capture.kind, scaled_dims());
    let mtu = std::env::var("RTP_MTU").ok().and_then(|s| s.parse::<u32>().ok()).unwrap_or(1200);

    let video = format!(
        "{src} ! {conv} ! \
         {enc} name=venc {tuning} ! \
         h264parse config-interval=-1 ! \
         rtph264pay aggregate-mode=zero-latency pt=96 mtu={mtu} ! \
         application/x-rtp,media=video,encoding-name=H264,payload=96 ! \
         {webrtcbin_name}.",
        src = capture_src(probed),
        enc = probed.encoder.element,
        tuning = probed.encoder.tuning,
    );

    // Optional second branch: system audio (loopback) -> Opus -> a separate audio
    // m-line on the SAME webrtcbin. Only added when audio_available() (env + element
    // presence) so it can never break the video-only path.
    let audio = if crate::probe::audio_available() {
        format!(
            " wasapi2src loopback=true low-latency=true ! \
             audioconvert ! audioresample ! \
             audio/x-raw,rate=48000,channels=2 ! \
             opusenc ! \
             rtpopuspay pt=97 ! \
             application/x-rtp,media=audio,encoding-name=OPUS,payload=97 ! \
             {webrtcbin_name}."
        )
    } else {
        String::new()
    };

    format!("{video}{audio}")
}

/// PREVIEW mode: self-contained pipeline that exercises capture + encode + decode with
/// ZERO networking, to confirm the chosen encoder/capture work before touching WebRTC.
///
/// Pipeline (decode side uses `decodebin` so it works regardless of which HW/SW decoder
/// is available):
///   <capture> ! <conv> ! NV12 ! <enc> ! h264parse ! decodebin ! videoconvert ! autovideosink
pub fn run_preview(probed: &Probed) -> Result<()> {
    let description = format!(
        "{src} ! {conv} ! \
         {enc} {tuning} ! \
         h264parse config-interval=-1 ! \
         decodebin ! videoconvert ! autovideosink",
        src = probed.capture.desc,
        conv = conversion(probed.capture.kind),
        enc = probed.encoder.element,
        tuning = probed.encoder.tuning,
    );

    tracing::info!("Preview pipeline:\n  {description}");
    tracing::info!(
        "Encoder = {} ({:?}); capture = {}",
        probed.encoder.element,
        probed.encoder.kind,
        probed.capture.desc
    );
    tracing::info!(
        "If a HARDWARE encoder was selected, watch Task Manager -> Performance -> GPU \
         -> 'Video Encode' to confirm the GPU encoder is active."
    );

    let pipeline = gst::parse::launch(&description).context(
        "failed to construct preview pipeline (is GStreamer + the d3d11/nvcodec/\
         mediafoundation/libav plugins installed?)",
    )?;
    let pipeline = pipeline
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow::anyhow!("parsed preview pipeline is not a gst::Pipeline"))?;

    pipeline
        .set_state(gst::State::Playing)
        .context("failed to set preview pipeline to PLAYING")?;

    let bus = pipeline.bus().context("preview pipeline has no bus")?;

    tracing::info!("Preview running. Press Ctrl-C in this terminal to stop.");
    for msg in bus.iter_timed(gst::ClockTime::NONE) {
        use gst::MessageView;
        match msg.view() {
            MessageView::Eos(..) => {
                tracing::info!("Preview: end of stream");
                break;
            }
            MessageView::Error(err) => {
                tracing::error!(
                    "Preview error from {:?}: {} ({:?})",
                    err.src().map(|s| s.path_string()),
                    err.error(),
                    err.debug()
                );
                break;
            }
            MessageView::StateChanged(sc) => {
                if sc
                    .src()
                    .map(|s| s == pipeline.upcast_ref::<gst::Object>())
                    .unwrap_or(false)
                {
                    tracing::debug!("Pipeline state: {:?} -> {:?}", sc.old(), sc.current());
                }
            }
            _ => {}
        }
    }

    pipeline
        .set_state(gst::State::Null)
        .context("failed to set preview pipeline to NULL")?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::fit_within;

    #[test]
    fn fit_preserves_aspect_no_upscale_even_dims() {
        // 3:2 desktop into a 1080p cap -> 1620x1080 (height-bound), no letterbox.
        assert_eq!(fit_within((2880, 1920), (1920, 1080)), (1620, 1080));
        // Already within the cap -> unchanged (never upscale).
        assert_eq!(fit_within((1280, 720), (1920, 1080)), (1280, 720));
        // Exact 16:9 fills the cap.
        assert_eq!(fit_within((1920, 1080), (1920, 1080)), (1920, 1080));
        // Odd inputs round to even dimensions.
        let (w, h) = fit_within((1001, 667), (1920, 1080));
        assert_eq!(w % 2, 0);
        assert_eq!(h % 2, 0);
    }
}
