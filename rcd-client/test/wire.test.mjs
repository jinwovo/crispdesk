// Unit tests for the input/clipboard wire format.
//
// These import the COMPILED renderer wire module (dist/renderer/wire.js), which
// tsconfig.renderer.json emits as a native ES module. So this test file is ESM
// too (.js under "type":"module" via the package "test" script using --import is
// unnecessary; node loads dist/renderer/wire.js as ESM because it has a top-level
// `export`). Run with: npm test  (which builds first, then `node --test`).
//
// The byte layouts asserted here are the SINGLE SOURCE OF TRUTH shared with
// rcd-host's decoder. If a test here changes, the host decoder must change too.

import test from "node:test";
import assert from "node:assert/strict";

import {
  OPCODE_MOUSE_MOVE_ABS,
  OPCODE_MOUSE_BUTTON,
  OPCODE_KEY,
  OPCODE_WHEEL,
  OPCODE_CLIPBOARD_TEXT,
  encodeMouseMoveAbs,
  encodeMouseButton,
  encodeKey,
  encodeWheel,
  encodeClipboard,
  decodeClipboard,
  BUTTON_MAP,
  SCANCODES,
} from "../dist/renderer/wire.js";

/** Helper: view an ArrayBuffer as a byte array for easy assertions. */
function bytes(buf) {
  assert.ok(buf instanceof ArrayBuffer, "expected an ArrayBuffer");
  return Array.from(new Uint8Array(buf));
}

test("opcode constants have the pinned values", () => {
  assert.equal(OPCODE_MOUSE_MOVE_ABS, 0x01);
  assert.equal(OPCODE_MOUSE_BUTTON, 0x02);
  assert.equal(OPCODE_KEY, 0x03);
  assert.equal(OPCODE_WHEEL, 0x04);
  assert.equal(OPCODE_CLIPBOARD_TEXT, 0x06);
});

test("encodeMouseMoveAbs: 9-byte layout, little-endian f32 x then y", () => {
  const buf = encodeMouseMoveAbs(0.25, 0.75);
  assert.equal(buf.byteLength, 9);
  const dv = new DataView(buf);
  assert.equal(dv.getUint8(0), OPCODE_MOUSE_MOVE_ABS);
  // x at offset 1, y at offset 5, both little-endian f32.
  assert.equal(dv.getFloat32(1, true), 0.25);
  assert.equal(dv.getFloat32(5, true), 0.75);
  // Confirm endianness is actually LITTLE: big-endian read must NOT match.
  assert.notEqual(dv.getFloat32(1, false), 0.25);
});

test("encodeMouseMoveAbs: exact bytes for 0 and 1 (boundary)", () => {
  // 0.0 f32 LE = 00 00 00 00 ; 1.0 f32 LE = 00 00 80 3f
  assert.deepEqual(bytes(encodeMouseMoveAbs(0, 1)), [
    0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x80, 0x3f,
  ]);
});

test("encodeMouseButton: 3-byte layout [opcode][button][down]", () => {
  assert.deepEqual(bytes(encodeMouseButton(2, true)), [0x02, 0x02, 0x01]);
  assert.deepEqual(bytes(encodeMouseButton(0, false)), [0x02, 0x00, 0x00]);
  // X2 (button id 4), up.
  assert.deepEqual(bytes(encodeMouseButton(4, false)), [0x02, 0x04, 0x00]);
});

test("encodeKey: 4-byte layout [opcode][u16 scan LE][flags]", () => {
  // scancode 0x011c (a value that spans both bytes) -> 1c 01 little-endian.
  const buf = encodeKey(0x011c, true, false);
  assert.equal(buf.byteLength, 4);
  assert.deepEqual(bytes(buf), [0x03, 0x1c, 0x01, 0x01]);

  // flags: bit0 = down, bit1 = extended.
  assert.deepEqual(bytes(encodeKey(0x1d, true, true)), [0x03, 0x1d, 0x00, 0x03]);
  assert.deepEqual(bytes(encodeKey(0x1d, false, true)), [0x03, 0x1d, 0x00, 0x02]);
  assert.deepEqual(bytes(encodeKey(0x1d, false, false)), [0x03, 0x1d, 0x00, 0x00]);

  // Endianness check via DataView.
  const dv = new DataView(encodeKey(0x1234, true, false));
  assert.equal(dv.getUint16(1, true), 0x1234);
  assert.notEqual(dv.getUint16(1, false), 0x1234);
});

test("encodeWheel: 5-byte layout [opcode][i16 dy LE][i16 dx LE]", () => {
  const buf = encodeWheel(120, -120);
  assert.equal(buf.byteLength, 5);
  const dv = new DataView(buf);
  assert.equal(dv.getUint8(0), OPCODE_WHEEL);
  assert.equal(dv.getInt16(1, true), 120); // dy at offset 1
  assert.equal(dv.getInt16(3, true), -120); // dx at offset 3
  // 120 LE = 78 00 ; -120 LE (two's complement i16) = 88 ff
  assert.deepEqual(bytes(buf), [0x04, 0x78, 0x00, 0x88, 0xff]);
});

test("encodeWheel: negative values are signed (i16, little-endian)", () => {
  const dv = new DataView(encodeWheel(-1, -32767));
  assert.equal(dv.getInt16(1, true), -1);
  assert.equal(dv.getInt16(3, true), -32767);
});

