//! UNIVERSAL encoder + capture probing.
//!
//! The host must run on ANY Windows GPU (NVIDIA / AMD / Intel / Qualcomm-Adreno)
//! and still produce H.264 even when no hardware encoder is usable. Instead of a
//! single hard-coded `nvh264enc`, we pick the best AVAILABLE element at runtime via
//! two ordered ladders, each validated by actually running a tiny pipeline:
//!
//!   ENCODER ladder (H.264):
//!     nvh264enc  (NVIDIA NVENC)
//!     qsvh264enc (Intel Quick Sync)
//!     amfh264enc (AMD VCN/AMF)
//!     mfh264enc  (Media Foundation — the ONLY HW path for Snapdragon/Adreno,
//!                 and a generic HW fallback on other GPUs)
//!     openh264enc(software, license-clean, SHIPPED fallback)
//!     x264enc    (software, GPL — best dev quality; always-present last resort)
//!
//!   CAPTURE ladder:
//!     d3d11screencapturesrc capture-api=wgc   (Windows Graphics Capture; popup-free)
//!     d3d11screencapturesrc capture-api=dxgi  (DXGI; not under RDP / inactive displays)
//!     gdiscreencapturesrc                     (GDI software capture; last resort)
//!
//! Two-stage probe per candidate:
//!   Stage 1 (cheap): `ElementFactory::find(name)` — GStreamer registers vendor HW
//!     elements (nvh264enc/qsvh264enc/amfh264enc) ONLY when matching hardware is
//!     present, so a registry hit is itself a first-order hardware probe.
//!   Stage 2 (truth): build a THROWAWAY pipeline ending in an appsink and confirm at
//!     least one buffer flows within a timeout. For the encoder we feed a SYNTHETIC
//!     `videotestsrc` (NOT the screen) so a capture problem can never poison encoder
//!     selection. (This mirrors Sunshine's probe philosophy; reimplemented from
//!     scratch — Sunshine is GPL, read-for-ideas only.)
//!
//! Manual overrides skip the probe entirely:
//!   ENCODER=x264enc   force the software encoder (e.g. on this Adreno laptop)
//!   CAPTURE=gdiscreencapturesrc   force a capture source
//!
//! ============================ VERSION SENSITIVITY ============================
//! Element existence is stable, but the low-latency TUNING property names/enums are
//! version- and vendor-specific and are UNVERIFIED STARTING GUESSES. The probe
//! validates the encoder with NO tuning props (so a wrong prop name can't make us
//! skip a working encoder); tuning is applied only in the real pipeline and is the
//! documented on-device tweak point (verify with `gst-inspect-1.0 <element>`).
//! ============================================================================

use anyhow::{anyhow, Result};
use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;

/// How long to wait for the validation pipeline to emit its first buffer.
const PROBE_TIMEOUT_SECS: u64 = 3;

/// Whether the chosen encoder is hardware- or software-backed (affects messaging,
/// and later: quality/latency expectations).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EncoderKind {
    Hardware,
    Software,
}

/// Whether the chosen capture source delivers frames as `D3D11Memory` (GPU) or as
/// plain system memory — decides which conversion the real pipeline splices in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureKind {
    D3d11,
    System,
}

#[derive(Clone, Debug)]
pub struct EncoderChoice {
    /// GStreamer element name, e.g. "nvh264enc".
    pub element: String,
    pub kind: EncoderKind,
    /// Low-latency tuning fragment appended after the element in the pipeline string.
    /// UNVERIFIED per version — the on-device tweak point.
    pub tuning: String,
}

#[derive(Clone, Debug)]
pub struct CaptureChoice {
    /// Source element + its props, e.g. "d3d11screencapturesrc capture-api=wgc show-cursor=true".
    pub desc: String,
    pub kind: CaptureKind,
}

#[derive(Clone, Debug)]
pub struct Probed {
    pub encoder: EncoderChoice,
    pub capture: CaptureChoice,
}

