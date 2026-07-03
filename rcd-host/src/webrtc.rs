//! STREAM mode — the WebRTC OFFERER.
//!
//! Builds the streaming pipeline:
//!   <capture -> encode -> h264parse config-interval=-1 -> rtph264pay aggregate-mode=zero-latency pt=96>
//!     -> webrtcbin (name=webrtcbin, bundle-policy=max-bundle)
//!
//! and drives full WebRTC negotiation as the offerer:
//!   * builds a FRESH pipeline+webrtcbin PER CLIENT SESSION (so the client can
//!     refresh/reconnect any number of times — a webrtcbin can't renegotiate a
//!     torn-down session),
//!   * `bundle-policy=max-bundle` so video + the DataChannel share ONE ICE
//!     transport (verified on-device: with the default policy the DataChannel's
//!     transport can connect while the video's separate transport fails -> black
//!     screen with working input),
//!   * creates the "input" DataChannel (ordered=false, max-retransmits=0) BEFORE
//!     negotiation (and AFTER the pipeline is READY — in NULL it returns None),
//!   * on `on-negotiation-needed` -> create-offer -> set-local-description -> send offer,
//!   * on `on-ice-candidate` -> trickle to the client,
//!   * on inbound "answer"/"ice" -> apply to the current session,
//!   * decodes DataChannel input (PROTOCOL.md opcodes) and injects via src/input.rs.
//!
//! ============================ VERSION SENSITIVITY ============================
//! The webrtcbin signal names and promise/structure dance are the parts most likely
//! to differ across gstreamer-rs releases. Modeled on the official gstreamer-rs
//! webrtc example; verified against gstreamer-rs 0.23 + GStreamer 1.28.3.
//! =============================================================================

use std::sync::Arc;

use anyhow::{Context, Result};
use gstreamer as gst;
use gstreamer::glib;
use gstreamer::prelude::*;
use gstreamer_sdp as gst_sdp;
use gstreamer_webrtc as gst_webrtc;
use tokio::sync::mpsc;

use crate::pipeline;
use crate::probe::Probed;
use crate::signaling::{self, IceServerCfg, Role, SignalMessage};

/// Default STUN server (matches the client). Note webrtcbin wants the `stun://` scheme.
const DEFAULT_STUN: &str = "stun://stun.l.google.com:19302";

/// Input opcodes (PROTOCOL.md). All multi-byte fields are little-endian.
const OPCODE_MOUSE_MOVE_ABS: u8 = 0x01; // [f32 x][f32 y] normalized 0..1
const OPCODE_MOUSE_BUTTON: u8 = 0x02; //   [u8 button 0=L 1=R 2=M 3=X1 4=X2][u8 down(1)/up(0)]
const OPCODE_KEY: u8 = 0x03; //            [u16 scancode][u8 flags bit0=down bit1=extended]
const OPCODE_WHEEL: u8 = 0x04; //          [i16 wheelY][i16 wheelX] WHEEL_DELTA units (+120/notch)
// TODO(milestone): 0x05 GAMEPAD (ViGEmBus).

/// Commands marshalled from DataChannel callbacks (GLib thread) into the host's
/// main loop below, which owns the `Session` and can rebuild/retune it.
#[derive(Debug, Clone, Copy)]
pub enum HostCmd {
    /// Rebuild the session capturing monitor `index` (control `switch-monitor`).
    SwitchMonitor(usize),
    /// Move the encoder/ABR bitrate ceiling live (control `set-bitrate`; 0 = default).
    SetBitrate(u32),
}

/// Shared bundle so signal callbacks (which need 'static closures) can reach the
/// pieces they require: the webrtcbin element and a sender back into signaling.
struct AppState {
    webrtcbin: gst::Element,
    /// Send messages out to the signaling server (offer / ice).
    signal_tx: mpsc::UnboundedSender<SignalMessage>,
}

/// One live client connection: its pipeline, shared state, and the bus watch
/// (the guard removes the watch when the session is dropped).
struct Session {
    pipeline: gst::Pipeline,
    state: Arc<AppState>,
    /// The encoder element ("venc"), kept so adaptive bitrate can retune it live.
    venc: Option<gst::Element>,
    /// Per-session adaptive-bitrate controller (see abr.rs).
    abr: crate::abr::AbrState,
    _bus_watch: gst::bus::BusWatchGuard,
}

