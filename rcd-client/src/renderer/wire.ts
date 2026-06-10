// rcd-client — WIRE format (PURE encode/decode helpers, no DOM/Electron state).
//
// This module is intentionally side-effect free and depends only on standard
// JS values available in BOTH the browser renderer and Node's test runner
// (ArrayBuffer, DataView, TextEncoder/TextDecoder). It exists so the input
// DataChannel wire format can be unit-tested in isolation (see test/wire.test.js)
// WITHOUT pulling in the renderer's RTCPeerConnection / DOM machinery.
//
// The byte layouts here are NORMATIVE and must match rcd-host's decoder EXACTLY.
// Do not change offsets, endianness, or opcode values without updating the host.

// ---------------------------------------------------------------------------
// Opcodes (first byte of every frame on the relevant channel).
// ---------------------------------------------------------------------------

export const OPCODE_MOUSE_MOVE_ABS = 0x01; // [f32 x][f32 y] normalized 0..1
export const OPCODE_MOUSE_BUTTON = 0x02; //   [u8 button 0=L 1=R 2=M 3=X1 4=X2][u8 down(1)/up(0)]
export const OPCODE_KEY = 0x03; //            [u16 scancode LE][u8 flags bit0=down bit1=extended]
export const OPCODE_WHEEL = 0x04; //          [i16 wheelY][i16 wheelX] WHEEL_DELTA units (+120/notch)
// Reserved: 0x05 GAMEPAD ...
export const OPCODE_CLIPBOARD_TEXT = 0x06; // [u32 len LE][utf8 text] on the "clipboard" channel

// ---------------------------------------------------------------------------
// Encoders / decoders. All multi-byte integers are little-endian.
// ---------------------------------------------------------------------------

/** Build the 9-byte MOUSE_MOVE_ABS message (little-endian). */
export function encodeMouseMoveAbs(x: number, y: number): ArrayBuffer {
  const buf = new ArrayBuffer(9);
  const dv = new DataView(buf);
  dv.setUint8(0, OPCODE_MOUSE_MOVE_ABS);
  dv.setFloat32(1, x, /* littleEndian */ true);
  dv.setFloat32(5, y, /* littleEndian */ true);
  return buf;
}

export function encodeMouseButton(button: number, down: boolean): ArrayBuffer {
  const buf = new ArrayBuffer(3);
  const dv = new DataView(buf);
  dv.setUint8(0, OPCODE_MOUSE_BUTTON);
  dv.setUint8(1, button);
  dv.setUint8(2, down ? 1 : 0);
  return buf;
}

export function encodeKey(scan: number, down: boolean, extended: boolean): ArrayBuffer {
  const buf = new ArrayBuffer(4);
  const dv = new DataView(buf);
  dv.setUint8(0, OPCODE_KEY);
  dv.setUint16(1, scan, true);
  dv.setUint8(3, (down ? 0x01 : 0) | (extended ? 0x02 : 0));
  return buf;
}

export function encodeWheel(dy: number, dx: number): ArrayBuffer {
  const buf = new ArrayBuffer(5);
  const dv = new DataView(buf);
  dv.setUint8(0, OPCODE_WHEEL);
  dv.setInt16(1, dy, true);
  dv.setInt16(3, dx, true);
  return buf;
}

/** Build a CLIPBOARD_TEXT frame: [0x06][u32 len LE][utf8]. */
export function encodeClipboard(text: string): ArrayBuffer {
  const utf8 = new TextEncoder().encode(text);
  const buf = new ArrayBuffer(5 + utf8.byteLength);
  const dv = new DataView(buf);
  dv.setUint8(0, OPCODE_CLIPBOARD_TEXT);
  dv.setUint32(1, utf8.byteLength, true);
  new Uint8Array(buf, 5).set(utf8);
  return buf;
}

/** Decode a CLIPBOARD_TEXT frame; returns the text or null if malformed. */
export function decodeClipboard(buf: ArrayBuffer): string | null {
  if (buf.byteLength < 5) return null;
  const dv = new DataView(buf);
  if (dv.getUint8(0) !== OPCODE_CLIPBOARD_TEXT) return null;
  const len = dv.getUint32(1, true);
  if (buf.byteLength < 5 + len) return null;
  return new TextDecoder().decode(new Uint8Array(buf, 5, len));
}