// --- Tuning strings (UNVERIFIED starting guesses; see VERSION SENSITIVITY) -------
// Each is "best effort, low latency". If a property name is wrong for your installed
// GStreamer, the *real* pipeline build will error clearly — adjust here after
// `gst-inspect-1.0 <element>`. The probe itself never applies these.
const NVENC_TUNING: &str = "rc-mode=cbr gop-size=30 bframes=0 bitrate=8000"; // + preset/tune vary by build
const QSV_TUNING: &str = "rate-control=cbr ref-frames=1 bitrate=8000";
const AMF_TUNING: &str = "usage=ultra-low-latency rate-control=cbr bitrate=8000";
// gop-size=30 is CRITICAL: gst-inspect warns mfh264enc may emit "only one keyframe
// at the beginning" otherwise -> if that single IDR's packets are partly lost on a
// real network (Tailscale), the browser can NEVER assemble a keyframe -> permanent
// black screen with a fully-connected transport. A 1s GOP guarantees periodic
// recovery. low-latency=true + bframes=0 keep frames small and in decode order.
// Bitrate (kbit/s) is `BITRATE` env, default 12000 — enough for sharp 1080p text.
fn mf_tuning() -> String {
    let bitrate = std::env::var("BITRATE")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(12000);
    format!("rc-mode=cbr bitrate={bitrate} gop-size=30 bframes=0 low-latency=true")
}
const OPENH264_TUNING: &str = "rate-control=bitrate bitrate=8000000"; // openh264 bitrate is bits/s
const X264_TUNING: &str = "tune=zerolatency speed-preset=veryfast bitrate=8000 key-int-max=30";

fn encoder_ladder() -> Vec<EncoderChoice> {
    use EncoderKind::*;
    vec![
        EncoderChoice { element: "nvh264enc".into(), kind: Hardware, tuning: NVENC_TUNING.into() },
        EncoderChoice { element: "qsvh264enc".into(), kind: Hardware, tuning: QSV_TUNING.into() },
        EncoderChoice { element: "amfh264enc".into(), kind: Hardware, tuning: AMF_TUNING.into() },
        EncoderChoice { element: "mfh264enc".into(), kind: Hardware, tuning: mf_tuning() },
        EncoderChoice { element: "openh264enc".into(), kind: Software, tuning: OPENH264_TUNING.into() },
        EncoderChoice { element: "x264enc".into(), kind: Software, tuning: X264_TUNING.into() },
    ]
}

fn capture_ladder() -> Vec<CaptureChoice> {
    use CaptureKind::*;
    vec![
        CaptureChoice {
            desc: "d3d11screencapturesrc capture-api=wgc show-cursor=true".into(),
            kind: D3d11,
        },
        CaptureChoice {
            desc: "d3d11screencapturesrc capture-api=dxgi show-cursor=true".into(),
            kind: D3d11,
        },
        // Software last resort. (winscreencap plugin; system-memory output.)
        CaptureChoice { desc: "gdiscreencapturesrc".into(), kind: System },
    ]
}

/// First whitespace-delimited token of a description = the element factory name.
fn factory_name(desc: &str) -> &str {
    desc.split_whitespace().next().unwrap_or(desc)
}

/// Stage 1: is the element even registered? (For vendor HW elements this already
/// implies the matching GPU is present.)
fn factory_present(name: &str) -> bool {
    gst::ElementFactory::find(name).is_some()
}