/// Entry point for `cargo run -- stream`.
pub async fn run() -> Result<()> {
    let (signal_url, pairing_code) = signaling::config_from_env();

    // Resolve the capture/control monitor (MONITOR env; default primary) and log all.
    crate::monitors::init_from_env();

    // Probe ONCE for a working encoder + capture source (reused across reconnects).
    let probed = crate::probe::probe_all().context("encoder/capture probe failed")?;
    tracing::info!(
        "Using encoder '{}' ({:?}) + capture '{}'",
        probed.encoder.element,
        probed.encoder.kind,
        probed.capture.desc
    );

    // Start the host clipboard poller once (it forwards LOCAL clipboard changes to the
    // current session's "clipboard" DataChannel; see clipboard.rs). No-op if disabled.
    crate::clipboard::start_poller();

    // Drive a shared GLib main loop on a dedicated thread: webrtcbin signals, bus
    // watches, and promises are serviced here for ALL sessions.
    let main_loop = glib::MainLoop::new(None, false);
    {
        let main_loop = main_loop.clone();
        std::thread::spawn(move || main_loop.run());
    }

    let mut session: Option<Session> = None;
    let mut ctrl_c = Box::pin(tokio::signal::ctrl_c());
    let mut backoff_secs = 1u64;
    // Control-channel commands (monitor switch / bitrate) arrive on the GLib thread;
    // this channel marshals them into the loop below, which owns the Session.
    let (cmd_tx, mut cmd_rx) = mpsc::unbounded_channel::<HostCmd>();
    // Latest ICE servers from the signaling server (STUN + ephemeral TURN creds).
    // The server sends these right after we join, before any negotiation.
    let mut ice_servers: Vec<IceServerCfg> = Vec::new();

    // Outer RECONNECT loop: the host stays up and keeps re-joining the room across
    // signaling drops (e.g. a self-hosted server / NAS rebooting). Ctrl-C exits.
    'reconnect: loop {
        // Connect (abortable by Ctrl-C). On failure, back off and retry.
        let sig = tokio::select! {
            _ = &mut ctrl_c => break 'reconnect,
            r = signaling::connect(&signal_url, &pairing_code) => r,
        };
        let mut sig = match sig {
            Ok(s) => {
                backoff_secs = 1;
                s
            }
            Err(e) => {
                tracing::error!("signaling connect failed: {e:?}; retrying in {backoff_secs}s");
                tokio::select! {
                    _ = &mut ctrl_c => break 'reconnect,
                    _ = tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)) => {}
                }
                backoff_secs = (backoff_secs * 2).min(30);
                continue 'reconnect;
            }
        };

        tracing::info!("Host ready; waiting for a client to join room '{pairing_code}'...");

        // Drive adaptive bitrate once per second whenever a session is live.
        let mut abr_tick = tokio::time::interval(std::time::Duration::from_secs(1));
        abr_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        // Inner loop: service this signaling connection until it drops or Ctrl-C.
        loop {
            tokio::select! {
                biased;

                _ = &mut ctrl_c => {
                    tracing::info!("Ctrl-C received; shutting down.");
                    break 'reconnect;
                }

                maybe_msg = sig.inbound.recv() => {
                    let Some(msg) = maybe_msg else {
                        tracing::warn!("signaling connection lost; tearing down and reconnecting");
                        break; // inner break -> reconnect
                    };
                    handle_signal(&mut session, &sig.outbound, &probed, &mut ice_servers, &cmd_tx, msg);
                }

                maybe_cmd = cmd_rx.recv() => {
                    // cmd_tx lives in this scope, so recv() cannot return None; guard anyway.
                    if let Some(cmd) = maybe_cmd {
                        handle_command(&mut session, &sig.outbound, &probed, &ice_servers, &cmd_tx, cmd).await;
                    }
                }

                _ = abr_tick.tick() => {
                    if let Some(s) = session.as_mut() {
                        // Clone the (refcounted) elements so the &mut borrow of `s.abr`
                        // doesn't conflict with the &borrows of webrtcbin/venc.
                        let wb = s.state.webrtcbin.clone();
                        if let Some(venc) = s.venc.clone() {
                            // Forward the tick's stats to the client HUD. With ABR=0
                            // and no control channel open, tick() skips the get-stats
                            // call entirely (the original escape-hatch guarantee).
                            if let Some(st) = s.abr.tick(&wb, &venc, crate::control::has_channel()) {
                                crate::control::send(&crate::control::ToClient::Stats {
                                    encoder_kbps: st.kbps,
                                    loss_pct: (st.loss_pct * 10.0).round() / 10.0,
                                    rtt_ms: st.rtt_ms.round(),
                                });
                            }
                        }
                    }
                }
            }
        }

        // Signaling dropped: the peer is unreachable, so end the session (which also
        // releases any stuck keys), then reconnect after a short backoff.
        teardown_session(&mut session);
        tokio::select! {
            _ = &mut ctrl_c => break 'reconnect,
            _ = tokio::time::sleep(std::time::Duration::from_secs(backoff_secs)) => {}
        }
        backoff_secs = (backoff_secs * 2).min(30);
    }

    teardown_session(&mut session);
    main_loop.quit();
    Ok(())
}

/// Tear down the current session (if any): stop the pipeline and drop everything.
fn teardown_session(session: &mut Option<Session>) {
    if let Some(s) = session.take() {
        if let Err(e) = s.pipeline.set_state(gst::State::Null) {
            tracing::warn!("failed to set old session pipeline to NULL: {e}");
        }
        // Backstop: a client that vanished without sending key/button-ups must not
        // leave stuck modifiers or held mouse buttons on the host desktop.
        crate::input::release_all();
        // Drop the clipboard/control/file channel handles so nothing sends into a
        // dead session, and abort any in-flight file transfers (deletes .part temps).
        crate::clipboard::set_channel(None);
        crate::control::set_channel(None);
        crate::files::set_channel(None);
        crate::files::reset();
        crate::audit::log_event("session_end", &[]);
        tracing::info!("session torn down");
    }
}

