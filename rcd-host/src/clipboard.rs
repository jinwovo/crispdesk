//! Host clipboard sync over the reliable, ordered "clipboard" DataChannel.
//!
//! Bidirectional TEXT clipboard between the client and host (PROTOCOL.md opcode 0x06):
//!   * host -> client: a background poller watches the Windows clipboard and, on a
//!     LOCAL change, sends `CLIPBOARD_TEXT` to the current session's channel.
//!   * client -> host: inbound `CLIPBOARD_TEXT` is written to the Windows clipboard.
//!
//! ECHO-LOOP PREVENTION: a single `LAST_SYNCED` string holds the text most recently
//! sent OR received. The poller ignores a clipboard value equal to it (so text we just
//! wrote from a remote message is never bounced back), and a remote write updates it
//! first. Last-write-wins; no SHA needed for text equality.
//!
//! The clipboard get/set uses the `clipboard-win` crate (wraps the fiddly Win32
//! OpenClipboard/Global* dance and is callable from any thread).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use gstreamer::glib;
use gstreamer::prelude::*;

/// PROTOCOL.md opcode 0x06 CLIPBOARD_TEXT: [u8 0x06][u32 len LE][utf8 text].
const OPCODE_CLIPBOARD_TEXT: u8 = 0x06;

/// The last clipboard text we synced in EITHER direction (echo-loop guard).
static LAST_SYNCED: LazyLock<Mutex<Option<String>>> = LazyLock::new(|| Mutex::new(None));

/// The current session's "clipboard" DataChannel (host -> client send path).
static CURRENT_CLIP: LazyLock<Mutex<Option<SendChannel>>> = LazyLock::new(|| Mutex::new(None));

/// Ensures the poller thread is only spawned once for the process lifetime.
static POLLER_STARTED: AtomicBool = AtomicBool::new(false);

/// A DataChannel handle we can store across threads. GstWebRTCDataChannel's `send-data`
/// is internally thread-safe and GObject ref/unref is atomic, so sending from the poller
/// thread is sound; the Rust binding just isn't auto-`Send`, hence this asserting wrapper.
struct SendChannel(glib::Object);
// SAFETY: see the struct doc — only `send-data` is invoked, which GStreamer serializes.
unsafe impl Send for SendChannel {}
unsafe impl Sync for SendChannel {}

impl SendChannel {
    fn send(&self, bytes: &glib::Bytes) {
        self.0.emit_by_name::<()>("send-data", &[bytes]);
    }
}

/// Clipboard sync is on unless `CLIPBOARD=0`.
pub fn enabled() -> bool {
    std::env::var("CLIPBOARD").as_deref() != Ok("0")
}

fn poll_ms() -> u64 {
    std::env::var("CLIPBOARD_POLL_MS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(500)
}

fn max_bytes() -> usize {
    std::env::var("CLIPBOARD_MAX_BYTES")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(102_400)
}

/// Register (or clear with `None`) the current session's clipboard channel.
pub fn set_channel(dc: Option<glib::Object>) {
    if let Ok(mut cur) = CURRENT_CLIP.lock() {
        *cur = dc.map(SendChannel);
    }
}

/// Read the Windows clipboard text (None if empty / non-text / unavailable).
fn get() -> Option<String> {
    clipboard_win::get_clipboard_string().ok()
}

/// Apply remote clipboard text to the Windows clipboard, recording it as synced so the
/// poller does not echo it back.
fn set_from_remote(text: &str) {
    if let Ok(mut last) = LAST_SYNCED.lock() {
        *last = Some(text.to_string());
    }
    if let Err(e) = clipboard_win::set_clipboard_string(text) {
        tracing::warn!("failed to set host clipboard: {e:?}");
    } else {
        tracing::debug!("clipboard <- client ({} bytes)", text.len());
    }
}

/// Decode + apply an inbound CLIPBOARD_TEXT frame (called from the DataChannel handler).
pub fn handle_incoming(data: &[u8]) {
    // Consent gate: a non-consented peer must not alter the host clipboard either
    // (same gate as injected input; see input::request_consent / REQUIRE_CONSENT).
    if !crate::input::input_allowed() {
        return;
    }
    match decode_frame(data) {
        Some(text) => set_from_remote(text),
        None => tracing::warn!("clipboard: malformed/invalid frame ({} bytes)", data.len()),
    }
}

/// Parse a CLIPBOARD_TEXT frame `[0x06][u32 len LE][utf8]` into its text, or None if
/// the opcode is wrong, the header is short, the body is truncated, or it isn't UTF-8.
fn decode_frame(data: &[u8]) -> Option<&str> {
    if data.first() != Some(&OPCODE_CLIPBOARD_TEXT) || data.len() < 5 {
        return None;
    }
    let len = u32::from_le_bytes([data[1], data[2], data[3], data[4]]) as usize;
    let end = 5usize.checked_add(len)?;
    if data.len() < end {
        return None;
    }
    std::str::from_utf8(&data[5..end]).ok()
}

/// Encode a CLIPBOARD_TEXT frame.
fn encode(text: &str) -> Vec<u8> {
    let bytes = text.as_bytes();
    let mut frame = Vec::with_capacity(5 + bytes.len());
    frame.push(OPCODE_CLIPBOARD_TEXT);
    frame.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    frame.extend_from_slice(bytes);
    frame
}

/// Spawn the clipboard poller once. It watches the Windows clipboard and forwards LOCAL
/// changes to whichever session channel is currently registered.
pub fn start_poller() {
    if !enabled() {
        tracing::info!("CLIPBOARD=0 -> clipboard sync disabled");
        return;
    }
    if POLLER_STARTED.swap(true, Ordering::SeqCst) {
        return; // already running
    }
    tracing::info!("clipboard sync ON (poll {}ms, max {}B)", poll_ms(), max_bytes());

    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_millis(poll_ms()));

        let Some(cur) = get() else { continue };

        // Genuine local change? (differs from the last text we synced either way)
        let changed = match LAST_SYNCED.lock() {
            Ok(mut last) => {
                if last.as_deref() == Some(cur.as_str()) {
                    false
                } else {
                    *last = Some(cur.clone());
                    true
                }
            }
            Err(_) => false,
        };
        if !changed {
            continue;
        }
        if cur.len() > max_bytes() {
            tracing::warn!("local clipboard {} bytes exceeds max; not syncing", cur.len());
            continue;
        }

        let frame = encode(&cur);
        if let Ok(guard) = CURRENT_CLIP.lock() {
            if let Some(ch) = guard.as_ref() {
                ch.send(&glib::Bytes::from_owned(frame));
                tracing::debug!("clipboard -> client ({} bytes)", cur.len());
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{decode_frame, encode, OPCODE_CLIPBOARD_TEXT};

    #[test]
    fn encode_decode_roundtrip() {
        let big = "x".repeat(5000);
        for s in ["", "hello", "한글 🎉 émoji\nline2", big.as_str()] {
            let frame = encode(s);
            assert_eq!(frame[0], OPCODE_CLIPBOARD_TEXT);
            assert_eq!(decode_frame(&frame), Some(s));
        }
    }

    #[test]
    fn decode_rejects_malformed() {
        assert_eq!(decode_frame(&[]), None); // empty
        assert_eq!(decode_frame(&[0x06, 0, 0]), None); // header too short
        assert_eq!(decode_frame(&[0x01, 0, 0, 0, 0]), None); // wrong opcode
        assert_eq!(decode_frame(&[0x06, 10, 0, 0, 0, b'a', b'b']), None); // truncated body
    }
}
