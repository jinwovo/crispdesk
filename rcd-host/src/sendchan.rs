//! Shared plumbing for "the current session's outbound DataChannel".
//!
//! The clipboard, control, and file channels all need the same two things:
//!   1. store a `GstWebRTCDataChannel` handle in a `static` so host-side code on any
//!      thread (the clipboard poller, the ABR tick, file-reply paths) can send on it;
//!   2. register it on channel-open and deregister it on close — with an *identity*
//!      check so a torn-down session's late close can't clobber the channel a rebuilt
//!      session just registered.
//!
//! Previously each module hand-rolled its own `struct SendChannel(glib::Object)` with
//! its own `unsafe impl Send/Sync` (the soundness argument repeated three times) plus
//! its own `Mutex<Option<..>>` registry. This module holds that ONCE.
//!
//! ============================ SAFETY (the whole point) ========================
//! `glib::Object` (a `GstWebRTCDataChannel`) is not auto-`Send`/`Sync` in the Rust
//! binding, but GObject ref/unref is atomic and the ONLY methods we ever invoke on
//! the stored handle are the `send-data` / `send-string` action signals, which
//! GStreamer serializes internally. So moving/sharing the handle across threads and
//! calling only those signals is sound. Do NOT add methods here that touch other
//! (non-thread-safe) GObject state without revisiting this argument.
//! =============================================================================

use std::sync::Mutex;

use gstreamer::glib;
use gstreamer::prelude::*;

/// A `GstWebRTCDataChannel` handle that is safe to store/share across threads for the
/// purpose of invoking its `send-data` / `send-string` action signals (see module
/// SAFETY note).
struct SendChannel(glib::Object);
// SAFETY: see the module-level SAFETY note.
unsafe impl Send for SendChannel {}
unsafe impl Sync for SendChannel {}

/// A per-channel registry slot: holds the current session's channel handle, or none.
/// Construct one `static SLOT: ChannelSlot = ChannelSlot::new();` per channel.
pub struct ChannelSlot {
    inner: Mutex<Option<SendChannel>>,
}

impl ChannelSlot {
    pub const fn new() -> Self {
        ChannelSlot { inner: Mutex::new(None) }
    }

    /// Register (`Some`) or clear (`None`) the current session's channel.
    pub fn set(&self, dc: Option<glib::Object>) {
        if let Ok(mut cur) = self.inner.lock() {
            *cur = dc.map(SendChannel);
        }
    }

    /// Clear the registration ONLY if `dc` is the currently-registered channel;
    /// returns whether it was. Guards against a stale close from a torn-down session
    /// deregistering the channel a rebuilt session already registered.
    pub fn clear_if(&self, dc: &glib::Object) -> bool {
        if let Ok(mut cur) = self.inner.lock() {
            if cur.as_ref().map(|c| c.0 == *dc).unwrap_or(false) {
                *cur = None;
                return true;
            }
        }
        false
    }

    /// Whether a channel is currently registered (i.e. sends will reach a peer).
    pub fn has(&self) -> bool {
        self.inner.lock().map(|c| c.is_some()).unwrap_or(false)
    }

    /// Send a binary frame; returns false if no channel is registered (no-op).
    pub fn send_data(&self, bytes: glib::Bytes) -> bool {
        if let Ok(cur) = self.inner.lock() {
            if let Some(ch) = cur.as_ref() {
                ch.0.emit_by_name::<()>("send-data", &[&bytes]);
                return true;
            }
        }
        false
    }

    /// Send a text frame; returns false if no channel is registered (no-op).
    pub fn send_string(&self, s: &str) -> bool {
        if let Ok(cur) = self.inner.lock() {
            if let Some(ch) = cur.as_ref() {
                ch.0.emit_by_name::<()>("send-string", &[&s]);
                return true;
            }
        }
        false
    }
}
