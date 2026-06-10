//! Multi-monitor selection (capture + input mapping).
//!
//! `MONITOR=<index>` selects which display to capture and inject input on; unset =
//! the primary monitor (unchanged behaviour). We enumerate monitors with the Win32
//! GDI APIs and select one by index, keeping BOTH its `HMONITOR` (handed to
//! `d3d11screencapturesrc monitor-handle=...` so capture targets exactly that display)
//! AND its virtual-desktop pixel rect (used by `pipeline::scaled_dims` for the encode
//! size and by `input::mouse_move_abs` to map normalized 0..1 onto the right monitor).
//! Using the same HMONITOR for capture and the rect for input guarantees they refer to
//! the SAME physical monitor (index ordering across DXGI vs GDI is not guaranteed).
//!
//! With per-monitor-DPI-V2 awareness (see main::set_dpi_awareness) the rect is in
//! PHYSICAL pixels, matching the d3d11 capture — so the normalized mapping is exact.

use std::sync::OnceLock;

/// The selected monitor's capture handle + virtual-desktop pixel rect.
#[derive(Clone, Copy, Debug)]
pub struct MonitorSel {
    /// HMONITOR value, passed verbatim to `d3d11screencapturesrc monitor-handle=`.
    pub handle: isize,
    pub left: i32,
    pub top: i32,
    pub width: i32,
    pub height: i32,
    pub primary: bool,
}

/// Resolved once at startup. `None` = use the primary monitor via the default path
/// (SetCursorPos over the primary; scaled_dims via GetSystemMetrics) — i.e. unchanged.
static SELECTED: OnceLock<Option<MonitorSel>> = OnceLock::new();

/// The currently-selected monitor, if `MONITOR` picked a specific one.
pub fn selected() -> Option<MonitorSel> {
    SELECTED.get().copied().flatten()
}

/// Read `MONITOR` from the env, enumerate displays, pick one, and log them all.
/// Idempotent: only the first call resolves the selection.
pub fn init_from_env() {
    if SELECTED.get().is_some() {
        return;
    }
    let monitors = list();
    tracing::info!("detected {} monitor(s):", monitors.len());
    for (i, m) in monitors.iter().enumerate() {
        tracing::info!(
            "  [{i}] {}x{} at ({},{}){}",
            m.width,
            m.height,
            m.left,
            m.top,
            if m.primary { " (primary)" } else { "" }
        );
    }

    let sel = match std::env::var("MONITOR").ok().and_then(|s| s.parse::<usize>().ok()) {
        Some(idx) => match monitors.get(idx) {
            Some(m) => {
                tracing::info!("MONITOR={idx} -> capturing monitor [{idx}] ({}x{})", m.width, m.height);
                Some(*m)
            }
            None => {
                tracing::warn!(
                    "MONITOR={idx} out of range ({} monitor(s)); falling back to primary",
                    monitors.len()
                );
                None
            }
        },
        None => None, // primary, default path
    };

    let _ = SELECTED.set(sel);
}

#[cfg(windows)]
fn list() -> Vec<MonitorSel> {
    use windows::Win32::Foundation::{BOOL, LPARAM, RECT, TRUE};
    use windows::Win32::Graphics::Gdi::{
        EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO,
    };

    // MONITORINFOF_PRIMARY (winuser.h); not re-exported as a constant in this windows
    // crate version, so define it locally.
    const MONITORINFOF_PRIMARY: u32 = 0x0000_0001;

    // EnumDisplayMonitors callback: append each monitor's handle + rect to the Vec
    // passed through `lparam`.
    unsafe extern "system" fn cb(h: HMONITOR, _hdc: HDC, _rc: *mut RECT, lparam: LPARAM) -> BOOL {
        let out = &mut *(lparam.0 as *mut Vec<MonitorSel>);
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        if GetMonitorInfoW(h, &mut mi).as_bool() {
            let r = mi.rcMonitor;
            out.push(MonitorSel {
                handle: h.0 as isize,
                left: r.left,
                top: r.top,
                width: r.right - r.left,
                height: r.bottom - r.top,
                primary: (mi.dwFlags & MONITORINFOF_PRIMARY) != 0,
            });
        }
        TRUE
    }

    let mut out: Vec<MonitorSel> = Vec::new();
    // SAFETY: standard EnumDisplayMonitors usage; `out` outlives the synchronous call.
    unsafe {
        let _ = EnumDisplayMonitors(
            HDC::default(),
            None,
            Some(cb),
            LPARAM(&mut out as *mut _ as isize),
        );
    }
    // Put the primary first so MONITOR=0 is the primary on typical setups.
    out.sort_by_key(|m| !m.primary);
    out
}

#[cfg(not(windows))]
fn list() -> Vec<MonitorSel> {
    Vec::new()
}
