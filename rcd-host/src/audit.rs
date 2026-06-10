//! Best-effort local audit log (JSONL) of connection lifecycle events, so there is a
//! record of when remote-control sessions started/ended on this machine — table-stakes
//! accountability for a remote-desktop product.
//!
//! Destination: `AUDIT_LOG` (a file path), or `AUDIT_LOG=off` to disable, else
//! `%LOCALAPPDATA%\crispdesk\audit.jsonl`. Writes are best-effort: a failure is logged,
//! never fatal. Each line is one JSON object: `{"ts":<unix_ms>,"event":"...",<detail>}`.

use std::fs::OpenOptions;
use std::io::Write;
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{json, Map, Value};

fn log_path() -> Option<std::path::PathBuf> {
    match std::env::var("AUDIT_LOG").ok().as_deref() {
        Some("off") => None,
        Some(p) if !p.is_empty() => Some(p.into()),
        _ => {
            let base = std::env::var("LOCALAPPDATA").ok()?;
            let dir = std::path::Path::new(&base).join("crispdesk");
            let _ = std::fs::create_dir_all(&dir);
            Some(dir.join("audit.jsonl"))
        }
    }
}

/// Append one event with optional `(key, value)` detail fields. Values are serialized
/// via serde_json, so quotes/backslashes/anything (e.g. an `ENCODER` env value) are
/// safely escaped — no JSON injection.
pub fn log_event(event: &str, detail: &[(&str, &str)]) {
    let Some(path) = log_path() else { return };
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);

    let line = build_line(ts, event, detail);

    match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(mut f) => {
            if let Err(e) = f.write_all(line.as_bytes()) {
                tracing::warn!("audit log write failed: {e}");
            }
        }
        Err(e) => tracing::warn!("audit log open failed ({}): {e}", path.display()),
    }
}

/// Build one JSONL line (pure, so it's unit-tested for injection-safety).
fn build_line(ts: u64, event: &str, detail: &[(&str, &str)]) -> String {
    let mut obj = Map::new();
    obj.insert("ts".into(), json!(ts));
    obj.insert("event".into(), json!(event));
    for (k, v) in detail {
        obj.insert((*k).to_string(), json!(v));
    }
    format!("{}\n", Value::Object(obj))
}

#[cfg(test)]
mod tests {
    use super::build_line;

    #[test]
    fn line_is_valid_json_and_escapes_injection() {
        // A hostile ENCODER value with quotes/backslashes must NOT break the JSONL.
        let line = build_line(123, "session_start", &[("encoder", "x264enc\"evil\\")]);
        assert!(line.ends_with('\n'));
        let v: serde_json::Value = serde_json::from_str(line.trim()).expect("valid JSON");
        assert_eq!(v["ts"], 123);
        assert_eq!(v["event"], "session_start");
        assert_eq!(v["encoder"], "x264enc\"evil\\");
    }
}