/// Build a FRESH session for a newly-present client and start negotiating.
/// `rebuild` = an internal restart for the SAME already-connected peer (e.g. a
/// monitor switch), as opposed to a new client session. Returns success — a caller
/// that triggered a rebuild can roll back when the new pipeline fails to come up.
fn start_session(
    session: &mut Option<Session>,
    signal_tx: &mpsc::UnboundedSender<SignalMessage>,
    probed: &Probed,
    ice_servers: &[IceServerCfg],
    cmd_tx: &mpsc::UnboundedSender<HostCmd>,
    rebuild: bool,
) -> bool {
    // A previous session (e.g. the client refreshed the page) cannot be reused.
    teardown_session(session);

    let (pipeline, state) = match build_pipeline(signal_tx.clone(), probed, ice_servers, cmd_tx) {
        Ok(ps) => ps,
        Err(e) => {
            tracing::error!("failed to build session pipeline: {e:?}");
            return false;
        }
    };

    // Log pipeline errors/EOS for this session via a bus watch on the shared loop.
    let bus = match pipeline.bus() {
        Some(b) => b,
        None => {
            tracing::error!("session pipeline has no bus");
            return false;
        }
    };
    let watch = bus.add_watch(move |_bus, msg| {
        use gst::MessageView;
        match msg.view() {
            MessageView::Error(err) => {
                tracing::error!(
                    "pipeline error from {:?}: {} ({:?})",
                    err.src().map(|s| s.path_string()),
                    err.error(),
                    err.debug()
                );
            }
            MessageView::Eos(..) => tracing::info!("pipeline EOS"),
            _ => {}
        }
        glib::ControlFlow::Continue
    });
    let watch = match watch {
        Ok(w) => w,
        Err(e) => {
            tracing::error!("failed to add bus watch: {e}");
            return false;
        }
    };

    tracing::info!("Client present -> starting a fresh WebRTC session (PLAYING)");
    if let Err(e) = pipeline.set_state(gst::State::Playing) {
        tracing::error!("failed to set session pipeline to PLAYING: {e}");
        return false;
    }

    // Consent gate: with REQUIRE_CONSENT=1 input stays blocked until the seated user
    // approves the modal dialog; otherwise input is enabled immediately (unchanged).
    // An internal REBUILD keeps the existing gate — it is the same, already-approved
    // peer, and re-prompting would revoke input on an unattended host mid-session.
    if !rebuild {
        if std::env::var("REQUIRE_CONSENT").as_deref() == Ok("1") {
            crate::input::request_consent();
        } else {
            crate::input::set_input_allowed(true);
        }
    }
    crate::audit::log_event("session_start", &[("encoder", &probed.encoder.element)]);

    // Adaptive bitrate retunes this session's encoder; grab it by the name set in
    // pipeline.rs ("venc"). Absent only if the pipeline shape changed.
    let venc = pipeline.by_name("venc");
    let abr = crate::abr::AbrState::new(probed.encoder.element.clone());
    // The fresh encoder element starts at its tuning-string default — push the
    // (possibly client-overridden, rebuild-surviving) target explicitly.
    if let Some(v) = venc.as_ref() {
        abr.apply(v);
    }

    *session = Some(Session {
        pipeline,
        state,
        venc,
        abr,
        _bus_watch: watch,
    });
    true
}

/// React to one inbound signaling message.
fn handle_signal(
    session: &mut Option<Session>,
    signal_tx: &mpsc::UnboundedSender<SignalMessage>,
    probed: &Probed,
    ice_servers: &mut Vec<IceServerCfg>,
    cmd_tx: &mpsc::UnboundedSender<HostCmd>,
    msg: SignalMessage,
) {
    match msg {
        // Our own join ack. If the room already has 2 peers, the client is present.
        SignalMessage::Joined { role, peers } => {
            tracing::info!("joined room as {role:?}; peers in room = {peers}");
            if peers >= 2 {
                start_session(session, signal_tx, probed, ice_servers, cmd_tx, false);
            }
        }
        // The other peer joined (or rejoined after a refresh): start a fresh session.
        SignalMessage::PeerJoined { role } => {
            tracing::info!("peer joined: {role:?}");
            if role == Role::Client {
                start_session(session, signal_tx, probed, ice_servers, cmd_tx, false);
            }
        }
        // The signaling server handed us ICE servers (STUN + ephemeral TURN). Store
        // them; the next session build applies them to webrtcbin.
        SignalMessage::IceServers { ice_servers: ice } => {
            tracing::info!("received {} ICE server entr(ies) from signaling", ice.len());
            *ice_servers = ice;
        }
        SignalMessage::PeerLeft { role } => {
            tracing::warn!("peer left: {role:?}");
            teardown_session(session);
            // The departing peer's bitrate choice must not cap the NEXT client's
            // session (it deliberately survives internal rebuilds, not peer changes).
            crate::abr::clear_override();
            // Optionally lock the workstation so the desktop isn't left unlocked after
            // a remote session ends (only on a real peer-leave, not a session rebuild).
            if std::env::var("LOCK_ON_DISCONNECT").as_deref() == Ok("1") {
                crate::input::lock_workstation();
            }
        }
        SignalMessage::Error { message } => {
            tracing::error!("signaling error: {message}");
        }
        // The server issued our pairing code — show it prominently for the user to
        // hand to a client. (Dynamic-pairing mode; see rcd-signal.)
        SignalMessage::CodeAssigned { code, expires_at } => {
            tracing::info!("==================================================");
            tracing::info!("   PAIRING CODE:  {code}");
            tracing::info!("   Enter this code on the client to connect.");
            tracing::info!("   (expires at unix-ms {expires_at})");
            tracing::info!("==================================================");
        }
        // The client's SDP answer -> set as remote description on the live session.
        SignalMessage::Answer { sdp } => {
            tracing::info!("received answer ({} bytes of SDP)", sdp.len());
            match session.as_ref() {
                Some(s) => {
                    if let Err(e) = apply_answer(&s.state, &sdp) {
                        tracing::error!("failed to apply answer: {e:?}");
                    }
                }
                None => tracing::warn!("answer received but no live session; ignoring"),
            }
        }
        // A trickled ICE candidate from the client -> add to the live session.
        SignalMessage::Ice {
            candidate,
            sdp_mline_index,
            ..
        } => {
            let Some(s) = session.as_ref() else {
                tracing::warn!("ICE received but no live session; ignoring");
                return;
            };
            let mline = sdp_mline_index.unwrap_or(0);
            tracing::debug!("adding remote ICE candidate (mline={mline}): {candidate}");
            // webrtcbin "add-ice-candidate" takes (mlineindex: u32, candidate: &str).
            s.state
                .webrtcbin
                .emit_by_name::<()>("add-ice-candidate", &[&mline, &candidate]);
        }
        // The host should never receive these (it sends offer / receives answer).
        SignalMessage::Offer { .. } | SignalMessage::Join { .. } => {
            tracing::warn!("host received unexpected message type: {msg:?}");
        }
    }
}

