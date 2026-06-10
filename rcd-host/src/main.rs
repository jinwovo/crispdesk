//! rcd-host — the HOST side of "rcd" (remote control desktop).
//!
//! The host:
//!   1. captures the Windows desktop (`d3d11screencapturesrc`, or a fallback),
//!   2. hardware-encodes it to H.264 using whichever encoder this machine has
//!      (NVENC / QSV / AMF / Media Foundation), or software-encodes as a last resort,
//!   3. streams it over WebRTC (`webrtcbin`) to the Electron client, and
//!   4. receives mouse input back over an SCTP DataChannel labeled "input".
//!
//! The encoder and capture source are chosen at RUNTIME (see `probe.rs`) so the one
//! binary is UNIVERSAL across GPUs. In WebRTC negotiation the host is the OFFERER.
//!
//! =========================== M1a BRING-UP FLOW ===========================
//! GStreamer is NOT bundled; install it first (see README.md). Then:
//!
//!   Step 0 — `cargo run -- probe`
//!     Prints which H.264 encoder and capture source THIS machine actually supports
//!     (no streaming). Run this first to see your hardware reality.
//!
//!   Step 1 — `cargo run -- preview`
//!     Self-contained, ZERO-networking pipeline: capture -> encode -> decode -> window.
//!     Confirms capture + encode + decode work; if a HW encoder was picked, watch the
//!     "Video Encode" graph in Task Manager.
//!
//!   Step 2 — `cargo run -- stream`
//!     Connects to signaling, joins the pairing room as "host", and once the client is
//!     present negotiates WebRTC and streams video while logging mouse input.
//! ==========================================================================

mod abr;
mod audit;
mod clipboard;
mod embedded_signaling;
mod input;
mod monitors;
mod pipeline;
mod probe;
mod signaling;
mod webrtc;

use anyhow::Result;

/// Make the process per-monitor DPI aware (V2) BEFORE any window / metrics / GStreamer
/// init. Without this, on a scaled display GetSystemMetrics returns LOGICAL pixels and
/// SetCursorPos takes logical coords, while the d3d11 screen capture delivers PHYSICAL
/// pixels — the mismatch offsets the encode size and the injected cursor. Per-monitor-V2
/// makes every Win32 call we use see physical pixels, staying consistent with capture.
#[cfg(windows)]
fn set_dpi_awareness() {
    use windows::Win32::UI::HiDpi::{
        SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
    };
    // SAFETY: a plain Win32 call with no pointer/lifetime invariants.
    unsafe {
        if let Err(e) = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) {
            tracing::warn!("SetProcessDpiAwarenessContext failed: {e} (continuing DPI-unaware)");
        }
    }
}
#[cfg(not(windows))]
fn set_dpi_awareness() {}

/// Initialize logging. Honors `RUST_LOG` (e.g. `RUST_LOG=rcd_host=debug,info`).
/// GStreamer's own logging is controlled separately via the `GST_DEBUG` env var.
fn init_logging() {
    use tracing_subscriber::{fmt, prelude::*, EnvFilter};

    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("rcd_host=info,warn"));

    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(filter)
        .init();
}

