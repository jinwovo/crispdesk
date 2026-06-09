//! Signaling WebSocket client (tokio-tungstenite).
//!
//! Implements the PINNED signaling protocol shared by rcd-host / rcd-signal / rcd-client.
//! Transport is WebSocket, endpoint path is EXACTLY `/ws`. Every message is a JSON object
//! with a `"type"` field. The struct/enum field names below are NORMATIVE and must match
//! the other two components verbatim (including `sdpMid` / `sdpMLineIndex`).
//!
//! This module owns the socket: it connects, sends `join` as role "host", forwards
//! outbound messages from an mpsc channel, and forwards inbound messages onto another
//! mpsc channel for `webrtc.rs` to consume.

use anyhow::{Context, Result};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;

/// Default signaling URL if `SIGNAL_URL` is unset. Path MUST be `/ws`.
pub const DEFAULT_SIGNAL_URL: &str = "ws://127.0.0.1:8080/ws";
/// Default room / pairing code if `PAIRING_CODE` is unset.
pub const DEFAULT_PAIRING_CODE: &str = "123456";

/// A peer's role in a room. Serializes to lowercase "host" / "client".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    Host,
    Client,
}

/// Every protocol message tagged by its `"type"` field.
///
/// `#[serde(tag = "type", rename_all = "kebab-case")]` makes the variant name the value
/// of the `"type"` field. Variant names map to: join, joined, peer-joined, peer-left,
/// error, offer, answer, ice. ICE field names are forced with `#[serde(rename = ...)]`
/// so `sdpMid` / `sdpMLineIndex` are emitted EXACTLY as pinned (camelCase, not kebab).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum SignalMessage {
    /// client/host -> server: join a room with a pairing code and role.
    Join {
        room: String,
        role: Role,
    },

    /// server -> peer (ack): you joined; `peers` is how many are currently in the room.
    Joined {
        role: Role,
        peers: i64,
    },

    /// server -> the OTHER peer: someone with `role` joined your room.
    #[serde(rename = "peer-joined")]
    PeerJoined {
        role: Role,
    },

    /// server -> the OTHER peer: someone with `role` left your room.
    #[serde(rename = "peer-left")]
    PeerLeft {
        role: Role,
    },

    /// server -> peer: bad code / room full / protocol error.
    Error {
        message: String,
    },

    /// host -> client (relayed): the SDP offer (full SDP string).
    Offer {
        sdp: String,
    },

    /// client -> host (relayed): the SDP answer (full SDP string).
    Answer {
        sdp: String,
    },

    /// either -> other (relayed): a trickled ICE candidate.
    ///
    /// NOTE: `sdpMid` and `sdpMLineIndex` are nullable per the protocol. The host emits
    /// candidates with `sdpMid: null` and a populated `sdpMLineIndex` (see webrtc.rs).
    Ice {
        candidate: String,
        #[serde(rename = "sdpMid")]
        sdp_mid: Option<String>,
        #[serde(rename = "sdpMLineIndex")]
        sdp_mline_index: Option<u32>,
    },
}

/// Handle returned by [`connect`]: send outbound messages, receive inbound messages.
pub struct Signaling {
    /// Push messages here to send them to the server (e.g. offer, ice).
    pub outbound: mpsc::UnboundedSender<SignalMessage>,
    /// Pull inbound messages from here (e.g. peer-joined, answer, ice).
    pub inbound: mpsc::UnboundedReceiver<SignalMessage>,
}

/// Resolve the signaling URL and pairing code from the environment.
pub fn config_from_env() -> (String, String) {
    let url = std::env::var("SIGNAL_URL").unwrap_or_else(|_| DEFAULT_SIGNAL_URL.to_string());
    let code = std::env::var("PAIRING_CODE").unwrap_or_else(|_| DEFAULT_PAIRING_CODE.to_string());
    (url, code)
}

