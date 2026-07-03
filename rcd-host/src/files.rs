//! File transfer over the reliable, ordered "file" DataChannel (PROTOCOL.md §2.6).
//!
//! M1 scope: CLIENT -> HOST uploads (the client drags files onto its window; the
//! host saves them under the Downloads folder). The frame format is direction-
//! agnostic so host -> client can be added later without a protocol break.
//!
//! Flow (all frames binary, opcode-tagged, little-endian):
//!   client:  FILE_OFFER {id, size, name}
//!   host:    FILE_ACCEPT {id}                    (or FILE_REJECT {id, reason})
//!   client:  FILE_CHUNK {id, bytes} ... repeated (ordered channel -> implicit offsets)
//!   client:  FILE_DONE {id}                      (all bytes sent)
//!   host:    FILE_DONE {id}                      (ack: verified + saved)
//!
//! Safety: the offered name is SANITIZED (last path component, Windows-invalid
//! chars replaced, reserved device names defused) so a hostile client cannot
//! traverse paths; size is capped (`FILE_MAX_BYTES`); receipt is gated by the same
//! consent gate as input/clipboard (`REQUIRE_CONSENT`); data lands in a `.part`
//! temp file that is renamed only after the byte count verifies.
//!
//! Env: `FILES=0` disables the channel entirely; `FILE_DIR` overrides the save
//! directory (default `%USERPROFILE%\Downloads`); `FILE_MAX_BYTES` caps one file.

use std::collections::HashMap;
use std::fs;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex};

use gstreamer::glib;

use crate::sendchan::ChannelSlot;

pub const OP_FILE_OFFER: u8 = 0x10;
pub const OP_FILE_ACCEPT: u8 = 0x11;
pub const OP_FILE_REJECT: u8 = 0x12;
pub const OP_FILE_CHUNK: u8 = 0x13;
pub const OP_FILE_DONE: u8 = 0x14;

/// Never hold more than this many transfers in flight (the client sends
/// sequentially; this bounds a misbehaving peer).
const MAX_CONCURRENT: usize = 8;

/// File transfer is on unless `FILES=0`.
pub fn enabled() -> bool {
    crate::env::on("FILES")
}

fn max_bytes() -> u64 {
    crate::env::parse_or("FILE_MAX_BYTES", 2 * 1024 * 1024 * 1024) // 2 GiB
}

/// Where received files land: `FILE_DIR`, else `%USERPROFILE%\Downloads`, else temp.
fn save_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("FILE_DIR") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    if let Ok(profile) = std::env::var("USERPROFILE") {
        return PathBuf::from(profile).join("Downloads");
    }
    std::env::temp_dir()
}

// ---------------------------------------------------------------------------
// Wire codec (pure; unit-tested below).
// ---------------------------------------------------------------------------

/// One decoded file-channel frame (zero-copy views into the input).
#[derive(Debug, PartialEq, Eq)]
pub enum Frame<'a> {
    Offer { id: u32, size: u64, name: &'a str },
    Accept { id: u32 },
    Reject { id: u32, reason: &'a str },
    Chunk { id: u32, data: &'a [u8] },
    Done { id: u32 },
}

fn read_u32(data: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(data.get(at..at + 4)?.try_into().ok()?))
}

/// Decode one frame; `None` for anything malformed or unknown (ignored by callers).
pub fn decode(data: &[u8]) -> Option<Frame<'_>> {
    let op = *data.first()?;
    let id = read_u32(data, 1)?;
    match op {
        OP_FILE_OFFER => {
            // [u8][u32 id][u64 size][u16 nameLen][utf8 name]
            let size = u64::from_le_bytes(data.get(5..13)?.try_into().ok()?);
            let name_len = u16::from_le_bytes(data.get(13..15)?.try_into().ok()?) as usize;
            let end = 15usize.checked_add(name_len)?;
            if data.len() < end {
                return None;
            }
            let name = std::str::from_utf8(&data[15..end]).ok()?;
            Some(Frame::Offer { id, size, name })
        }
        OP_FILE_ACCEPT => Some(Frame::Accept { id }),
        OP_FILE_REJECT => {
            // [u8][u32 id][u16 len][utf8 reason]
            let len = u16::from_le_bytes(data.get(5..7)?.try_into().ok()?) as usize;
            let end = 7usize.checked_add(len)?;
            if data.len() < end {
                return None;
            }
            let reason = std::str::from_utf8(&data[7..end]).ok()?;
            Some(Frame::Reject { id, reason })
        }
        OP_FILE_CHUNK => Some(Frame::Chunk { id, data: &data[5..] }),
        OP_FILE_DONE => Some(Frame::Done { id }),
        _ => None,
    }
}