test("encodeClipboard: layout [0x06][u32 len LE][utf8] for ASCII", () => {
  const buf = encodeClipboard("hi");
  assert.deepEqual(bytes(buf), [
    0x06, // opcode
    0x02, 0x00, 0x00, 0x00, // len=2 little-endian u32
    0x68, 0x69, // 'h','i'
  ]);
});

test("encodeClipboard: length prefix counts UTF-8 BYTES, not code points", () => {
  // "한" is 3 UTF-8 bytes (Eor others); "café" has an é = 2 bytes.
  const text = "café"; // c a f é -> 4 chars, 5 bytes
  const buf = encodeClipboard(text);
  const dv = new DataView(buf);
  assert.equal(dv.getUint8(0), OPCODE_CLIPBOARD_TEXT);
  assert.equal(dv.getUint32(1, true), 5, "len must be UTF-8 byte length");
  assert.equal(buf.byteLength, 5 + 5);
});

test("clipboard roundtrip: ASCII, empty, multibyte UTF-8, emoji", () => {
  for (const s of ["", "hello", "café ☕", "한국어 테스트", "emoji 😀🎉", "with\nnewlines\tand\0nul"]) {
    const decoded = decodeClipboard(encodeClipboard(s));
    assert.equal(decoded, s, `roundtrip mismatch for ${JSON.stringify(s)}`);
  }
});

test("decodeClipboard: empty string encodes to a 5-byte header and roundtrips", () => {
  const buf = encodeClipboard("");
  assert.equal(buf.byteLength, 5);
  assert.deepEqual(bytes(buf), [0x06, 0x00, 0x00, 0x00, 0x00]);
  assert.equal(decodeClipboard(buf), "");
});

test("decodeClipboard: rejects too-short buffers (header < 5 bytes)", () => {
  assert.equal(decodeClipboard(new ArrayBuffer(0)), null);
  assert.equal(decodeClipboard(new ArrayBuffer(4)), null);
});

test("decodeClipboard: rejects wrong opcode", () => {
  const buf = encodeClipboard("hi");
  new DataView(buf).setUint8(0, 0x01); // corrupt the opcode
  assert.equal(decodeClipboard(buf), null);
});

test("decodeClipboard: rejects truncated payload (len > available bytes)", () => {
  // Claim len=100 but provide only a few payload bytes.
  const buf = new ArrayBuffer(5 + 3);
  const dv = new DataView(buf);
  dv.setUint8(0, OPCODE_CLIPBOARD_TEXT);
  dv.setUint32(1, 100, true);
  new Uint8Array(buf, 5).set([0x61, 0x62, 0x63]);
  assert.equal(decodeClipboard(buf), null);
});

test("decodeClipboard: tolerates a buffer LONGER than declared len (reads only len)", () => {
  // Encode "abc" then append trailing garbage; decode must return exactly "abc".
  const head = encodeClipboard("abc");
  const extended = new ArrayBuffer(head.byteLength + 4);
  new Uint8Array(extended).set(new Uint8Array(head));
  new Uint8Array(extended, head.byteLength).set([0xde, 0xad, 0xbe, 0xef]);
  assert.equal(decodeClipboard(extended), "abc");
});

test("BUTTON_MAP: JS MouseEvent.button -> protocol id mapping", () => {
  // JS:      0=Left 1=Middle 2=Right 3=Back 4=Forward
  // Protocol: 0=L    2=M      1=R     3=X1   4=X2
  assert.deepEqual([...BUTTON_MAP], [0, 2, 1, 3, 4]);
  assert.equal(BUTTON_MAP[0], 0); // left
  assert.equal(BUTTON_MAP[1], 2); // middle -> M
  assert.equal(BUTTON_MAP[2], 1); // right  -> R
  assert.equal(BUTTON_MAP[3], 3); // back   -> X1
  assert.equal(BUTTON_MAP[4], 4); // fwd    -> X2
  // Out-of-range index is undefined (callers fall back to 0 with ?? 0).
  assert.equal(BUTTON_MAP[5], undefined);
});

test("SCANCODES: representative make codes + extended flags", () => {
  assert.deepEqual([...SCANCODES.Escape], [0x01, false]);
  assert.deepEqual([...SCANCODES.KeyA], [0x1e, false]);
  assert.deepEqual([...SCANCODES.Enter], [0x1c, false]);
  // Extended keys share base codes with numpad twins but set extended=true.
  assert.deepEqual([...SCANCODES.NumpadEnter], [0x1c, true]);
  assert.deepEqual([...SCANCODES.ArrowUp], [0x48, true]);
  assert.deepEqual([...SCANCODES.Numpad8], [0x48, false]);
  // Korean specials.
  assert.deepEqual([...SCANCODES.Lang1], [0x72, false]);
  assert.deepEqual([...SCANCODES.Lang2], [0x71, false]);
  // Unknown code is absent.
  assert.equal(SCANCODES.NoSuchKey, undefined);
});

test("encodeKey end-to-end via SCANCODES table entry", () => {
  // Simulate what renderer does for ArrowUp keydown: [scan, extended] from table.
  const [scan, ext] = SCANCODES.ArrowUp;
  assert.deepEqual(bytes(encodeKey(scan, true, ext)), [0x03, 0x48, 0x00, 0x03]);
});
