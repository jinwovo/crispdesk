//! The reliable "control" DataChannel: JSON TEXT messages (unlike the binary
//! input/clipboard/file channels) for session control + telemetry.
//!
//! Direction summary (PROTOCOL.md §2.5):
//!   * host -> client: `hello` (monitors / encoder / capabilities, on channel open),
//!     `stats` (~1 Hz encoder bitrate + RTCP loss/RTT), `restart` (the host is about
//!     to rebuild the WebRTC session — reset and await a fresh offer), `error`.
//!   * client -> host: `switch-monitor` (capture+control another display),
//!     `set-bitrate` (move the encoder/ABR ceiling live; 0 = back to default).
//!
//! Field names are camelCase and `type` values kebab-case, matching the signaling
//! protocol's conventions. UNKNOWN message types are ignored (forward-compat) —
//! never treat them as an error.
//!
//! Threading mirrors clipboard.rs: the channel handle is registered on open and
//! stored globally; `send-string` on a GstWebRTCDataChannel is internally
//! thread-safe, so host-side code can push messages from any thread.

use gstreamer::glib;
use serde::{Deserialize, Serialize};

use crate::sendchan::ChannelSlot;

/// One monitor entry in `hello` (index matches `MONITOR=` / `switch-monitor`).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MonitorInfo {
    pub index: usize,
    pub width: i32,
    pub height: i32,
    pub left: i32,
    pub top: i32,
    pub primary: bool,
    /// True for the monitor currently being captured.
    pub current: bool,
}

/// Bitrate config snapshot reported in `hello`.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AbrInfo {
    pub floor_kbps: u32,
    pub ceiling_kbps: u32,
    /// False when the host pins a fixed bitrate (`ABR=0`).
    pub adaptive: bool,
}

/// host -> client control messages.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum ToClient {
    #[serde(rename_all = "camelCase")]
    Hello {
        monitors: Vec<MonitorInfo>,
        /// The GStreamer encoder element in use (e.g. "mfh264enc").
        encoder: String,
        /// Whether the host accepts file transfers (the "file" channel exists).
        file_transfer: bool,
        abr: AbrInfo,
    },
    #[serde(rename_all = "camelCase")]
    Stats {
        encoder_kbps: u32,
        loss_pct: f64,
        rtt_ms: f64,
    },
    /// The host is about to tear down + rebuild the WebRTC session (e.g. a monitor
    /// switch). The client should reset its peer connection and await a new offer;
    /// the signaling socket stays up throughout.
    Restart { reason: String },
    /// A rejected control request (bad monitor index, consent not granted, ...).
    Error { message: String },
}

/// client -> host control messages.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum FromClient {
    SwitchMonitor { index: usize },
    /// kbps == 0 restores the host's configured default ceiling.
    SetBitrate { kbps: u32 },
}

/// Parse one inbound control payload. `None` = malformed OR an unknown type —
/// both are ignored by the caller (forward-compat), with a debug log here.
pub fn parse_from_client(text: &str) -> Option<FromClient> {
    match serde_json::from_str::<FromClient>(text) {
        Ok(msg) => Some(msg),
        Err(e) => {
            tracing::debug!("control: ignoring unparseable/unknown message ({e}): {text}");
            None
        }
    }
}

/// The current session's "control" DataChannel (host -> client send path).
static CURRENT: ChannelSlot = ChannelSlot::new();

/// Register (or clear with `None`) the current session's control channel.
pub fn set_channel(dc: Option<glib::Object>) {
    CURRENT.set(dc);
}

/// Clear the registration only if `dc` IS the currently-registered channel; returns
/// whether it was. An old session's channel can close asynchronously AFTER the new
/// session registered its own — that stale close must not clobber the live handle.
pub fn clear_channel_if(dc: &glib::Object) -> bool {
    CURRENT.clear_if(dc)
}

/// Whether a control channel is registered (i.e. someone consumes `stats`).
pub fn has_channel() -> bool {
    CURRENT.has()
}