/// Execute one control-channel command (already marshalled off the GLib thread).
async fn handle_command(
    session: &mut Option<Session>,
    signal_tx: &mpsc::UnboundedSender<SignalMessage>,
    probed: &Probed,
    ice_servers: &[IceServerCfg],
    cmd_tx: &mpsc::UnboundedSender<HostCmd>,
    cmd: HostCmd,
) {
    // Same consent gate as input/clipboard/files: an unapproved peer may watch
    // (consent covers the initial screen) but not steer the host's capture/encode.
    if !crate::input::input_allowed() {
        crate::control::send(&crate::control::ToClient::Error {
            message: "consent not granted on host".into(),
        });
        return;
    }
    match cmd {
        HostCmd::SwitchMonitor(index) => {
            if session.is_none() {
                tracing::warn!("switch-monitor with no live session; ignoring");
                return;
            }
            // Validate WITHOUT mutating the live selection: until the old session is
            // torn down it keeps injecting input, and its coordinate mapping must
            // stay on the monitor the client is still seeing.
            let count = crate::monitors::all().len();
            if index >= count {
                crate::control::send(&crate::control::ToClient::Error {
                    message: format!("monitor index {index} out of range ({count} monitor(s) detected)"),
                });
                return;
            }
            let prev = crate::monitors::current_index();
            tracing::info!("client requested monitor switch [{prev}] -> [{index}]; rebuilding session");
            crate::audit::log_event("monitor_switch", &[("index", &index.to_string())]);
            // Tell the client to reset + await the fresh offer, then give the message
            // a moment to flush before the channel dies with the old session. (Belt-
            // and-braces: if it is lost, the client still detects the rebuild by the
            // new offer's changed DTLS fingerprint.)
            crate::control::send(&crate::control::ToClient::Restart {
                reason: "monitor-switch".into(),
            });
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            // Retarget only AFTER the old session (and its input path) is dead, so
            // capture and input mapping flip together.
            teardown_session(session);
            let switched = crate::monitors::select_index(index).is_ok()
                && start_session(session, signal_tx, probed, ice_servers, cmd_tx, true);
            if !switched {
                // One bad display must not kill the whole connection: restore the
                // previous monitor and rebuild there, so the client still gets an
                // offer instead of waiting forever.
                tracing::warn!("monitor switch to [{index}] failed; rolling back to [{prev}]");
                let _ = crate::monitors::select_index(prev);
                if !start_session(session, signal_tx, probed, ice_servers, cmd_tx, true) {
                    tracing::error!("rollback rebuild failed too; waiting for the client to rejoin");
                }
            }
        }
        HostCmd::SetBitrate(kbps) => match session.as_mut() {
            Some(s) => {
                let Some(venc) = s.venc.clone() else {
                    tracing::warn!("set-bitrate: no encoder element; ignoring");
                    return;
                };
                s.abr.set_ceiling(&venc, kbps);
            }
            None => tracing::warn!("set-bitrate with no live session; ignoring"),
        },
    }
}

/// Create one host-owned DataChannel on `webrtcbin`. `ordered` toggles in-order
/// delivery; `max_retransmits` (e.g. `Some(0)` for the unreliable "input" channel)
/// is set only when given. Logs and returns `None` if webrtcbin refuses (e.g. the
/// pipeline is not yet READY) — the caller treats that as a non-fatal skip.
fn create_channel(
    webrtcbin: &gst::Element,
    label: &str,
    ordered: bool,
    max_retransmits: Option<i32>,
) -> Option<glib::Object> {
    let mut builder = gst::Structure::builder("application/data-channel").field("ordered", ordered);
    if let Some(mr) = max_retransmits {
        builder = builder.field("max-retransmits", mr);
    }
    let opts = builder.build();
    let dc =
        webrtcbin.emit_by_name::<Option<glib::Object>>("create-data-channel", &[&label, &opts]);
    match &dc {
        Some(_) => tracing::info!(
            "Created DataChannel '{label}' (ordered={ordered}{})",
            match max_retransmits {
                Some(mr) => format!(", max-retransmits={mr}"),
                None => String::new(),
            }
        ),
        None => tracing::error!(
            "create-data-channel('{label}') returned None; is the pipeline at least READY?"
        ),
    }
    dc
}

