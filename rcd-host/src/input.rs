//! Windows input injection for the "input" DataChannel (PROTOCOL.md opcodes).
//!
//!   0x01 MOUSE_MOVE_ABS — absolute move on the PRIMARY monitor via `SetCursorPos`
//!   0x02 MOUSE_BUTTON   — button down/up via `SendInput` (L/R/M/X1/X2)
//!   0x03 KEY            — keyboard via `SendInput` with hardware SCANCODES (layout-
//!                         independent; the client maps KeyboardEvent.code → scancode)
//!   0x04 WHEEL          — vertical/horizontal wheel in WHEEL_DELTA units (±120/notch)
//!
//! TODO(next):
//!   * multi-monitor: map through the *captured* monitor's rect and use SendInput with
//!     MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK (0..65535 space);
//!   * per-process DPI awareness so pixel mapping matches scaled displays;
//!   * "release all keys" safety on disconnect (host-side mirror of the client's);
//!   * the secure desktop / UAC requires running as a LocalSystem service (later);
//!   * gamepad (0x05) via ViGEmBus.

use windows::Win32::UI::Input::KeyboardAndMouse::{
    SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, INPUT_MOUSE, KEYBDINPUT, KEYEVENTF_EXTENDEDKEY,
    KEYEVENTF_KEYUP, KEYEVENTF_SCANCODE, MOUSEEVENTF_HWHEEL, MOUSEEVENTF_LEFTDOWN,
    MOUSEEVENTF_LEFTUP, MOUSEEVENTF_MIDDLEDOWN, MOUSEEVENTF_MIDDLEUP, MOUSEEVENTF_RIGHTDOWN,
    MOUSEEVENTF_RIGHTUP, MOUSEEVENTF_WHEEL, MOUSEEVENTF_XDOWN, MOUSEEVENTF_XUP, MOUSEINPUT,
    MOUSE_EVENT_FLAGS, VIRTUAL_KEY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SetCursorPos, SM_CXSCREEN, SM_CYSCREEN,
};

// XBUTTON1/2 values for MOUSEINPUT.mouseData (hardcoded to dodge per-version const types).
const XBUTTON1_DATA: i32 = 0x0001;
const XBUTTON2_DATA: i32 = 0x0002;

/// Inject a single INPUT event, logging (not failing) on rejection.
fn send_one(input: INPUT) {
    // SAFETY: SendInput is a plain Win32 FFI call; the INPUT array lives on the stack
    // for the duration of the call.
    unsafe {
        let sent = SendInput(&[input], std::mem::size_of::<INPUT>() as i32);
        if sent != 1 {
            tracing::warn!("SendInput injected {sent}/1 events (blocked by UIPI/secure desktop?)");
        }
    }
}

fn mouse_input(flags: MOUSE_EVENT_FLAGS, data: i32) -> INPUT {
    INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                // mouseData is declared u32 but Windows interprets it as a signed DWORD
                // for wheel deltas — two's-complement cast keeps negative notches intact.
                mouseData: data as u32,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// Primary monitor size in pixels (logical / DPI-scaled). The ASPECT RATIO is
/// correct regardless of DPI scaling, which is exactly what the encoder scaler
/// needs to avoid letterbox bars (those would break the normalized input mapping).
pub fn primary_resolution() -> Option<(u32, u32)> {
    // SAFETY: plain Win32 FFI calls.
    unsafe {
        let w = GetSystemMetrics(SM_CXSCREEN);
        let h = GetSystemMetrics(SM_CYSCREEN);
        if w > 0 && h > 0 {
            Some((w as u32, h as u32))
        } else {
            None
        }
    }
}

/// 0x01 — move the cursor to a normalized (0..1) position on the primary monitor.
pub fn mouse_move_abs(nx: f32, ny: f32) {
    let nx = nx.clamp(0.0, 1.0);
    let ny = ny.clamp(0.0, 1.0);

    // SAFETY: plain Win32 FFI calls without pointer/lifetime invariants.
    unsafe {
        let w = GetSystemMetrics(SM_CXSCREEN);
        let h = GetSystemMetrics(SM_CYSCREEN);
        if w <= 0 || h <= 0 {
            tracing::warn!("GetSystemMetrics returned non-positive screen size ({w}x{h})");
            return;
        }
        let x = (nx * (w - 1) as f32).round() as i32;
        let y = (ny * (h - 1) as f32).round() as i32;
        if let Err(e) = SetCursorPos(x, y) {
            tracing::warn!("SetCursorPos({x},{y}) failed: {e}");
        }
    }
}

/// 0x02 — mouse button. `button`: 0=left, 1=right, 2=middle, 3=back(X1), 4=forward(X2).
pub fn mouse_button(button: u8, down: bool) {
    let (flags, data) = match (button, down) {
        (0, true) => (MOUSEEVENTF_LEFTDOWN, 0),
        (0, false) => (MOUSEEVENTF_LEFTUP, 0),
        (1, true) => (MOUSEEVENTF_RIGHTDOWN, 0),
        (1, false) => (MOUSEEVENTF_RIGHTUP, 0),
        (2, true) => (MOUSEEVENTF_MIDDLEDOWN, 0),
        (2, false) => (MOUSEEVENTF_MIDDLEUP, 0),
        (3, true) => (MOUSEEVENTF_XDOWN, XBUTTON1_DATA),
        (3, false) => (MOUSEEVENTF_XUP, XBUTTON1_DATA),
        (4, true) => (MOUSEEVENTF_XDOWN, XBUTTON2_DATA),
        (4, false) => (MOUSEEVENTF_XUP, XBUTTON2_DATA),
        (other, _) => {
            tracing::warn!("unknown mouse button {other}");
            return;
        }
    };
    send_one(mouse_input(flags, data));
}

/// 0x04 — wheel in WHEEL_DELTA units (+120 = one notch up/away; dx for horizontal).
pub fn wheel(dx: i16, dy: i16) {
    if dy != 0 {
        send_one(mouse_input(MOUSEEVENTF_WHEEL, dy as i32));
    }
    if dx != 0 {
        send_one(mouse_input(MOUSEEVENTF_HWHEEL, dx as i32));
    }
}

/// 0x03 — keyboard by hardware SCANCODE (layout-independent; wVk=0 + KEYEVENTF_SCANCODE).
/// `extended` marks the 0xE0-prefixed keys (right ctrl/alt, arrows, ins/del/home/end/
/// pgup/pgdn, numpad-divide, numpad-enter, win keys).
pub fn key_scan(scan: u16, down: bool, extended: bool) {
    let mut flags = KEYEVENTF_SCANCODE;
    if !down {
        flags = flags | KEYEVENTF_KEYUP;
    }
    if extended {
        flags = flags | KEYEVENTF_EXTENDEDKEY;
    }
    let input = INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    send_one(input);
}