/// Stage 2: build `desc` (which MUST contain `appsink name=sink`), go PLAYING, and
/// return true iff one buffer arrives within the timeout. Always tears down to NULL.
fn produces_buffer(desc: &str) -> bool {
    let element = match gst::parse::launch(desc) {
        Ok(e) => e,
        Err(e) => {
            tracing::debug!("probe parse failed [{desc}]: {e}");
            return false;
        }
    };
    let pipeline = match element.downcast::<gst::Pipeline>() {
        Ok(p) => p,
        Err(_) => return false,
    };
    let sink = match pipeline
        .by_name("sink")
        .and_then(|e| e.downcast::<gst_app::AppSink>().ok())
    {
        Some(s) => s,
        None => {
            let _ = pipeline.set_state(gst::State::Null);
            return false;
        }
    };

    if pipeline.set_state(gst::State::Playing).is_err() {
        let _ = pipeline.set_state(gst::State::Null);
        return false;
    }

    // try_pull_sample blocks up to the timeout for the first sample.
    let got = sink
        .try_pull_sample(gst::ClockTime::from_seconds(PROBE_TIMEOUT_SECS))
        .is_some();

    let _ = pipeline.set_state(gst::State::Null);
    got
}

/// Validate an encoder with a SYNTHETIC source (no screen capture involved) and NO
/// tuning props — purely "does this encoder element accept NV12 and emit H.264".
fn encoder_works(element: &str) -> bool {
    // NOTE: no fixed pixel format here — `videoconvert` negotiates whatever the encoder
    // wants (verified on-device: mfh264enc takes NV12 but openh264enc is I420-only, so
    // pinning NV12 would wrongly fail openh264enc).
    let desc = format!(
        "videotestsrc num-buffers=2 is-live=false ! \
         video/x-raw,width=1280,height=720,framerate=30/1 ! \
         videoconvert ! \
         {element} ! h264parse ! appsink name=sink sync=false"
    );
    produces_buffer(&desc)
}

/// Validate a capture source by pulling one frame through to system memory.
fn capture_works(choice: &CaptureChoice) -> bool {
    let conv = match choice.kind {
        // d3d11 capture lands in D3D11Memory; download to system memory to test fully.
        CaptureKind::D3d11 => "d3d11convert ! d3d11download ! videoconvert",
        CaptureKind::System => "videoconvert",
    };
    let desc = format!(
        "{src} num-buffers=2 ! {conv} ! video/x-raw,format=NV12 ! appsink name=sink sync=false",
        src = choice.desc
    );
    produces_buffer(&desc)
}

/// Look up a ladder entry's tuning for a manually-overridden encoder name (empty if unknown).
fn tuning_for(element: &str) -> String {
    encoder_ladder()
        .into_iter()
        .find(|c| c.element == element)
        .map(|c| c.tuning)
        .unwrap_or_default()
}

fn kind_for(element: &str) -> EncoderKind {
    encoder_ladder()
        .into_iter()
        .find(|c| c.element == element)
        .map(|c| c.kind)
        .unwrap_or(EncoderKind::Software)
}

/// Pick the encoder: honor `ENCODER` override, else walk the ladder (find + run).
pub fn probe_encoder() -> Result<EncoderChoice> {
    if let Ok(forced) = std::env::var("ENCODER") {
        if !forced.is_empty() {
            tracing::info!("ENCODER override = '{forced}' (skipping probe)");
            return Ok(EncoderChoice {
                kind: kind_for(&forced),
                tuning: tuning_for(&forced),
                element: forced,
            });
        }
    }

    for cand in encoder_ladder() {
        if !factory_present(&cand.element) {
            tracing::debug!("encoder '{}' not registered; skipping", cand.element);
            continue;
        }
        if encoder_works(&cand.element) {
            tracing::info!("selected encoder '{}' ({:?})", cand.element, cand.kind);
            return Ok(cand);
        }
        tracing::warn!(
            "encoder '{}' is registered but failed the run-probe; trying next",
            cand.element
        );
    }

    Err(anyhow!(
        "no working H.264 encoder found — not even the software fallback. \
         Is the GStreamer plugin set complete (openh264/x264, libav)? \
         Try `gst-inspect-1.0 openh264enc`."
    ))
}

