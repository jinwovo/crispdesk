# CLAUDE.md — crispdesk orientation

Orientation for contributors and AI agents working in this repo. Read this first,
then **[PROTOCOL.md](./PROTOCOL.md)** — that file is the **wire source of truth**
(signaling JSON, the input/clipboard binary frames, media payload types). When this
doc and PROTOCOL.md disagree about a byte layout or message field, PROTOCOL.md wins.

**crispdesk** (internal crate/dir prefix `rcd`) is a one-app, zero-popup WebRTC
remote desktop: Chrome-Remote-Desktop accessibility (pair with a code, no port
forwarding) with Moonlight-class image quality (hardware encode/decode, low latency).
Working Milestone 1 — the full loop runs end-to-end over LAN and Tailscale today.

---

## The three components

| Dir          | Stack                       | Role in WebRTC | What it does |
| ------------ | --------------------------- | -------------- | ------------ |
| `rcd-host/`  | Rust + GStreamer `webrtcbin`| **OFFERER**    | Captures the screen, hardware-encodes H.264 (+ Opus system audio), sends media, **receives** input and injects it. |
| `rcd-signal/`| Node.js + TypeScript + `ws` | (relay)        | WebSocket relay: rooms keyed by pairing code, max 2 peers, forwards offer/answer/ICE **verbatim**. Also serves a browser test client at `GET /`. |
| `rcd-client/`| Electron + TypeScript       | **ANSWERER**   | Renders host video fullscreen, plays audio, **sends** mouse/keyboard/wheel + clipboard back. |

Normative direction (do not flip): **HOST offers, CLIENT answers.** The host sends
video and receives input; the client shows video and sends input.

### Key source files

- `rcd-host/src/main.rs` — entry + arg dispatch (`probe`/`preview`/`stream`), DPI awareness, logging.
- `rcd-host/src/probe.rs` — runtime encoder + capture ladders (the "universal HW encode").
- `rcd-host/src/pipeline.rs` — the capture→encode→pay GStreamer chain + aspect-fit math.
- `rcd-host/src/webrtc.rs` — STREAM mode: webrtcbin negotiation, DataChannels, input decode,
  `HostCmd` marshalling (GLib thread → main loop) for monitor-switch session rebuilds.
- `rcd-host/src/signaling.rs` — WS client, the `SignalMessage` enum (normative field names).
- `rcd-host/src/input.rs` — Win32 `SendInput`/`SetCursorPos` injection + stuck-key backstop.
- `rcd-host/src/abr.rs` — adaptive bitrate (AIMD on RTCP loss) + live client-set ceiling.
- `rcd-host/src/clipboard.rs` — host-side clipboard poller + bidi sync.
- `rcd-host/src/control.rs` — the JSON `"control"` channel (hello/stats/restart ↔ switch-monitor/set-bitrate).
- `rcd-host/src/files.rs` — the `"file"` channel receive path (sanitize, size cap, `.part`+rename).
- `rcd-host/src/sendchan.rs` — shared `ChannelSlot` (one `unsafe impl Send/Sync` for the DataChannel send-handle, reused by clipboard/control/files).
- `rcd-host/src/env.rs` — env-var helpers (`on("VAR")` for the `VAR=0` disable convention, `parse_or`).
- `rcd-host/src/monitors.rs` — multi-monitor enumeration + runtime-mutable selection (`MONITOR`, `switch-monitor`).
- `rcd-signal/src/server.ts` — the relay (HTTP + WS), rooms, dynamic pairing, ICE-server minting.
- `rcd-signal/src/pairing.ts` — pure helpers (code gen, TURN creds, rate limit, origin allowlist); unit-tested.
- `rcd-client/src/renderer/renderer.ts` — the answerer; all WebRTC/signaling/input lives here.
- `rcd-client/src/main.ts` + `preload.ts` — thin Electron shell + clipboard IPC bridge.

The Electron **renderer uses only browser APIs**, so `rcd-signal` serves the
*compiled* `renderer.js` verbatim at `GET /renderer.js` to power the no-install
browser test client — keep it browser-API-only.

---

## Negotiation flow (signaling)