pub fn encode_accept(id: u32) -> Vec<u8> {
    let mut f = Vec::with_capacity(5);
    f.push(OP_FILE_ACCEPT);
    f.extend_from_slice(&id.to_le_bytes());
    f
}

pub fn encode_done(id: u32) -> Vec<u8> {
    let mut f = Vec::with_capacity(5);
    f.push(OP_FILE_DONE);
    f.extend_from_slice(&id.to_le_bytes());
    f
}

pub fn encode_reject(id: u32, reason: &str) -> Vec<u8> {
    let bytes = reason.as_bytes();
    let len = bytes.len().min(u16::MAX as usize);
    let mut f = Vec::with_capacity(7 + len);
    f.push(OP_FILE_REJECT);
    f.extend_from_slice(&id.to_le_bytes());
    f.extend_from_slice(&(len as u16).to_le_bytes());
    f.extend_from_slice(&bytes[..len]);
    f
}

// ---------------------------------------------------------------------------
// Filename hygiene (pure; unit-tested below).
// ---------------------------------------------------------------------------

/// Reduce a client-supplied name to a safe Windows basename: keep only the last
/// path component, replace invalid/control characters, trim trailing dots/spaces,
/// defuse reserved device names, and never return an empty string.
pub fn sanitize_filename(raw: &str) -> String {
    // Last path component only — defeats "..\..\evil.exe" and absolute paths.
    let base = raw.rsplit(['/', '\\']).next().unwrap_or(raw);

    let mut out: String = base
        .chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();

    // Bound the length (chars, so multi-byte names stay valid UTF-8).
    if out.chars().count() > 120 {
        out = out.chars().take(120).collect();
    }
    // Windows names cannot end with a dot or a space.
    while out.ends_with('.') || out.ends_with(' ') {
        out.pop();
    }

    // Reserved DOS device names (with or without an extension) get a prefix.
    const RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
        "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    let stem = out.split('.').next().unwrap_or("");
    if RESERVED.iter().any(|r| stem.eq_ignore_ascii_case(r)) {
        out.insert(0, '_');
    }

    if out.is_empty() {
        out = "file".to_string();
    }
    out
}

/// First non-existing path for `name` in `dir`: `name`, `name (1)`, `name (2)`, ...
/// (suffix before the extension). Pure given an `exists` probe, so it's testable.
fn unique_path_with(dir: &std::path::Path, name: &str, exists: impl Fn(&std::path::Path) -> bool) -> PathBuf {
    let first = dir.join(name);
    if !exists(&first) {
        return first;
    }
    let (stem, ext) = match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    };
    for n in 1..10_000u32 {
        let cand = dir.join(format!("{stem} ({n}){ext}"));
        if !exists(&cand) {
            return cand;
        }
    }
    dir.join(format!("{stem}.{}{ext}", std::process::id()))
}

fn unique_path(dir: &std::path::Path, name: &str) -> PathBuf {
    unique_path_with(dir, name, |p| p.exists())
}

// ---------------------------------------------------------------------------
// Receive state machine.
// ---------------------------------------------------------------------------

