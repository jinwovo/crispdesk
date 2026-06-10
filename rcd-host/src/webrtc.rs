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
use crate::signaling::{self, Role, SignalMessage};

/// Default STUN server (matches the client). Note webrtcbin wants the `stun://` scheme.
const DEFAULT_STUN: &str = "stun://stun.l.google.com:19302";

/// Input opcodes (PROTOCOL.md). All multi-byte fields are little-endian.
const OPCODE_MOUSE_MOVE_ABS: u8 = 0x01; // [f32 x][f32 y] normalized 0..1
const OPCODE_MOUSE_BUTTON: u8 = 0x02; //   [u8 button 0=L 1=R 2=M 3=X1 4=X2][u8 down(1)/up(0)]
const OPCODE_KEY: u8 = 0x03; //            [u16 scancode][u8 flags bit0=down bit1=extended]
const OPCODE_WHEEL: u8 = 0x04; //          [i16 wheelY][i16 wheelX] WHEEL_DELTA units (+120/notch)
// TODO(milestone): 0x05 GAMEPAD (ViGEmBus), clipboard channel, etc.

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
                    handle_signal(&mut session, &sig.outbound, &probed, msg);
                }

                _ = abr_tick.tick() => {
                    if let Some(s) = session.as_mut() {
                        // Clone the (refcounted) elements so the &mut borrow of `s.abr`
                        // doesn't conflict with the &borrows of webrtcbin/venc.
                        let wb = s.state.webrtcbin.clone();
                        if let Some(venc) = s.venc.clone() {
                            s.abr.tick(&wb, &venc);
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
        // Drop the clipboard channel handle so the poller stops sending into a dead session.
        crate::clipboard::set_channel(None);
        tracing::info!("session torn down");
    }
}

/// Build a FRESH session for a newly-present client and start negotiating.
fn start_session(
    session: &mut Option<Session>,
    signal_tx: &mpsc::UnboundedSender<SignalMessage>,
    probed: &Probed,
) {
    // A previous session (e.g. the client refreshed the page) cannot be reused.
    teardown_session(session);

    let (pipeline, state) = match build_pipeline(signal_tx.clone(), probed) {
        Ok(ps) => ps,
        Err(e) => {
            tracing::error!("failed to build session pipeline: {e:?}");
            return;
        }
    };

    // Log pipeline errors/EOS for this session via a bus watch on the shared loop.
    let bus = match pipeline.bus() {
        Some(b) => b,
        None => {
            tracing::error!("session pipeline has no bus");
            return;
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
            return;
        }
    };

    tracing::info!("Client present -> starting a fresh WebRTC session (PLAYING)");
    if let Err(e) = pipeline.set_state(gst::State::Playing) {
        tracing::error!("failed to set session pipeline to PLAYING: {e}");
        return;
    }

    // Adaptive bitrate retunes this session's encoder; grab it by the name set in
    // pipeline.rs ("venc"). Absent only if the pipeline shape changed.
    let venc = pipeline.by_name("venc");
    let abr = crate::abr::AbrState::new(probed.encoder.element.clone());

    *session = Some(Session {
        pipeline,
        state,
        venc,
        abr,
        _bus_watch: watch,
    });
}

/// React to one inbound signaling message.
fn handle_signal(
    session: &mut Option<Session>,
    signal_tx: &mpsc::UnboundedSender<SignalMessage>,
    probed: &Probed,
    msg: SignalMessage,
) {
    match msg {
        // Our own join ack. If the room already has 2 peers, the client is present.
        SignalMessage::Joined { role, peers } => {
            tracing::info!("joined room as {role:?}; peers in room = {peers}");
            if peers >= 2 {
                start_session(session, signal_tx, probed);
            }
        }
        // The other peer joined (or rejoined after a refresh): start a fresh session.
        SignalMessage::PeerJoined { role } => {
            tracing::info!("peer joined: {role:?}");
            if role == Role::Client {
                start_session(session, signal_tx, probed);
            }
        }
        SignalMessage::PeerLeft { role } => {
            tracing::warn!("peer left: {role:?}");
            teardown_session(session);
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

/// Build one session's streaming pipeline and wire all webrtcbin signals.
fn build_pipeline(
    signal_tx: mpsc::UnboundedSender<SignalMessage>,
    probed: &Probed,
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
            tracing::info!("TURN server '{turn}' added: {added}");
        }
    }

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

    // --- Create the "input" DataChannel BEFORE negotiation (host = offerer owns it). ---
    {
        let dc_opts = gst::Structure::builder("application/data-channel")
            .field("ordered", false)
            .field("max-retransmits", 0i32)
            .build();
        let data_channel = state.webrtcbin.emit_by_name::<Option<glib::Object>>(
            "create-data-channel",
            &[&"input", &dc_opts],
        );

        match data_channel {
            Some(dc) => {
                tracing::info!("Created DataChannel 'input' (ordered=false, max-retransmits=0)");
                wire_data_channel(&dc);
            }
            None => {
                tracing::error!(
                    "create-data-channel returned None; input will not work. \
                     (Is the pipeline at least READY?)"
                );
            }
        }
    }

    // --- Create the RELIABLE "clipboard" DataChannel (ordered, no retransmit limit). ---
    // Unlike "input" (unreliable/newest-wins), clipboard text must arrive intact and in
    // order. Gated by CLIPBOARD env (clipboard::enabled).
    if crate::clipboard::enabled() {
        let dc_opts = gst::Structure::builder("application/data-channel")
            .field("ordered", true)
            .build();
        let clip = state.webrtcbin.emit_by_name::<Option<glib::Object>>(
            "create-data-channel",
            &[&"clipboard", &dc_opts],
        );
        match clip {
            Some(dc) => {
                tracing::info!("Created DataChannel 'clipboard' (ordered, reliable)");
                wire_clipboard_channel(&dc);
            }
            None => tracing::error!("create-data-channel('clipboard') returned None"),
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
    dc.connect("on-close", false, move |_| {
        tracing::info!("DataChannel 'clipboard' closed");
        crate::clipboard::set_channel(None);
        None
    });
}

/// Decode one input message (PROTOCOL.md) and inject it via src/input.rs.
fn decode_input(bytes: &glib::Bytes) {
    let data: &[u8] = bytes;
    if data.is_empty() {
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