/// Build one session's streaming pipeline and wire all webrtcbin signals.
fn build_pipeline(
    signal_tx: mpsc::UnboundedSender<SignalMessage>,
    probed: &Probed,
    ice_servers: &[IceServerCfg],
    cmd_tx: &mpsc::UnboundedSender<HostCmd>,
) -> Result<(gst::Pipeline, Arc<AppState>)> {
    // The capture/encode/parse/pay chain is defined in pipeline.rs so preview and stream
    // share one source-of-truth. It ends by feeding into our `webrtcbin`.
    let chain = pipeline::capture_to_rtp_chain(probed, "webrtcbin");

    let description = format!("{chain} webrtcbin name=webrtcbin");
    tracing::info!("Streaming pipeline:\n  {description}");

    let bin = gst::parse::launch(&description)
        .context("failed to construct streaming pipeline")?;
    let pipeline = bin
        .downcast::<gst::Pipeline>()
        .map_err(|_| anyhow::anyhow!("parsed streaming pipeline is not a gst::Pipeline"))?;

    let webrtcbin = pipeline
        .by_name("webrtcbin")
        .context("could not find 'webrtcbin' element in the pipeline")?;

    // --- Buffer-flow probe (black-screen diagnosis) -------------------------
    // Count encoded buffers leaving the encoder. If this logs, video IS being
    // produced and handed to webrtcbin -> a remaining black screen is then a
    // TRANSPORT or BROWSER-DECODE problem, not a capture/encode stall.
    if let Some(venc) = pipeline.by_name("venc") {
        if let Some(srcpad) = venc.static_pad("src") {
            use std::sync::atomic::{AtomicU64, Ordering};
            let counter = Arc::new(AtomicU64::new(0));
            srcpad.add_probe(gst::PadProbeType::BUFFER, move |_pad, _info| {
                let n = counter.fetch_add(1, Ordering::Relaxed) + 1;
                // Log the first buffer immediately, then once per ~2s at 30fps.
                if n == 1 || n % 60 == 0 {
                    tracing::info!("encoder output: {n} buffers so far (video IS flowing)");
                }
                gst::PadProbeReturn::Ok
            });
        } else {
            tracing::warn!("encoder 'venc' has no static src pad; cannot attach flow probe");
        }
    } else {
        tracing::warn!("encoder element 'venc' not found; cannot attach flow probe");
    }

    // --- Transport policy ---------------------------------------------------
    // max-bundle = ONE ICE transport for everything (video RTP + SCTP DataChannel).
    // With the default policy each m-line gets its own transport, and on real
    // networks the data transport can succeed while the video transport fails
    // (verified symptom: black screen + working input channel).
    // (set by string nick — version-proof across gstreamer-rs releases)
    webrtcbin.set_property_from_str("bundle-policy", "max-bundle");

    // --- STUN / TURN configuration ---
    let stun = std::env::var("STUN").unwrap_or_else(|_| DEFAULT_STUN.to_string());
    webrtcbin.set_property("stun-server", &stun);
    tracing::info!("STUN server: {stun}");

    // Optional TURN (M1b): TURN="turn://user:pass@host:3478".
    if let Ok(turn) = std::env::var("TURN") {
        if !turn.is_empty() {
            let added: bool = webrtcbin.emit_by_name("add-turn-server", &[&turn]);
            tracing::info!("TURN server (env) added: {added}");
        }
    }

    // Server-provided ICE servers (STUN + ephemeral-credential TURN), if any.
    apply_ice_servers(&webrtcbin, ice_servers);

    // --- Connection-state visibility (black-screen debugging) ---
    webrtcbin.connect_notify(Some("ice-connection-state"), |wb, _| {
        let st = wb.property_value("ice-connection-state");
        tracing::info!("ICE connection state: {st:?}");
    });
    webrtcbin.connect_notify(Some("connection-state"), |wb, _| {
        let st = wb.property_value("connection-state");
        tracing::info!("peer connection state: {st:?}");
    });

    let state = Arc::new(AppState {
        webrtcbin: webrtcbin.clone(),
        signal_tx,
    });

    // webrtcbin cannot create a data channel while the pipeline is NULL (returns None —
    // verified on-device). Bring it to READY now; PLAYING (which triggers
    // on-negotiation-needed) happens in start_session once everything is wired.
    pipeline
        .set_state(gst::State::Ready)
        .context("failed to set streaming pipeline to READY")?;

    // --- Create the host-owned DataChannels BEFORE negotiation (host = offerer). ---
    // The "input" channel is unreliable/unordered (newest-wins); the rest are reliable
    // + ordered. Each is wired by its own handler; a None return is logged, not fatal
    // (additive — a missing auxiliary channel never breaks the video/input path).
    if let Some(dc) = create_channel(&webrtcbin, "input", false, Some(0)) {
        wire_data_channel(&dc);
    }
    if crate::clipboard::enabled() {
        if let Some(dc) = create_channel(&webrtcbin, "clipboard", true, None) {
            wire_clipboard_channel(&dc);
        }
    }
    if let Some(dc) = create_channel(&webrtcbin, "control", true, None) {
        wire_control_channel(&dc, cmd_tx.clone(), probed.encoder.element.clone());
    }
    if crate::files::enabled() {
        if let Some(dc) = create_channel(&webrtcbin, "file", true, None) {
            wire_file_channel(&dc);
        }
    }

    // --- on-negotiation-needed: create the offer. ---
    {
        let state = state.clone();
        webrtcbin.connect("on-negotiation-needed", false, move |_values| {
            tracing::info!("on-negotiation-needed fired; creating offer");
            on_negotiation_needed(&state);
            None
        });
    }

    // --- on-ice-candidate: trickle local candidates to the client. ---
    {
        let state = state.clone();
        webrtcbin.connect("on-ice-candidate", false, move |values| {
            // Signal args: (webrtcbin, mlineindex: u32, candidate: String). Be
            // defensive: a malformed emission must log, not panic the GLib thread.
            let mlineindex = match values.get(1).and_then(|v| v.get::<u32>().ok()) {
                Some(m) => m,
                None => {
                    tracing::warn!("on-ice-candidate: missing/invalid mlineindex; ignoring");
                    return None;
                }
            };
            let candidate = match values.get(2).and_then(|v| v.get::<String>().ok()) {
                Some(c) => c,
                None => {
                    tracing::warn!("on-ice-candidate: missing/invalid candidate; ignoring");
                    return None;
                }
            };
            tracing::debug!("local ICE candidate (mline={mlineindex}): {candidate}");

            let msg = SignalMessage::Ice {
                candidate,
                // Per PROTOCOL.md the host emits sdpMid: null and uses sdpMLineIndex.
                sdp_mid: None,
                sdp_mline_index: Some(mlineindex),
            };
            if state.signal_tx.send(msg).is_err() {
                tracing::error!("failed to forward local ICE candidate (signaling closed)");
            }
            None
        });
    }

    // --- on-data-channel: not used by the host (host creates the channel), but log it. ---
    {
        webrtcbin.connect("on-data-channel", false, move |values| {
            if let Ok(Some(dc)) = values[1].get::<Option<glib::Object>>() {
                tracing::info!("on-data-channel (peer-initiated) received; wiring handlers");
                wire_data_channel(&dc);
            }
            None
        });
    }

    Ok((pipeline, state))
}