struct Transfer {
    /// Sanitized final basename (the actual path is resolved collision-free at DONE).
    name: String,
    tmp: PathBuf,
    /// Buffered: chunks arrive as ≤16 KiB frames on the DataChannel callback thread;
    /// buffering keeps that path to ~2 syscalls/MiB instead of one per frame.
    file: BufWriter<fs::File>,
    expected: u64,
    written: u64,
}

impl Transfer {
    /// Abort-path cleanup: close the handle, then delete the temp file (Windows
    /// requires the handle closed before the file can be removed).
    fn discard(self) {
        let Transfer { tmp, file, .. } = self;
        drop(file);
        let _ = fs::remove_file(&tmp);
    }
}

static TRANSFERS: LazyLock<Mutex<HashMap<u32, Transfer>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// The current session's "file" DataChannel (host reply path: accept/reject/done).
static CURRENT: ChannelSlot = ChannelSlot::new();

/// Register (or clear with `None`) the current session's file channel.
pub fn set_channel(dc: Option<glib::Object>) {
    CURRENT.set(dc);
}

/// Clear the registration only if `dc` IS the currently-registered channel; returns
/// whether it was. A stale close from a torn-down session must not deregister the
/// new session's channel (or worse, trigger a reset of ITS transfers).
pub fn clear_channel_if(dc: &glib::Object) -> bool {
    CURRENT.clear_if(dc)
}

fn reply(frame: Vec<u8>) {
    CURRENT.send_data(glib::Bytes::from_owned(frame));
}

/// Abort every in-flight transfer and delete its temp file. Called on session
/// teardown / channel close so a dropped client never leaves `.part` litter.
pub fn reset() {
    let drained: Vec<(u32, Transfer)> = TRANSFERS
        .lock()
        .map(|mut t| t.drain().collect())
        .unwrap_or_default();
    for (id, tr) in drained {
        tracing::info!("file transfer #{id} '{}' aborted (session ended)", tr.name);
        tr.discard();
    }
}

fn reject(id: u32, reason: &str) {
    tracing::warn!("file transfer #{id} rejected: {reason}");
    reply(encode_reject(id, reason));
}

/// Drop transfer `id` (if present) and delete its temp file.
fn abort(id: u32) {
    let tr = TRANSFERS.lock().ok().and_then(|mut m| m.remove(&id));
    if let Some(tr) = tr {
        tr.discard();
    }
}

fn handle_offer(id: u32, size: u64, raw_name: &str) {
    // Same consent gate as injected input + inbound clipboard: an unapproved peer
    // must not be able to write files onto the host either.
    if !crate::input::input_allowed() {
        reject(id, "consent not granted on host");
        return;
    }
    if size > max_bytes() {
        reject(id, &format!("file too large (max {} bytes)", max_bytes()));
        return;
    }
    {
        let Ok(map) = TRANSFERS.lock() else { return };
        if map.contains_key(&id) {
            reject(id, "duplicate transfer id");
            return;
        }
        if map.len() >= MAX_CONCURRENT {
            reject(id, "too many concurrent transfers");
            return;
        }
    }

    let name = sanitize_filename(raw_name);
    let dir = save_dir();
    if let Err(e) = fs::create_dir_all(&dir) {
        reject(id, &format!("cannot create save dir: {e}"));
        return;
    }
    // Temp name is id-scoped so concurrent transfers of the same name never collide.
    let tmp = dir.join(format!("{name}.rcd{id}.part"));
    let file = match fs::File::create(&tmp) {
        Ok(f) => BufWriter::with_capacity(512 * 1024, f),
        Err(e) => {
            reject(id, &format!("cannot create file: {e}"));
            return;
        }
    };

    tracing::info!("file transfer #{id}: receiving '{name}' ({size} bytes) -> {}", dir.display());
    if let Ok(mut map) = TRANSFERS.lock() {
        map.insert(id, Transfer { name, tmp, file, expected: size, written: 0 });
    }
    reply(encode_accept(id));
}

