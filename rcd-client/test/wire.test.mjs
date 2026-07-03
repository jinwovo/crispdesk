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
  OPCODE_FILE_OFFER,
  OPCODE_FILE_ACCEPT,
  OPCODE_FILE_REJECT,
  OPCODE_FILE_CHUNK,
  OPCODE_FILE_DONE,
  encodeMouseMoveAbs,
  encodeMouseButton,
  encodeKey,
  encodeWheel,
  encodeClipboard,
  decodeClipboard,
  encodeFileOffer,
  encodeFileChunk,
  encodeFileDone,
  encodeFileReject,
  decodeFileReply,
  parseControlMessage,
  controlSwitchMonitor,
  controlSetBitrate,
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

// ---------------------------------------------------------------------------
// "file" channel frames (must mirror rcd-host's files.rs decoder EXACTLY).
// ---------------------------------------------------------------------------

test("file opcode constants have the pinned values", () => {
  assert.equal(OPCODE_FILE_OFFER, 0x10);
  assert.equal(OPCODE_FILE_ACCEPT, 0x11);
  assert.equal(OPCODE_FILE_REJECT, 0x12);
  assert.equal(OPCODE_FILE_CHUNK, 0x13);
  assert.equal(OPCODE_FILE_DONE, 0x14);
});

test("encodeFileOffer: [0x10][u32 id][u64 size][u16 nameLen][utf8 name], LE", () => {
  const buf = encodeFileOffer(7, 0x1_0000_0001, "a.txt"); // size spans >32 bits
  const dv = new DataView(buf);
  assert.equal(dv.getUint8(0), OPCODE_FILE_OFFER);
  assert.equal(dv.getUint32(1, true), 7);
  assert.equal(dv.getBigUint64(5, true), 0x1_0000_0001n);
  assert.equal(dv.getUint16(13, true), 5);
  assert.equal(buf.byteLength, 15 + 5);
  // Name length counts UTF-8 BYTES (Korean chars are 3 bytes each).
  const kr = encodeFileOffer(1, 10, "한글.txt");
  assert.equal(new DataView(kr).getUint16(13, true), 10); // 3+3+4
});

test("encodeFileChunk / encodeFileDone layouts", () => {
  const chunk = encodeFileChunk(9, new Uint8Array([1, 2, 3]));
  assert.deepEqual(bytes(chunk), [0x13, 0x09, 0x00, 0x00, 0x00, 1, 2, 3]);
  assert.deepEqual(bytes(encodeFileDone(4)), [0x14, 0x04, 0x00, 0x00, 0x00]);
});

test("decodeFileReply: accept / done / reject roundtrip, malformed -> null", () => {
  // ACCEPT and DONE are 5-byte [op][u32 id] frames — build via encode helpers
  // where available (DONE) and by hand (ACCEPT is host->client only).
  const accept = new Uint8Array([0x11, 0x2a, 0x00, 0x00, 0x00]).buffer;
  assert.deepEqual(decodeFileReply(accept), { kind: "accept", id: 42 });
  assert.deepEqual(decodeFileReply(encodeFileDone(3)), { kind: "done", id: 3 });
  assert.deepEqual(decodeFileReply(encodeFileReject(5, "too large")), {
    kind: "reject",
    id: 5,
    reason: "too large",
  });
  // Unknown opcode, short frames, truncated reason -> null.
  assert.equal(decodeFileReply(new Uint8Array([0xee, 0, 0, 0, 0]).buffer), null);
  assert.equal(decodeFileReply(new ArrayBuffer(4)), null);
  const truncated = new Uint8Array([0x12, 1, 0, 0, 0, 50, 0, 0x61]).buffer; // claims len=50
  assert.equal(decodeFileReply(truncated), null);
});

// ---------------------------------------------------------------------------
// "control" channel JSON (must mirror rcd-host's control.rs serde shapes).
// ---------------------------------------------------------------------------

test("parseControlMessage accepts the pinned host shapes", () => {
  const hello = parseControlMessage(
    JSON.stringify({
      type: "hello",
      monitors: [
        { index: 0, width: 2880, height: 1800, left: 0, top: 0, primary: true, current: true },
      ],
      encoder: "mfh264enc",
      fileTransfer: true,
      abr: { floorKbps: 1500, ceilingKbps: 12000, adaptive: true },
    }),
  );
  assert.equal(hello?.type, "hello");
  assert.equal(hello?.monitors[0].width, 2880);
  assert.equal(hello?.fileTransfer, true);

  const stats = parseControlMessage('{"type":"stats","encoderKbps":8500,"lossPct":1.2,"rttMs":12}');
  assert.deepEqual(stats, { type: "stats", encoderKbps: 8500, lossPct: 1.2, rttMs: 12 });

  assert.deepEqual(parseControlMessage('{"type":"restart","reason":"monitor-switch"}'), {
    type: "restart",
    reason: "monitor-switch",
  });
  assert.deepEqual(parseControlMessage('{"type":"error","message":"nope"}'), {
    type: "error",
    message: "nope",
  });
});

test("parseControlMessage rejects malformed/unknown without throwing", () => {
  assert.equal(parseControlMessage("not json"), null);
  assert.equal(parseControlMessage('{"type":"future-thing"}'), null); // forward-compat
  assert.equal(parseControlMessage('{"type":"hello","monitors":"nope"}'), null);
  assert.equal(parseControlMessage('{"type":"stats"}'), null);
  assert.equal(parseControlMessage("42"), null);
});

test("parseControlMessage normalizes a hello missing optional-ish fields", () => {
  // A hello from a different host build without fileTransfer/abr must not leave
  // undefined behind the non-optional types: fileTransfer defaults TRUE (the file
  // channel's existence is the authoritative gate) and abr gets safe zeros.
  const hello = parseControlMessage('{"type":"hello","monitors":[],"encoder":"x264enc"}');
  assert.equal(hello?.type, "hello");
  assert.equal(hello?.fileTransfer, true);
  assert.deepEqual(hello?.abr, { floorKbps: 0, ceilingKbps: 0, adaptive: true });
  // Explicit false is preserved; malformed abr is replaced by the default.
  const off = parseControlMessage(
    '{"type":"hello","monitors":[],"encoder":"x","fileTransfer":false,"abr":"nope"}',
  );
  assert.equal(off?.fileTransfer, false);
  assert.deepEqual(off?.abr, { floorKbps: 0, ceilingKbps: 0, adaptive: true });
});

test("client -> host control builders emit the pinned shapes", () => {
  assert.deepEqual(JSON.parse(controlSwitchMonitor(1)), { type: "switch-monitor", index: 1 });
  assert.deepEqual(JSON.parse(controlSetBitrate(8000)), { type: "set-bitrate", kbps: 8000 });
  assert.deepEqual(JSON.parse(controlSetBitrate(0)), { type: "set-bitrate", kbps: 0 });
});
