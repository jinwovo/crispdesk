//! Tiny helpers for the environment-variable idioms shared across modules, so the
//! "VAR=0 disables" convention and "parse-or-default" pattern live in ONE place
//! (previously hand-rolled in clipboard.rs, files.rs, and abr.rs).

/// True unless the variable is exactly `"0"` — the project's feature-disable
/// convention (`CLIPBOARD=0`, `FILES=0`, `ABR=0`, ...). Unset ⇒ on.
pub fn on(name: &str) -> bool {
    std::env::var(name).as_deref() != Ok("0")
}

/// Parse the variable as `T`, falling back to `default` when unset, empty, or
/// unparseable.
pub fn parse_or<T: std::str::FromStr>(name: &str, default: T) -> T {
    std::env::var(name)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}