fn handle_chunk(id: u32, data: &[u8]) {
    // On failure the transfer is removed INSIDE the lock (one acquisition) and its
    // temp file discarded after the guard drops.
    let mut aborted: Option<(Transfer, String)> = None;
    if let Ok(mut map) = TRANSFERS.lock() {
        let Some(tr) = map.get_mut(&id) else {
            return; // unknown/aborted id — chunks may race an abort; ignore
        };
        if tr.written + data.len() as u64 > tr.expected {
            let reason = format!(
                "received more bytes than offered ({} > {})",
                tr.written + data.len() as u64,
                tr.expected
            );
            aborted = map.remove(&id).map(|t| (t, reason));
        } else if let Err(e) = tr.file.write_all(data) {
            let reason = format!("write failed: {e}");
            aborted = map.remove(&id).map(|t| (t, reason));
        } else {
            tr.written += data.len() as u64;
        }
    }
    if let Some((tr, reason)) = aborted {
        tr.discard();
        reject(id, &reason);
    }
}

fn handle_done(id: u32) {
    let tr = match TRANSFERS.lock() {
        Ok(mut map) => map.remove(&id),
        Err(_) => None,
    };
    let Some(mut tr) = tr else {
        return;
    };
    if tr.written != tr.expected {
        let reason = format!("size mismatch (got {} of {} bytes)", tr.written, tr.expected);
        tr.discard();
        reject(id, &reason);
        return;
    }
    if let Err(e) = tr.file.flush() {
        let reason = format!("flush failed: {e}");
        tr.discard();
        reject(id, &reason);
        return;
    }

    let Transfer { name, tmp, file, written, .. } = tr;
    drop(file); // close before rename (Windows)

    let final_path = unique_path(&save_dir(), &name);
    if let Err(e) = fs::rename(&tmp, &final_path) {
        let _ = fs::remove_file(&tmp);
        reject(id, &format!("rename failed: {e}"));
        return;
    }
    tracing::info!(
        "file transfer #{id} COMPLETE: {} ({} bytes)",
        final_path.display(),
        written
    );
    crate::audit::log_event("file_received", &[("name", &name), ("bytes", &written.to_string())]);
    reply(encode_done(id)); // ack: verified + saved
}