fn print_usage() {
    eprintln!(
        "rcd-host — remote control desktop HOST (WebRTC offerer)\n\
         \n\
         USAGE:\n\
         \x20   cargo run -- probe       Print which H.264 encoder + capture source this PC supports.\n\
         \x20   cargo run -- preview     Self-contained capture->encode->decode->window test (no network).\n\
         \x20   cargo run -- stream      Join an EXTERNAL signaling server, stream video, receive input.\n\
         \x20   cargo run -- serve       Run signaling IN-PROCESS (no separate server) + stream.\n\
         \n\
         The encoder + capture source are AUTO-DETECTED at runtime (NVENC -> QSV -> AMF\n\
         -> Media Foundation -> software). Override with the env vars below.\n\
         \n\
         ENVIRONMENT:\n\
         \x20   ENCODER       Force an H.264 encoder element (e.g. \"x264enc\"), skip the probe.\n\
         \x20   CAPTURE       Force a capture source (e.g. \"gdiscreencapturesrc\"), skip the probe.\n\
         \x20   MONITOR       Monitor to capture+control (index; unset=primary). Enumerated at startup.\n\
         \x20   RES           Encode resolution cap WxH (default 1920x1080), or \"native\".\n\
         \x20   BITRATE       Encoder/ABR ceiling in kbps        (default 12000)\n\
         \x20   BITRATE_MIN   Adaptive-bitrate floor in kbps     (default 1500)\n\
         \x20   ABR           Set ABR=0 to pin a fixed bitrate   (default: adaptive on)\n\
         \x20   AUDIO         Set AUDIO=0 to disable system audio (default: on if available)\n\
         \x20   CLIPBOARD     Set CLIPBOARD=0 to disable clipboard sync (default: on)\n\
         \x20   SIGNAL_URL    Signaling WebSocket URL            (default \"ws://127.0.0.1:8080/ws\")\n\
         \x20   PAIRING_CODE  Room / pairing code (DEV_MODE only); else server-issued.\n\
         \x20   STUN          STUN server URI                    (default \"stun://stun.l.google.com:19302\")\n\
         \x20   TURN          Optional TURN URI \"turn://user:pass@host:3478\" (M1b)\n\
         \x20   RUST_LOG      Rust log filter                    (default \"rcd_host=info,warn\")\n\
         \x20   GST_DEBUG     GStreamer log level                (e.g. \"3\", \"webrtcbin:5\")\n"
    );
}

/// We use the multi-thread Tokio runtime for the signaling WebSocket client.
/// The GStreamer side runs on a GLib main loop (see `webrtc::run` / `pipeline::run_preview`),
/// which we drive on a dedicated thread so it does not block the async executor.
#[tokio::main]
async fn main() -> Result<()> {
    init_logging();

    // DPI awareness MUST be set before GStreamer (which creates D3D11 devices/windows)
    // and before any GetSystemMetrics call. See set_dpi_awareness().
    set_dpi_awareness();

    // Initialize GStreamer once, up front, before creating any element.
    gstreamer::init()?;
    tracing::info!("GStreamer initialized: {}", gstreamer::version_string());

    // Minimal arg parsing: the first positional arg selects the mode.
    let mode = std::env::args().nth(1);
    match mode.as_deref() {
        Some("probe") => {
            probe::run_probe()?;
        }
        Some("preview") => {
            tracing::info!("Mode: preview (local capture -> encode -> decode -> window)");
            let probed = probe::probe_all()?;
            pipeline::run_preview(&probed)?;
        }
        Some("stream") => {
            tracing::info!("Mode: stream (WebRTC offerer)");
            webrtc::run().await?;
        }
        Some("serve") => {
            // Embedded signaling + stream: no separate server needed. The client
            // connects to ws://<this-host-ip>:<port>/ws with the PIN below.
            let port: u16 = std::env::var("PORT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(8080);
            let pin = std::env::var("PAIRING_CODE")
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| embedded_signaling::generate_pin(6));

            // Point the host's own WebRTC client at the embedded server.
            std::env::set_var("SIGNAL_URL", format!("ws://127.0.0.1:{port}/ws"));
            std::env::set_var("PAIRING_CODE", &pin);

            tracing::info!("Mode: serve (embedded signaling on :{port} + WebRTC offerer)");
            {
                let pin = pin.clone();
                tokio::spawn(async move {
                    if let Err(e) = embedded_signaling::serve(port, pin).await {
                        tracing::error!("embedded signaling server failed: {e:?}");
                    }
                });
            }
            // Let the listener bind before the host connects to it.
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            tracing::info!("Connect a client to ws://<this-host-ip>:{port}/ws — PIN below.");
            webrtc::run().await?;
        }
        _ => {
            print_usage();
        }
    }

    Ok(())
}