/// Pick the capture source: honor `CAPTURE` override, else walk the ladder.
pub fn probe_capture() -> Result<CaptureChoice> {
    if let Ok(forced) = std::env::var("CAPTURE") {
        if !forced.is_empty() {
            tracing::info!("CAPTURE override = '{forced}' (skipping probe)");
            // Heuristic: d3d11 source => D3D11Memory, anything else => system memory.
            let kind = if forced.contains("d3d11") {
                CaptureKind::D3d11
            } else {
                CaptureKind::System
            };
            return Ok(CaptureChoice { desc: forced, kind });
        }
    }

    for cand in capture_ladder() {
        if !factory_present(factory_name(&cand.desc)) {
            tracing::debug!("capture '{}' not registered; skipping", factory_name(&cand.desc));
            continue;
        }
        if capture_works(&cand) {
            tracing::info!("selected capture '{}' ({:?})", cand.desc, cand.kind);
            return Ok(cand);
        }
        tracing::warn!("capture '{}' failed the run-probe; trying next", cand.desc);
    }

    Err(anyhow!(
        "no working screen-capture source found (tried d3d11 WGC/DXGI and gdi). \
         On this Adreno/ARM laptop d3d11 capture is a known unknown — see README."
    ))
}

/// Probe both and return the combined choice.
pub fn probe_all() -> Result<Probed> {
    let encoder = probe_encoder()?;
    let capture = probe_capture()?;
    Ok(Probed { encoder, capture })
}

/// Whether to add a system-audio (loopback) track. Audio is ADDITIVE: if anything
/// here is missing we stream video-only, so audio can never break the video path.
/// Disable explicitly with `AUDIO=0`. We gate on element PRESENCE only (not a buffer
/// probe) because WASAPI loopback legitimately yields no buffers while the system is
/// silent — that must NOT disable audio.
pub fn audio_available() -> bool {
    if std::env::var("AUDIO").as_deref() == Ok("0") {
        tracing::info!("AUDIO=0 -> system audio disabled");
        return false;
    }
    let ok = factory_present("wasapi2src")
        && factory_present("opusenc")
        && factory_present("rtpopuspay");
    if ok {
        tracing::info!("system audio available (wasapi2src loopback + Opus)");
    } else {
        tracing::warn!("system-audio elements missing; streaming VIDEO ONLY");
    }
    ok
}

/// `cargo run -- probe`: print, without streaming, exactly what THIS machine supports.
/// Handy first diagnostic on any host (especially this Snapdragon, where it should
/// pick gdi/d3d11 capture + a software encoder).
pub fn run_probe() -> Result<()> {
    println!("\n=== rcd-host capability probe ===\n");

    println!("ENCODERS (priority order):");
    for cand in encoder_ladder() {
        let present = factory_present(&cand.element);
        let status = if !present {
            "absent".to_string()
        } else if encoder_works(&cand.element) {
            "OK (works)".to_string()
        } else {
            "registered but FAILED run-probe".to_string()
        };
        println!("  {:<12} {:<9} {}", cand.element, format!("{:?}", cand.kind), status);
    }

    println!("\nCAPTURE (priority order):");
    for cand in capture_ladder() {
        let name = factory_name(&cand.desc);
        let present = factory_present(name);
        let status = if !present {
            "absent".to_string()
        } else if capture_works(&cand) {
            "OK (works)".to_string()
        } else {
            "registered but FAILED run-probe".to_string()
        };
        println!("  {:<40} {}", cand.desc, status);
    }

    println!("\nSELECTION:");
    match probe_all() {
        Ok(p) => {
            println!("  encoder = {} ({:?})", p.encoder.element, p.encoder.kind);
            println!("  capture = {} ({:?})", p.capture.desc, p.capture.kind);
            if p.encoder.kind == EncoderKind::Software {
                println!(
                    "\n  NOTE: software encoder selected — guaranteed to RUN, but quality/latency\n\
                     \x20       is CPU-bound. For hardware quality use a PC with an NVIDIA/AMD/Intel GPU,\n\
                     \x20       or (on Snapdragon) verify native ARM64 mfh264enc lights up Video Encode."
                );
            }
        }
        Err(e) => println!("  no viable combination: {e}"),
    }
    println!();
    Ok(())
}