/// `on-negotiation-needed` handler: create-offer -> set-local-description -> send offer.
fn on_negotiation_needed(state: &Arc<AppState>) {
    let webrtcbin = state.webrtcbin.clone();
    let state_cl = state.clone();

    // create-offer(options: Option<&Structure>, promise: &Promise)
    let promise = gst::Promise::with_change_func(move |reply| {
        // The reply structure contains the created offer under key "offer".
        let reply = match reply {
            Ok(Some(reply)) => reply,
            Ok(None) => {
                tracing::error!("create-offer produced no reply");
                return;
            }
            Err(e) => {
                tracing::error!("create-offer failed: {e:?}");
                return;
            }
        };

        let offer = match reply.get::<gst_webrtc::WebRTCSessionDescription>("offer") {
            Ok(offer) => offer,
            Err(e) => {
                tracing::error!("create-offer reply missing 'offer': {e:?}");
                return;
            }
        };

        // set-local-description(desc, promise). Pass a no-op promise (fire and forget).
        webrtcbin.emit_by_name::<()>("set-local-description", &[&offer, &None::<gst::Promise>]);

        // Serialize the SDP and send {type:"offer", sdp}.
        let sdp_text = offer.sdp().as_text().unwrap_or_default();
        tracing::info!("Created offer ({} bytes); sending to client", sdp_text.len());

        let msg = SignalMessage::Offer { sdp: sdp_text };
        if state_cl.signal_tx.send(msg).is_err() {
            tracing::error!("failed to send offer (signaling closed)");
        }
    });

    state
        .webrtcbin
        .emit_by_name::<()>("create-offer", &[&None::<gst::Structure>, &promise]);
}

/// Apply the client's SDP answer as webrtcbin's remote description.
fn apply_answer(state: &Arc<AppState>, sdp: &str) -> Result<()> {
    let sdp_msg = gst_sdp::SDPMessage::parse_buffer(sdp.as_bytes())
        .context("failed to parse answer SDP")?;
    let answer = gst_webrtc::WebRTCSessionDescription::new(
        gst_webrtc::WebRTCSDPType::Answer,
        sdp_msg,
    );

    // set-remote-description(desc, promise). Fire-and-forget promise.
    state
        .webrtcbin
        .emit_by_name::<()>("set-remote-description", &[&answer, &None::<gst::Promise>]);
    tracing::info!("Applied remote answer");
    Ok(())
}