/// Connect to the signaling server, send `join` as role "host" with `pairing_code`,
/// and spawn a background task that pumps the socket in both directions.
///
/// Returns a [`Signaling`] handle for the rest of the app to talk through.
pub async fn connect(signal_url: &str, pairing_code: &str) -> Result<Signaling> {
    tracing::info!("Connecting to signaling server at {signal_url}");

    let (ws_stream, _resp) = tokio_tungstenite::connect_async(signal_url)
        .await
        .with_context(|| format!("failed to connect to signaling server at {signal_url}"))?;

    let (mut ws_sink, mut ws_source) = ws_stream.split();

    // Outbound: app -> server.
    let (out_tx, mut out_rx) = mpsc::unbounded_channel::<SignalMessage>();
    // Inbound: server -> app.
    let (in_tx, in_rx) = mpsc::unbounded_channel::<SignalMessage>();

    // Immediately announce ourselves as the host in the pairing room.
    let join = SignalMessage::Join {
        room: pairing_code.to_string(),
        role: Role::Host,
    };
    let join_json = serde_json::to_string(&join).context("failed to serialize join message")?;
    ws_sink
        .send(WsMessage::Text(join_json.into()))
        .await
        .context("failed to send join message")?;
    tracing::info!("Sent join (room={pairing_code}, role=host)");

    // Outbound pump: drain the mpsc channel onto the websocket, and send a periodic
    // keepalive Ping so idle NAT mappings / proxies (e.g. a self-hosted NAS behind a
    // home router) don't silently drop a long-lived connection.
    tokio::spawn(async move {
        let mut keepalive = tokio::time::interval(std::time::Duration::from_secs(20));
        keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        keepalive.tick().await; // consume the immediate first tick
        loop {
            tokio::select! {
                maybe_msg = out_rx.recv() => {
                    let Some(msg) = maybe_msg else { break };
                    match serde_json::to_string(&msg) {
                        Ok(json) => {
                            tracing::debug!("-> signaling: {json}");
                            if let Err(e) = ws_sink.send(WsMessage::Text(json.into())).await {
                                tracing::error!("signaling send failed: {e}");
                                break;
                            }
                        }
                        Err(e) => {
                            tracing::error!("failed to serialize outbound signaling message: {e}")
                        }
                    }
                }
                _ = keepalive.tick() => {
                    if let Err(e) = ws_sink.send(WsMessage::Ping(Vec::new().into())).await {
                        tracing::warn!("signaling keepalive ping failed: {e}");
                        break;
                    }
                }
            }
        }
        tracing::debug!("signaling outbound pump finished");
    });

    // Inbound pump: parse websocket frames and forward typed messages to the app.
    tokio::spawn(async move {
        while let Some(frame) = ws_source.next().await {
            match frame {
                Ok(WsMessage::Text(text)) => {
                    tracing::debug!("<- signaling: {text}");
                    match serde_json::from_str::<SignalMessage>(&text) {
                        Ok(msg) => {
                            if in_tx.send(msg).is_err() {
                                // Receiver dropped; nothing left to do.
                                break;
                            }
                        }
                        Err(e) => {
                            tracing::warn!("ignoring unparseable signaling message: {e}: {text}");
                        }
                    }
                }
                Ok(WsMessage::Close(frame)) => {
                    tracing::warn!("signaling server closed the connection: {frame:?}");
                    break;
                }
                Ok(WsMessage::Ping(_)) | Ok(WsMessage::Pong(_)) => {
                    // tungstenite auto-responds to pings; nothing to do.
                }
                Ok(other) => {
                    tracing::debug!("ignoring non-text signaling frame: {other:?}");
                }
                Err(e) => {
                    tracing::error!("signaling receive error: {e}");
                    break;
                }
            }
        }
        tracing::debug!("signaling inbound pump finished");
    });

    Ok(Signaling {
        outbound: out_tx,
        inbound: in_rx,
    })
}