Transport: WebSocket, endpoint path **exactly** `/ws` → `ws://<server>:8080/ws`.
Every message is one JSON object with a string `type`. The server relays
`offer`/`answer`/`ice` verbatim; it never parses/rewrites SDP.

1. Both peers `join` the room (`{type:"join", room:<code>, role:"host"|"client"}`).
2. Host learns the client is present — either `peer-joined{role:"client"}` **or** its
   own `joined` reports `peers == 2` (`webrtc.rs::handle_signal` handles both).
3. Host (offerer): `create-offer` → `set-local-description` → send `offer`.
4. Client: `setRemoteDescription` → `createAnswer` → send `answer`.
5. Host: `set-remote-description(answer)`.
6. ICE candidates **trickle in both directions** as `ice` messages, relayed.

### Dynamic pairing codes (default)

When a HOST joins, the server **mints a random `CODE_LENGTH`-char code** (default 8,
over an unambiguous alphabet that omits 0/O/1/I/L), binds it to the host's room, and
returns it via `code-assigned` (with a TTL). The host **logs the code prominently**;
the user types it into the client, which `join`s with it (case-insensitive). Client
joins are **per-IP rate-limited**. Set `DEV_MODE=true` on the server for a fixed
`PAIRING_CODE` (default `"123456"`) instead — convenient for local testing.

### ICE servers / TURN

Right after a peer joins, the server sends `ice-servers` (STUN + optionally TURN with
**ephemeral** coturn `use-auth-secret` credentials). The host applies them to
`webrtcbin` (`stun-server` / `add-turn-server`); the client passes them to
`RTCPeerConnection`. Without TURN configured, peers fall back to public STUN — i.e.
**LAN / Tailscale only** until a coturn relay is stood up (M1b is code-wired but
needs a server, which is explicitly out of scope for code-only work here).

---

## DataChannels

All channels are **created by the HOST (offerer)** before negotiation; the client
receives them via `pc.ondatachannel`. See PROTOCOL.md §2 for exact byte layouts.

| Label       | Options                            | Reliability        | Direction        | Payload |
| ----------- | ---------------------------------- | ------------------ | ---------------- | ------- |
| `"input"`   | `ordered:false, max-retransmits:0` | unreliable/unordered (newest wins) | client → host | opcodes `0x01`–`0x04` |
| `"clipboard"`| `ordered:true`                    | reliable + ordered | bidirectional    | opcode `0x06` |
| `"control"` | `ordered:true`                     | reliable + ordered | bidirectional    | **JSON text** (PROTOCOL.md §2.5) |
| `"file"`    | `ordered:true` (absent if `FILES=0`)| reliable + ordered | client → host (M1) | opcodes `0x10`–`0x14` (§2.6) |

**Input opcodes** (all multi-byte fields little-endian):
- `0x01 MOUSE_MOVE_ABS` (9 B): `[f32 x][f32 y]` normalized `0..1` within the video **content** rect.
- `0x02 MOUSE_BUTTON` (3 B): `[u8 button 0=L 1=R 2=M 3=X1 4=X2][u8 down/up]`.
- `0x03 KEY` (4 B): `[u16 PS/2 Set-1 scancode][u8 flags bit0=down bit1=extended]` — layout-independent (client maps `KeyboardEvent.code`).
- `0x04 WHEEL` (5 B): `[i16 wheelY][i16 wheelX]` in WHEEL_DELTA units (`+120`/notch).
- `0x05` reserved (gamepad). `0x06 CLIPBOARD_TEXT` is on the **`clipboard`** channel only: `[u8 0x06][u32 len LE][utf8]`.

Clipboard sync has **echo-loop prevention** on both sides (remember the last text
synced in either direction; ignore an inbound/poll value equal to it).

**Control channel** (JSON; kebab-case `type`, camelCase fields; unknown types ignored):
host → client `hello` (monitor list/encoder/capabilities on open), `stats` (~1 Hz),
`restart` (session rebuild incoming — client resets and answers the fresh offer),
`error`; client → host `switch-monitor {index}`, `set-bitrate {kbps}` (0 = default;
persists across rebuilds). A **monitor switch is a session rebuild** over the live
signaling socket: the client detects the new session by the `restart` message OR the
offer's changed DTLS fingerprint and answers on a **fresh** RTCPeerConnection.