/// Decode + apply one inbound file-channel frame (called from the DataChannel
/// handler on the GLib thread).
pub fn handle_incoming(data: &[u8]) {
    match decode(data) {
        Some(Frame::Offer { id, size, name }) => handle_offer(id, size, name),
        Some(Frame::Chunk { id, data }) => handle_chunk(id, data),
        Some(Frame::Done { id }) => handle_done(id),
        Some(Frame::Reject { id, reason }) => {
            // Client-side cancel of its own transfer.
            tracing::info!("file transfer #{id} cancelled by client: {reason}");
            abort(id);
        }
        Some(Frame::Accept { .. }) => {
            // Host->client transfers do not exist yet; ignore.
        }
        None => tracing::warn!("file: malformed/unknown frame ({} bytes)", data.len()),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        decode, encode_accept, encode_done, encode_reject, sanitize_filename, unique_path_with,
        Frame, OP_FILE_CHUNK, OP_FILE_OFFER,
    };
    use std::path::{Path, PathBuf};

    /// Build an OFFER frame the way the client does (see rcd-client wire.ts).
    fn encode_offer(id: u32, size: u64, name: &str) -> Vec<u8> {
        let n = name.as_bytes();
        let mut f = vec![OP_FILE_OFFER];
        f.extend_from_slice(&id.to_le_bytes());
        f.extend_from_slice(&size.to_le_bytes());
        f.extend_from_slice(&(n.len() as u16).to_le_bytes());
        f.extend_from_slice(n);
        f
    }

    #[test]
    fn offer_roundtrip_and_bounds() {
        let f = encode_offer(7, 1_234_567_890_123, "한글 report.pdf");
        assert_eq!(
            decode(&f),
            Some(Frame::Offer { id: 7, size: 1_234_567_890_123, name: "한글 report.pdf" })
        );
        // Truncated name -> None.
        assert_eq!(decode(&f[..f.len() - 1]), None);
        // Header shorter than fixed part -> None.
        assert_eq!(decode(&f[..10]), None);
        // Empty input -> None.
        assert_eq!(decode(&[]), None);
    }

    #[test]
    fn chunk_done_accept_reject_roundtrip() {
        let mut chunk = vec![OP_FILE_CHUNK];
        chunk.extend_from_slice(&9u32.to_le_bytes());
        chunk.extend_from_slice(&[1, 2, 3]);
        assert_eq!(decode(&chunk), Some(Frame::Chunk { id: 9, data: &[1, 2, 3] }));
        // Empty chunk is legal (a no-op for the receiver).
        assert_eq!(decode(&chunk[..5]), Some(Frame::Chunk { id: 9, data: &[] }));

        assert_eq!(decode(&encode_accept(3)), Some(Frame::Accept { id: 3 }));
        assert_eq!(decode(&encode_done(4)), Some(Frame::Done { id: 4 }));
        assert_eq!(
            decode(&encode_reject(5, "too large")),
            Some(Frame::Reject { id: 5, reason: "too large" })
        );
        // Unknown opcode -> None.
        assert_eq!(decode(&[0xEE, 0, 0, 0, 0]), None);
    }

    #[test]
    fn sanitize_strips_paths_and_invalid_chars() {
        assert_eq!(sanitize_filename("report.pdf"), "report.pdf");
        assert_eq!(sanitize_filename("..\\..\\evil.exe"), "evil.exe");
        assert_eq!(sanitize_filename("/etc/passwd"), "passwd");
        assert_eq!(sanitize_filename("C:\\Users\\x\\doc.txt"), "doc.txt");
        assert_eq!(sanitize_filename("a<b>c:d\"e|f?g*.txt"), "a_b_c_d_e_f_g_.txt");
        assert_eq!(sanitize_filename("tab\there.txt"), "tab_here.txt");
        // Trailing dots/spaces are illegal on Windows.
        assert_eq!(sanitize_filename("name..."), "name");
        assert_eq!(sanitize_filename("name.txt   "), "name.txt");
        // Reserved device names get defused, with or without extension.
        assert_eq!(sanitize_filename("CON"), "_CON");
        assert_eq!(sanitize_filename("con.txt"), "_con.txt");
        assert_eq!(sanitize_filename("LPT3.log"), "_LPT3.log");
        // Degenerate inputs never yield an empty name.
        assert_eq!(sanitize_filename(""), "file");
        assert_eq!(sanitize_filename("..."), "file");
        assert_eq!(sanitize_filename("..\\..\\"), "file");
        // Multibyte names survive, and length is bounded.
        assert_eq!(sanitize_filename("한글 파일.txt"), "한글 파일.txt");
        let long = "x".repeat(500) + ".txt";
        assert!(sanitize_filename(&long).chars().count() <= 120);
    }

    #[test]
    fn unique_path_appends_counter_before_extension() {
        let taken: Vec<PathBuf> =
            vec![PathBuf::from("d/a.txt"), PathBuf::from("d/a (1).txt"), PathBuf::from("d/noext")];
        let exists = |p: &Path| taken.iter().any(|t| t == p);
        assert_eq!(unique_path_with(Path::new("d"), "b.txt", exists), PathBuf::from("d/b.txt"));
        assert_eq!(unique_path_with(Path::new("d"), "a.txt", exists), PathBuf::from("d/a (2).txt"));
        assert_eq!(unique_path_with(Path::new("d"), "noext", exists), PathBuf::from("d/noext (1)"));
        // A dotfile's leading dot is not an "extension".
        assert_eq!(
            unique_path_with(Path::new("d"), ".gitignore", |_| false),
            PathBuf::from("d/.gitignore")
        );
    }
}