/// Apply server-issued ICE servers to webrtcbin: STUN via the `stun-server` property,
/// TURN via `add-turn-server`. This is what makes cross-NAT/CGNAT work (M1b) once a
/// coturn relay is configured on the signaling server.
fn apply_ice_servers(webrtcbin: &gst::Element, ice_servers: &[IceServerCfg]) {
    for srv in ice_servers {
        for url in &srv.urls {
            if let Some(host) = url.strip_prefix("stun:") {
                let uri = format!("stun://{host}");
                webrtcbin.set_property("stun-server", &uri);
                tracing::info!("ICE: stun-server = {uri}");
            } else if url.starts_with("turn:") || url.starts_with("turns:") {
                match (&srv.username, &srv.credential) {
                    (Some(user), Some(cred)) => {
                        let uri = to_webrtc_turn_uri(url, user, cred);
                        let added: bool = webrtcbin.emit_by_name("add-turn-server", &[&uri]);
                        tracing::info!("ICE: add-turn-server {url} (added={added})");
                    }
                    _ => tracing::warn!("TURN url {url} without credentials; skipping"),
                }
            } else {
                tracing::warn!("ignoring unrecognized ICE url: {url}");
            }
        }
    }
}

/// Rewrite a WebRTC `turn:host:port[?transport=...]` (or `turns:`) URL + credentials into
/// the `turn(s)://user:pass@host:port[?...]` form webrtcbin expects, percent-encoding the
/// user/pass (the coturn ephemeral username contains ':' and the base64 credential
/// contains '+', '/', '=', all reserved in a URI authority).
fn to_webrtc_turn_uri(url: &str, user: &str, cred: &str) -> String {
    let (scheme, rest) = if let Some(r) = url.strip_prefix("turns:") {
        ("turns", r)
    } else {
        ("turn", url.strip_prefix("turn:").unwrap_or(url))
    };
    format!("{scheme}://{}:{}@{rest}", pct(user), pct(cred))
}

/// Percent-encode everything but URI unreserved characters.
fn pct(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::{pct, to_webrtc_turn_uri};

    #[test]
    fn pct_encodes_reserved_keeps_unreserved() {
        // The ':' in the coturn username and '+','/','=' in the base64 credential
        // must be percent-encoded; unreserved chars pass through unchanged.
        assert_eq!(pct("1700000000:rcd"), "1700000000%3Arcd");
        assert_eq!(pct("ab+/=cd"), "ab%2B%2F%3Dcd");
        assert_eq!(pct("Aa0-_.~"), "Aa0-_.~");
    }

    #[test]
    fn turn_uri_embeds_encoded_credentials() {
        assert_eq!(
            to_webrtc_turn_uri("turn:nas:3478", "1700:rcd", "x+/="),
            "turn://1700%3Arcd:x%2B%2F%3D@nas:3478"
        );
        assert_eq!(
            to_webrtc_turn_uri("turns:nas:5349", "u", "p"),
            "turns://u:p@nas:5349"
        );
    }
}

/// Wire the DataChannel handlers: decode input opcodes and inject them.
///
/// We connect both "on-message-data" (binary; this is what the client sends) and
/// "on-message-string" (defensive: log unexpected text).
fn wire_data_channel(dc: &glib::Object) {
    // Binary messages: the normal path for input.
    dc.connect("on-message-data", false, move |values| {
        // Signal args: (datachannel, data: GBytes)
        let bytes = match values[1].get::<glib::Bytes>() {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!("on-message-data: could not read GBytes: {e:?}");
                return None;
            }
        };
        decode_input(&bytes);
        None
    });

    // String messages: not expected; log so we notice protocol drift.
    dc.connect("on-message-string", false, move |values| {
        if let Ok(s) = values[1].get::<String>() {
            tracing::warn!("DataChannel received unexpected text message: {s}");
        }
        None
    });

    // Lifecycle logging.
    dc.connect("on-open", false, move |_| {
        tracing::info!("DataChannel 'input' open");
        None
    });
    dc.connect("on-close", false, move |_| {
        tracing::info!("DataChannel 'input' closed");
        None
    });
    dc.connect("on-error", false, move |values| {
        tracing::warn!("DataChannel error: {:?}", values.get(1));
        None
    });
}

/// Wire the reliable "clipboard" DataChannel: inbound text -> host clipboard; register
/// the channel so the host clipboard poller can push LOCAL changes to the client.
fn wire_clipboard_channel(dc: &glib::Object) {
    dc.connect("on-message-data", false, move |values| {
        if let Ok(bytes) = values[1].get::<glib::Bytes>() {
            crate::clipboard::handle_incoming(&bytes);
        }
        None
    });

    // Register the channel for the poller on open. We pull the channel from the
    // signal's emitter arg (values[0]) instead of capturing `dc`, so the closure stays
    // Send (glib::Object is not Send, and connect() requires a Send + Sync closure).
    dc.connect("on-open", false, move |values| {
        tracing::info!("DataChannel 'clipboard' open");
        if let Ok(obj) = values[0].get::<glib::Object>() {
            crate::clipboard::set_channel(Some(obj));
        }
        None
    });
    // Identity-guarded clear: a torn-down session's channel can close AFTER the
    // rebuilt session registered its own — that stale close must not clobber it.
    dc.connect("on-close", false, move |values| {
        tracing::info!("DataChannel 'clipboard' closed");
        if let Ok(obj) = values[0].get::<glib::Object>() {
            crate::clipboard::clear_channel_if(&obj);
        }
        None
    });
}