**File channel**: `FILE_OFFER {id,size,name}` → `FILE_ACCEPT`/`FILE_REJECT` →
`FILE_CHUNK`* (≤16 KiB, offsets implicit on the ordered channel) → `FILE_DONE`,
acked by a receiver-side `FILE_DONE` after size verification + `.part`→final rename.
Host sanitizes filenames, caps size (`FILE_MAX_BYTES`), and respects the consent gate.

---

## Host media pipeline

Built fresh **per client session** (a webrtcbin can't renegotiate a torn-down
session, so the client may refresh/reconnect freely). The runtime-chosen encoder +
capture (see "Probe") splice into:

```
<capture> ! <convert+scale to system NV12, aspect-fit>
  ! <encoder> name=venc <tuning>
  ! h264parse config-interval=-1
  ! rtph264pay aggregate-mode=zero-latency pt=96 mtu=1200
  ! application/x-rtp,media=video,encoding-name=H264,payload=96
  ! webrtcbin name=webrtcbin   (bundle-policy=max-bundle)
```

Audio (additive — never breaks the video path; gated by `AUDIO`):
```
wasapi2src loopback=true low-latency=true ! audioconvert ! audioresample
  ! audio/x-raw,rate=48000,channels=2 ! opusenc ! rtpopuspay pt=97 ! webrtcbin.
```

- **Capture**: `d3d11screencapturesrc` (WGC, then DXGI) with `monitor-handle=<HMONITOR>`
  to pin the selected monitor; GDI software capture is the last resort. d3d11 frames
  are scaled **on the GPU** (`d3d11convert`) then downloaded to system NV12.
- **Encoder**: probed ladder NVENC → QSV → AMF → `mfh264enc` (the only HW path for
  Snapdragon/Adreno) → `openh264enc` (license-clean SW fallback) → `x264enc` (GPL, off
  by default). The `venc` name lets `abr.rs` retune bitrate live.
- **Aspect-fit, no letterbox**: encode dims fit *within* the `RES` cap preserving the
  desktop's aspect ratio (even dims, never upscale) so the encoded frame **is** the
  desktop with no black bars. See `pipeline::fit_within` (unit-tested). Baked-in bars
  would offset every normalized mouse coordinate and make clicks miss.
- **ABR** (`abr.rs`): ~1 Hz AIMD on RTCP loss — loss >5% cut to 85%, loss <2% add
  500 kbps, clamped to `[BITRATE_MIN .. BITRATE]`. `ABR=0` pins a fixed bitrate.
  The client can move the **ceiling** live (`set-bitrate` control message); the
  override persists across session rebuilds. Each tick's stats also feed the
  client HUD via the control channel (`stats`).

---

## KEY GOTCHAS (these bit us — do not regress)

- **`rtph264pay mtu=1200` is REQUIRED, not optional.** GStreamer defaults to 1400,
  which IP-fragments large keyframes; a single lost fragment over a WireGuard/Tailscale
  tunnel (MTU 1280) means the browser can **never** assemble a keyframe → fully-connected
  transport, permanent **black screen**, climbing `pliCount`. Leave `RTP_MTU=1200`.
- **`bundle-policy=max-bundle`.** Forces video + audio + DataChannel onto **one** ICE
  transport. With the default policy the data transport can connect while a separate
  video transport fails → black screen with working input.
- **Aspect-fit, not letterbox.** (See pipeline above.) Black bars break mouse mapping.
- **Per-monitor DPI awareness V2** is set in `main.rs` **before** GStreamer/any window
  or `GetSystemMetrics` call. Without it, on a scaled display Win32 returns logical
  pixels while d3d11 capture delivers physical pixels — the mismatch offsets both the
  encode size and the injected cursor.
- **webrtcbin must be at least `READY` before `create-data-channel`** — in `NULL` it
  returns `None` and input silently never works. `build_pipeline` brings it to READY,
  creates both channels, then `start_session` goes PLAYING (which fires
  `on-negotiation-needed`).
- **`mfh264enc` needs `gop-size=30`** (1s GOP). Otherwise it may emit only one IDR at
  the start; lose part of that single keyframe on a real network and it's a permanent
  black screen. `bframes=0` + `low-latency=true` keep frames small and in decode order.