// ---------------------------------------------------------------------------
// Input mapping tables.
// ---------------------------------------------------------------------------

/** JS MouseEvent.button (0=L 1=M 2=R 3=back 4=fwd) -> protocol button id. */
export const BUTTON_MAP: readonly number[] = [0, 2, 1, 3, 4];

/**
 * KeyboardEvent.code -> [PS/2 Set-1 make scancode, extended?]. `code` identifies
 * the PHYSICAL key (layout-independent), which is exactly what scancode injection
 * wants — the host's keyboard layout / Korean IME applies host-side.
 */
export const SCANCODES: Record<string, readonly [number, boolean]> = {
  Escape: [0x01, false], Digit1: [0x02, false], Digit2: [0x03, false],
  Digit3: [0x04, false], Digit4: [0x05, false], Digit5: [0x06, false],
  Digit6: [0x07, false], Digit7: [0x08, false], Digit8: [0x09, false],
  Digit9: [0x0a, false], Digit0: [0x0b, false], Minus: [0x0c, false],
  Equal: [0x0d, false], Backspace: [0x0e, false], Tab: [0x0f, false],
  KeyQ: [0x10, false], KeyW: [0x11, false], KeyE: [0x12, false],
  KeyR: [0x13, false], KeyT: [0x14, false], KeyY: [0x15, false],
  KeyU: [0x16, false], KeyI: [0x17, false], KeyO: [0x18, false],
  KeyP: [0x19, false], BracketLeft: [0x1a, false], BracketRight: [0x1b, false],
  Enter: [0x1c, false], ControlLeft: [0x1d, false],
  KeyA: [0x1e, false], KeyS: [0x1f, false], KeyD: [0x20, false],
  KeyF: [0x21, false], KeyG: [0x22, false], KeyH: [0x23, false],
  KeyJ: [0x24, false], KeyK: [0x25, false], KeyL: [0x26, false],
  Semicolon: [0x27, false], Quote: [0x28, false], Backquote: [0x29, false],
  ShiftLeft: [0x2a, false], Backslash: [0x2b, false],
  KeyZ: [0x2c, false], KeyX: [0x2d, false], KeyC: [0x2e, false],
  KeyV: [0x2f, false], KeyB: [0x30, false], KeyN: [0x31, false],
  KeyM: [0x32, false], Comma: [0x33, false], Period: [0x34, false],
  Slash: [0x35, false], ShiftRight: [0x36, false],
  NumpadMultiply: [0x37, false], AltLeft: [0x38, false], Space: [0x39, false],
  CapsLock: [0x3a, false],
  F1: [0x3b, false], F2: [0x3c, false], F3: [0x3d, false], F4: [0x3e, false],
  F5: [0x3f, false], F6: [0x40, false], F7: [0x41, false], F8: [0x42, false],
  F9: [0x43, false], F10: [0x44, false], F11: [0x57, false], F12: [0x58, false],
  NumLock: [0x45, false], ScrollLock: [0x46, false],
  Numpad7: [0x47, false], Numpad8: [0x48, false], Numpad9: [0x49, false],
  NumpadSubtract: [0x4a, false], Numpad4: [0x4b, false], Numpad5: [0x4c, false],
  Numpad6: [0x4d, false], NumpadAdd: [0x4e, false], Numpad1: [0x4f, false],
  Numpad2: [0x50, false], Numpad3: [0x51, false], Numpad0: [0x52, false],
  NumpadDecimal: [0x53, false], IntlBackslash: [0x56, false],
  // Extended (0xE0-prefixed) keys:
  NumpadEnter: [0x1c, true], ControlRight: [0x1d, true],
  NumpadDivide: [0x35, true], AltRight: [0x38, true], PrintScreen: [0x37, true],
  Home: [0x47, true], ArrowUp: [0x48, true], PageUp: [0x49, true],
  ArrowLeft: [0x4b, true], ArrowRight: [0x4d, true], End: [0x4f, true],
  ArrowDown: [0x50, true], PageDown: [0x51, true], Insert: [0x52, true],
  Delete: [0x53, true], MetaLeft: [0x5b, true], MetaRight: [0x5c, true],
  ContextMenu: [0x5d, true],
  // Korean keyboard specials (한/영, 한자).
  Lang1: [0x72, false], Lang2: [0x71, false],
};