/// Wire the reliable "control" DataChannel: JSON TEXT messages. Inbound commands are
/// marshalled to the main loop via `cmd_tx` (this callback runs on the GLib thread);
/// on open we register the channel for host->client sends and push the `hello`
/// snapshot (monitor list, encoder, capabilities).
fn wire_control_channel(
    dc: &glib::Object,
    cmd_tx: mpsc::UnboundedSender<HostCmd>,
    encoder: String,
) {
    dc.connect("on-message-string", false, move |values| {
        if let Ok(text) = values[1].get::<String>() {
            match crate::control::parse_from_client(&text) {
                Some(crate::control::FromClient::SwitchMonitor { index }) => {
                    let _ = cmd_tx.send(HostCmd::SwitchMonitor(index));
                }
                Some(crate::control::FromClient::SetBitrate { kbps }) => {
                    let _ = cmd_tx.send(HostCmd::SetBitrate(kbps));
                }
                None => {} // unknown/malformed: ignored (forward-compat)
            }
        }
        None
    });

    // Pull the channel from the emitter arg (values[0]) instead of capturing `dc`
    // so the closure stays Send (same pattern as the clipboard channel).
    dc.connect("on-open", false, move |values| {
        tracing::info!("DataChannel 'control' open");
        if let Ok(obj) = values[0].get::<glib::Object>() {
            crate::control::set_channel(Some(obj));
            crate::control::send(&crate::control::hello(&encoder));
        }
        None
    });
    // Identity-guarded clear (see the clipboard channel note).
    dc.connect("on-close", false, move |values| {
        tracing::info!("DataChannel 'control' closed");
        if let Ok(obj) = values[0].get::<glib::Object>() {
            crate::control::clear_channel_if(&obj);
        }
        None
    });
}

/// Wire the reliable "file" DataChannel: inbound frames drive the receive state
/// machine in files.rs; the registered channel is its reply path (accept/reject/done).
fn wire_file_channel(dc: &glib::Object) {
    dc.connect("on-message-data", false, move |values| {
        if let Ok(bytes) = values[1].get::<glib::Bytes>() {
            crate::files::handle_incoming(&bytes);
        }
        None
    });
    dc.connect("on-open", false, move |values| {
        tracing::info!("DataChannel 'file' open");
        if let Ok(obj) = values[0].get::<glib::Object>() {
            crate::files::set_channel(Some(obj));
        }
        None
    });
    dc.connect("on-close", false, move |values| {
        tracing::info!("DataChannel 'file' closed");
        // Abort in-flight transfers only when the CURRENT channel closed — a stale
        // close from a torn-down session must not delete the new session's .part
        // files or deregister its reply channel.
        if let Ok(obj) = values[0].get::<glib::Object>() {
            if crate::files::clear_channel_if(&obj) {
                crate::files::reset();
            }
        }
        None
    });
}

/// Decode one input message (PROTOCOL.md) and inject it via src/input.rs.
fn decode_input(bytes: &glib::Bytes) {
    let data: &[u8] = bytes;
    if data.is_empty() {
        return;
    }
    // Consent gate: drop all input until the seated user has approved (REQUIRE_CONSENT).
    if !crate::input::input_allowed() {
        return;
    }

    match data[0] {
        OPCODE_MOUSE_MOVE_ABS => {
            // Need 1 opcode byte + 2 f32 = 9 bytes total.
            if data.len() < 9 {
                tracing::warn!(
                    "MOUSE_MOVE_ABS too short: got {} bytes, need 9",
                    data.len()
                );
                return;
            }
            // Little-endian f32 at offsets 1 and 5.
            let x = f32::from_le_bytes([data[1], data[2], data[3], data[4]]);
            let y = f32::from_le_bytes([data[5], data[6], data[7], data[8]]);

            tracing::debug!("mouse move x={x:.4} y={y:.4}");
            // Inject into the real desktop (primary monitor; M1). See src/input.rs for
            // the multi-monitor / DPI TODOs.
            crate::input::mouse_move_abs(x, y);
        }
        OPCODE_MOUSE_BUTTON => {
            if data.len() < 3 {
                tracing::warn!("MOUSE_BUTTON too short: {} bytes, need 3", data.len());
                return;
            }
            tracing::debug!("mouse button {} down={}", data[1], data[2] != 0);
            crate::input::mouse_button(data[1], data[2] != 0);
        }
        OPCODE_KEY => {
            if data.len() < 4 {
                tracing::warn!("KEY too short: {} bytes, need 4", data.len());
                return;
            }
            let scan = u16::from_le_bytes([data[1], data[2]]);
            let flags = data[3];
            let down = flags & 0x01 != 0;
            let extended = flags & 0x02 != 0;
            tracing::debug!("key scan=0x{scan:02x} down={down} ext={extended}");
            crate::input::key_scan(scan, down, extended);
        }
        OPCODE_WHEEL => {
            if data.len() < 5 {
                tracing::warn!("WHEEL too short: {} bytes, need 5", data.len());
                return;
            }
            let dy = i16::from_le_bytes([data[1], data[2]]);
            let dx = i16::from_le_bytes([data[3], data[4]]);
            tracing::debug!("wheel dy={dy} dx={dx}");
            crate::input::wheel(dx, dy);
        }
        other => {
            tracing::warn!("unknown input opcode 0x{other:02x} ({} bytes)", data.len());
        }
    }
}
