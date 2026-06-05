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

/// Target encode resolution, parsed from the `RES` env var:
///   unset       -> 1080p cap (good sharpness/latency balance; browser decodes fine)
///   "native"    -> no scaling (full desktop resolution; heaviest)
///   "WxH"       -> explicit cap, e.g. "1280x720"
fn target_res() -> Option<(u32, u32)> {
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

/// Convert a capture source's output to system NV12, scaling to `res` if given.
/// Scaling is done WHERE IT'S CHEAPEST: on the GPU (d3d11convert) for d3d11 capture
/// — so only the already-downscaled frame is copied GPU->system and there is NO
/// per-frame CPU videoscale (the previous big latency/CPU cost). `add-borders=true`
/// keeps aspect ratio (pillar/letterbox) instead of stretching.
fn convert_and_scale(kind: CaptureKind, res: Option<(u32, u32)>) -> String {
    match (kind, res) {
        (CaptureKind::D3d11, Some((w, h))) => format!(
            "d3d11convert add-borders=true ! \
             video/x-raw(memory:D3D11Memory),width={w},height={h} ! \
             d3d11download ! videoconvert"
        ),
        (CaptureKind::D3d11, None) => "d3d11convert ! d3d11download ! videoconvert".to_string(),
        (CaptureKind::System, Some((w, h))) => format!(
            "videoconvert ! videoscale add-borders=true ! video/x-raw,width={w},height={h}"
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
    // Scale to the target resolution on the GPU (see convert_and_scale). Default
    // 1080p; RES=native streams the full desktop; RES=WxH sets an explicit cap.
    let conv = convert_and_scale(probed.capture.kind, target_res());
    let mtu = std::env::var("RTP_MTU").ok().and_then(|s| s.parse::<u32>().ok()).unwrap_or(1200);

    let video = format!(
        "{src} ! {conv} ! \
         {enc} name=venc {tuning} ! \
         h264parse config-interval=-1 ! \
         rtph264pay aggregate-mode=zero-latency pt=96 mtu={mtu} ! \
         application/x-rtp,media=video,encoding-name=H264,payload=96 ! \
         {webrtcbin_name}.",
        src = probed.capture.desc,
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