- **`Stop-Process` the running `rcd-host` before `cargo build`** — the linker can't
  overwrite a locked `rcd-host.exe`.
- **Build env must match the toolchain.** This dev machine is ARM64 (Snapdragon X) and
  is pinned to `stable-aarch64-pc-windows-msvc` (`rust-toolchain.toml`) against the
  **native ARM64** GStreamer — the only path to the Adreno HW encoder via Media
  Foundation (emulated x64 can't reach it). Before `cargo build`:
  - add `C:\gstreamer\1.0\msvc_arm64\bin` to `PATH`,
  - set `PKG_CONFIG_PATH=C:\gstreamer\1.0\msvc_arm64\lib\pkgconfig`,
  - have the MSVC ARM64 C++ build tools installed (else linking fails).
  On a normal x86_64 GPU PC: switch the toolchain to `stable-x86_64-pc-windows-msvc`
  and install the x86_64 MSVC GStreamer. **Never** use a `-gnu` toolchain — Windows
  GStreamer is MSVC-ABI and won't link.
- **GStreamer is not bundled** — the host won't build or run without an MSVC GStreamer
  1.24+ install (the repo targets 1.28 ARM64). gstreamer-rs is pinned to the `0.23`
  family (needs GStreamer ≥ 1.24); the `webrtcbin` signal/promise API is the most
  version-sensitive surface.
- **Signaling field names are normative across all three components.** `sdpMid` /
  `sdpMLineIndex` are camelCase on the wire even though the rest is kebab-case. The
  host serializes them via explicit `#[serde(rename = ...)]`. Don't "tidy" them.
- **Stuck-key safety is two-sided.** The client releases held keys on window blur /
  channel close; the host (`input::release_all`) is the backstop when the client
  vanishes without sending key-ups (crash/network drop).

---

## Environment-variable surface

### Host (`rcd-host`)
| Var | Default | Effect |
| --- | --- | --- |
| `RES` | `1920x1080` | Encode resolution cap `WxH`, or `native` (full desktop). |
| `BITRATE` | `12000` | Encoder/ABR ceiling (kbps). |
| `BITRATE_MIN` | `1500` | ABR floor (kbps). |
| `ABR` | on | `ABR=0` pins a fixed bitrate. |
| `MONITOR` | primary | Monitor index to capture + control (enumerated at startup). |
| `AUDIO` | on | `AUDIO=0` disables system-audio capture. |
| `CLIPBOARD` | on | `CLIPBOARD=0` disables clipboard sync. |
| `FILES` | on | `FILES=0` disables the file-transfer channel. |
| `FILE_DIR` | `%USERPROFILE%\Downloads` | Where received files are saved. |
| `FILE_MAX_BYTES` | 2 GiB | Per-file size cap for received files. |
| `ENCODER` | (auto-probe) | Force an encoder element, skip the probe (e.g. `x264enc`). |
| `ALLOW_GPL` | off | `ALLOW_GPL=1` lets the probe auto-select the GPL `x264enc`. |
| `CAPTURE` | (auto-probe) | Force a capture source, skip the probe. |
| `RTP_MTU` | `1200` | RTP packet size — **leave at 1200** (see gotchas). |
| `SIGNAL_URL` | `ws://127.0.0.1:8080/ws` | Signaling server URL (path must be `/ws`). |
| `PAIRING_CODE` | `123456` | Fixed code — **only** when the server runs `DEV_MODE=true`. |
| `STUN` | `stun://stun.l.google.com:19302` | STUN URI (webrtcbin wants the `stun://` scheme). |
| `TURN` | (unset) | Optional `turn://user:pass@host:3478` (overridden by server `ice-servers`). |
| `RUST_LOG` | `rcd_host=info,warn` | Rust log filter. |
| `GST_DEBUG` | (unset) | GStreamer log level (e.g. `3`, `webrtcbin:5`). |

### Signaling (`rcd-signal`)
| Var | Default | Effect |
| --- | --- | --- |
| `PORT` | `8080` | Listen port (WS path is always `/ws`). |
| `DEV_MODE` | off | `true` = fixed `PAIRING_CODE`; else dynamic codes. |
| `PAIRING_CODE` | `123456` | Fixed code (DEV_MODE only). |
| `CODE_LENGTH` | `8` | Dynamic code length (~40 bits of entropy at 8). |
| `CODE_TTL_MS` | `300000` | Dynamic code lifetime (5 min). |
| `JOIN_MAX_ATTEMPTS` / `JOIN_WINDOW_MS` | `10` / `60000` | Per-IP client-join rate limit. |
| `DISABLE_TEST_PAGE` | off | `true` gates the unauthenticated browser client (`GET /`). |
| `STUN_URL` | `stun:stun.l.google.com:19302` | STUN entry pushed via `ice-servers`. |
| `TURN_URLS` | (unset) | Comma-separated `turn:`/`turns:` URLs. Needs `TURN_SECRET` to activate TURN. |
| `TURN_SECRET` | (unset) | coturn `static-auth-secret` for ephemeral-cred minting. |
| `TURN_TTL_SEC` | `3600` | Ephemeral TURN credential lifetime. |
| `ALLOWED_ORIGINS` | (unset) | Comma-separated WS-upgrade Origin allowlist (empty = allow all; native host sends no Origin and is always allowed). |

---

## How to run

Three terminals. **Signaling and client need only Node/npm; the host needs the
GStreamer build env above.**

```powershell
# 1) Signaling (rcd-signal)
npm install
npm run dev          # ws://0.0.0.0:8080/ws ; browser client at http://<ip>:8080/
# DEV_MODE=true for a fixed PAIRING_CODE; default = dynamic per-host codes (shown in host log)

# 2) Host (rcd-host) — set PATH/PKG_CONFIG_PATH first (see gotchas)
cargo run -- probe     # print THIS machine's encoder + capture reality (no streaming)
cargo run -- preview   # local-only window: proves capture + HW encode + decode (no network)
cargo run -- stream    # join signaling, offer WebRTC, stream video + receive input

# 3) Client (rcd-client)
npm install
npm start              # Electron UI: Signaling URL, Pairing Code, Connect
# ...or open http://<host-ip>:8080/ in any LAN browser (no install) and press Connect
```

`probe` → `preview` → `stream` is the host bring-up order: probe shows what works,
preview confirms the capture+encode path with zero networking, stream goes live.

---

## Tests & CI

| Component | Command (in its dir) | Covers |
| --- | --- | --- |
| `rcd-signal` | `npm test` | `test/pairing.test.mjs` — pairing-code gen, TURN credentials, rate limit, origin allowlist (`src/pairing.ts` is pure + injectable). Also a `smoke-test.mjs`. |
| `rcd-host` | `cargo test` | inline `#[cfg(test)]` modules — aspect-fit math (`pipeline.rs`), TURN URI percent-encoding (`webrtc.rs`), ABR AIMD (`abr.rs`), clipboard codec, file-frame codec + filename sanitize (`files.rs`), control JSON shapes (`control.rs`). |
| `rcd-client` | `npm test` | `test/wire.test.mjs` — input/clipboard/file frame layouts, control-JSON parse/builders, button/scancode maps (against the compiled `dist/renderer/wire.js`). |

CI (`.github/workflows/ci.yml`) on every push/PR: the Node side (signal + client) on
Ubuntu, the Rust host on a **native ARM64 Windows** runner (`windows-11-arm`) that
installs the ARM64 MSVC GStreamer and runs `cargo build --locked` + `cargo test --locked`.

---

## Conventions & guardrails for changes

- **PROTOCOL.md is the wire contract.** Any change to a signaling message, a
  DataChannel opcode/byte layout, or a media payload type must update PROTOCOL.md and
  all three components together. Prefer additive, backward-compatible changes — the
  server already ignores unknown message types for forward-compat.
- Keep the renderer **browser-API-only** (it doubles as the served browser client).
- Audio and clipboard are **additive**: a missing element or `AUDIO=0`/`CLIPBOARD=0`
  must never break the video or input path.
- Encoder **tuning strings in `probe.rs` are version-sensitive starting guesses** —
  the probe validates encoders with **no** tuning props, so a wrong prop name can't
  hide a working encoder; verify real tuning with `gst-inspect-1.0 <element>`.
- M1 is H.264 4:2:0, payload type 96 (video) / 97 (audio). HEVC/4:4:4/AV1 are later.
</content>
</invoke>