/// Serialize + send one message to the client. Returns false when no channel is
/// registered (no client / channel not open yet) — callers treat that as a no-op.
pub fn send(msg: &ToClient) -> bool {
    let json = match serde_json::to_string(msg) {
        Ok(j) => j,
        Err(e) => {
            tracing::warn!("control: failed to serialize {msg:?}: {e}");
            return false;
        }
    };
    CURRENT.send_string(&json)
}

/// Build the `hello` sent when the control channel opens: the monitor list (current
/// one flagged), the encoder in use, and capability/config snapshots.
pub fn hello(encoder: &str) -> ToClient {
    let current = crate::monitors::current_index();
    let monitors = crate::monitors::all()
        .iter()
        .enumerate()
        .map(|(i, m)| MonitorInfo {
            index: i,
            width: m.width,
            height: m.height,
            left: m.left,
            top: m.top,
            primary: m.primary,
            current: i == current,
        })
        .collect();
    let (floor_kbps, ceiling_kbps, adaptive) = crate::abr::current_config();
    ToClient::Hello {
        monitors,
        encoder: encoder.to_string(),
        file_transfer: crate::files::enabled(),
        abr: AbrInfo { floor_kbps, ceiling_kbps, adaptive },
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_from_client, AbrInfo, FromClient, MonitorInfo, ToClient};

    #[test]
    fn to_client_json_shapes_are_pinned() {
        // These serialized shapes are the wire contract with rcd-client's
        // parseControlMessage — kebab-case types, camelCase fields.
        let hello = ToClient::Hello {
            monitors: vec![MonitorInfo {
                index: 0,
                width: 2880,
                height: 1800,
                left: 0,
                top: 0,
                primary: true,
                current: true,
            }],
            encoder: "mfh264enc".into(),
            file_transfer: true,
            abr: AbrInfo { floor_kbps: 1500, ceiling_kbps: 12000, adaptive: true },
        };
        let v: serde_json::Value = serde_json::from_str(&serde_json::to_string(&hello).unwrap()).unwrap();
        assert_eq!(v["type"], "hello");
        assert_eq!(v["monitors"][0]["index"], 0);
        assert_eq!(v["monitors"][0]["primary"], true);
        assert_eq!(v["monitors"][0]["current"], true);
        assert_eq!(v["encoder"], "mfh264enc");
        assert_eq!(v["fileTransfer"], true);
        assert_eq!(v["abr"]["floorKbps"], 1500);
        assert_eq!(v["abr"]["ceilingKbps"], 12000);
        assert_eq!(v["abr"]["adaptive"], true);

        let stats = ToClient::Stats { encoder_kbps: 8500, loss_pct: 1.25, rtt_ms: 12.0 };
        let v: serde_json::Value = serde_json::from_str(&serde_json::to_string(&stats).unwrap()).unwrap();
        assert_eq!(v["type"], "stats");
        assert_eq!(v["encoderKbps"], 8500);
        assert_eq!(v["lossPct"], 1.25);
        assert_eq!(v["rttMs"], 12.0);

        let restart = ToClient::Restart { reason: "monitor-switch".into() };
        let v: serde_json::Value = serde_json::from_str(&serde_json::to_string(&restart).unwrap()).unwrap();
        assert_eq!(v["type"], "restart");
        assert_eq!(v["reason"], "monitor-switch");
    }

    #[test]
    fn from_client_parses_known_and_ignores_unknown() {
        assert_eq!(
            parse_from_client(r#"{"type":"switch-monitor","index":1}"#),
            Some(FromClient::SwitchMonitor { index: 1 })
        );
        assert_eq!(
            parse_from_client(r#"{"type":"set-bitrate","kbps":8000}"#),
            Some(FromClient::SetBitrate { kbps: 8000 })
        );
        // Unknown type / malformed JSON / wrong field types -> None, never a panic.
        assert_eq!(parse_from_client(r#"{"type":"future-thing","x":1}"#), None);
        assert_eq!(parse_from_client("not json"), None);
        assert_eq!(parse_from_client(r#"{"type":"switch-monitor","index":"one"}"#), None);
    }
}
