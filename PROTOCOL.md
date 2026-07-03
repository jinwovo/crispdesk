# rcd Protocol Specification

This document is the **source of truth** for the wire protocols used by `rcd`
(remote control desktop) in **Milestone 1 (M1)**. All three components —
`rcd-host` (Rust + GStreamer), `rcd-signal` (Node.js + TypeScript), and
`rcd-client` (Electron + TypeScript) — MUST conform to the field names, byte
layouts, and directions described here **exactly**.

There are two protocol families:

1. The **signaling protocol** — JSON messages over a WebSocket, used to broker a
   WebRTC connection between a host and a client.
2. The **DataChannel protocols** — what flows over the host-created WebRTC
   DataChannels: binary **input** events (§2), binary **clipboard** sync (§2.4),
   JSON **control** messages (§2.5), and binary **file transfer** (§2.6).

---

## 1. Signaling Protocol

### 1.1 Transport

- **Transport:** WebSocket.
- **Endpoint:** the server listens on `PORT` (env, default `8080`) and serves the
  endpoint path **exactly** `/ws`.
- **URL form:** `ws://<server>:8080/ws`
- **Encoding:** every message is a single JSON object with a string `"type"`
  field that discriminates the message. Field names below are **normative**.

### 1.2 Rooms & roles

- A **room** is keyed by the **pairing code**.
- A room holds at most **2 peers**: one `host` and one `client`.
- On `join`, the server validates the pairing code.
  - **M1 STUB:** the code is compared to env `PAIRING_CODE` (default `"123456"`).
    A wrong code yields an `error` message and the socket is closed.
  - `TODO(M4)`: real account / pairing authentication.
- The server **never trusts client-claimed identity beyond `role`**. When
  relaying, the target is always resolved as *"the other peer in my room."*

### 1.3 Message reference

#### client/host -> server

| type   | fields                                            | meaning                                   |
| ------ | ------------------------------------------------- | ----------------------------------------- |
| `join` | `room: string`, `role: "host" \| "client"`        | Join (or create) the room `room` as `role`. |

```json
{ "type": "join", "room": "<pairingCode>", "role": "host" }
{ "type": "join", "room": "<pairingCode>", "role": "client" }
```

#### server -> peer

| type          | fields                                                      | meaning                                                     |
| ------------- | ----------------------------------------------------------- | ----------------------------------------------------------- |
| `joined`      | `role: "host" \| "client"`, `peers: int`                    | Ack to the joiner. `peers` = number currently in the room.  |
| `peer-joined` | `role: "host" \| "client"`                                  | Sent to the OTHER peer when someone joins. `role` is theirs.|
| `peer-left`   | `role: "host" \| "client"`                                  | Sent to the OTHER peer when someone leaves. `role` is theirs.|
| `error`       | `message: string`                                           | Bad pairing code / full room. Socket is then closed.        |
| `code-assigned` | `code: string`, `expiresAt: int` (unix ms)                | **Dynamic pairing:** server → HOST after it joins; the host shows this code for a client to enter. |
| `ice-servers`  | `iceServers: [{ urls: string[], username?: string, credential?: string }]` | Server → peer right after it joins. STUN + (when configured) TURN with **ephemeral** credentials. The host applies them to `webrtcbin` (`stun-server` / `add-turn-server`); the client passes them to `RTCPeerConnection`. |

**Pairing (dynamic, default):** when a HOST joins, the server mints a random
`CODE_LENGTH`-char code (unambiguous alphabet) with a TTL (`CODE_TTL_MS`, default
5 min), binds it to the host's room, and returns it via `code-assigned`. A CLIENT
must `join` with that code (case-insensitive). Client joins are per-IP rate-limited
(`JOIN_MAX_ATTEMPTS`/`JOIN_WINDOW_MS`). Set `DEV_MODE=true` on the server to instead
use a fixed `PAIRING_CODE` (default `"123456"`) for local testing.

```json
{ "type": "joined",      "role": "host",   "peers": 2 }
{ "type": "peer-joined", "role": "client" }
{ "type": "peer-left",   "role": "client" }
{ "type": "error",       "message": "invalid pairing code" }
```

#### relayed peer <-> peer (forwarded verbatim to the other peer)

| type     | fields                                                                          | direction        |
| -------- | ------------------------------------------------------------------------------- | ---------------- |
| `offer`  | `sdp: string` (full SDP)                                                         | host -> client   |
| `answer` | `sdp: string` (full SDP)                                                         | client -> host   |
| `ice`    | `candidate: string`, `sdpMid: string \| null`, `sdpMLineIndex: int \| null`     | either -> other  |

```json
{ "type": "offer",  "sdp": "<full SDP string>" }
{ "type": "answer", "sdp": "<full SDP string>" }
{ "type": "ice",    "candidate": "<candidate string>", "sdpMid": "0", "sdpMLineIndex": 0 }
```

The signaling server relays `offer` / `answer` / `ice` to the other peer in the
same room **verbatim**; it does not parse or rewrite SDP or candidates.

### 1.4 Negotiation direction (NORMATIVE)

- **HOST is the OFFERER.** It sends video and receives input.
- **CLIENT is the ANSWERER.** It shows video and sends input.

Flow:

1. Both peers `join` the room.
2. The host learns the client is present — either it receives `peer-joined`
   with `role: "client"`, **or** its own `joined` reports `peers == 2`.
3. The host starts WebRTC negotiation (`webrtcbin` `create-offer`).
4. Host sends `{"type":"offer"}`.
5. Client `setRemoteDescription(offer)`, `createAnswer`, sends `{"type":"answer"}`.
6. Host `setRemoteDescription(answer)`.
7. ICE candidates **trickle in BOTH directions** as `{"type":"ice"}` as they are
   gathered.

### 1.5 Sequence diagram (join -> media + input)

```
   HOST (offerer)            SIGNAL server            CLIENT (answerer)
        |                         |                         |
        |---- join(host) -------->|                         |
        |<--- joined(peers=1) ----|                         |
        |                         |<------ join(client) ----|
        |                         |--- joined(peers=2) ---->|
        |<-- peer-joined(client) -|                         |
        |  (host now knows client is present -> negotiate)  |
        |                         |                         |
        |------ offer(sdp) ------>|------ offer(sdp) ------->|
        |                         |                         | setRemoteDescription
        |                         |                         | createAnswer
        |<----- answer(sdp) ------|<----- answer(sdp) ------|
        | setRemoteDescription    |                         |
        |                         |                         |
        |<====== ice trickle (both directions, relayed) ===>|
        |                         |                         |
        |================ ICE connectivity check ===========|
        |                         |                         |
        |======== H.264 video (host -> client) ============>|   (media track)
        |<======= "input" DataChannel (client -> host) =====|   (mouse, etc.)
        v                         v                         v
```

---

## 2. Input DataChannel Protocol

### 2.1 Channel

- The **HOST (offerer)** creates a DataChannel labeled **exactly** `"input"`
  with options `{ ordered: false, maxRetransmits: 0 }` (unreliable / unordered —
  newest sample wins, drops are acceptable for live input).
- The **CLIENT** receives this channel via `pc.ondatachannel` and **sends** input
  over it.
- The **HOST** receives and decodes the input.

### 2.2 Wire format

- All multi-byte numbers are **little-endian**.
- Each message is a self-contained binary frame whose first byte is the
  **opcode**.
- Implemented messages: `0x01 MOUSE_MOVE_ABS`, `0x02 MOUSE_BUTTON`, `0x03 KEY`,
  `0x04 WHEEL` (`0x05+` reserved).

#### Opcode `0x01` — `MOUSE_MOVE_ABS` (9 bytes)

| offset (bytes) | size | type    | field  | meaning                                                    |
| -------------- | ---- | ------- | ------ | ---------------------------------------------------------- |
| `0`            | 1    | u8      | opcode | `0x01` = `MOUSE_MOVE_ABS`                                   |
| `1 .. 5`       | 4    | Float32 | `x`    | normalized `0.0 .. 1.0` — mouse X within the video content |
| `5 .. 9`       | 4    | Float32 | `y`    | normalized `0.0 .. 1.0` — mouse Y within the video content |

```
byte:   0    1   2   3   4    5   6   7   8
      +----+----+---+---+---++----+---+---+---+
      |0x01|     f32 x (LE)  ||     f32 y (LE) |
      +----+----+---+---+---++----+---+---+---+
       op   <--- bytes 1..5 -><-- bytes 5..9 ->
```

- `x` / `y` are **normalized** to the displayed video **content rectangle**
  (`0.0` = left/top edge, `1.0` = right/bottom edge). The client MUST account for
  letterboxing (`object-fit: contain`) so that pixels in the black bars map
  outside `0..1` (and may be clamped or dropped).
- The client sends one frame per `mousemove`, **throttled to an animation frame**.
- **Host:** decodes and **injects** the move on the primary monitor
  (`SetCursorPos`; multi-monitor via `SendInput` + `MOUSEEVENTF_VIRTUALDESK` is a
  `TODO`).

#### Opcode `0x02` — `MOUSE_BUTTON` (3 bytes)

| offset | size | type | field  | meaning                                              |
| ------ | ---- | ---- | ------ | ---------------------------------------------------- |
| `0`    | 1    | u8   | opcode | `0x02`                                                |
| `1`    | 1    | u8   | button | `0`=left `1`=right `2`=middle `3`=back(X1) `4`=fwd(X2) |
| `2`    | 1    | u8   | state  | `1`=down `0`=up                                       |

Note: JS `MouseEvent.button` numbering differs (`1`=middle, `2`=right) — the
client maps it (`BUTTON_MAP`). Host injects via `SendInput` (`MOUSEEVENTF_*`).

#### Opcode `0x03` — `KEY` (4 bytes)

| offset   | size | type | field    | meaning                                          |
| -------- | ---- | ---- | -------- | ------------------------------------------------ |
| `0`      | 1    | u8   | opcode   | `0x03`                                            |
| `1 .. 3` | 2    | u16  | scancode | PS/2 **Set-1 make scancode** (little-endian)      |
| `3`      | 1    | u8   | flags    | bit0: `1`=down `0`=up · bit1: `1`=extended (0xE0) |

- The client maps the **physical key** (`KeyboardEvent.code`) to a scancode, so
  the mapping is keyboard-layout-independent; the HOST's layout/IME (incl. 한/영
  `Lang1`=0x72, 한자 `Lang2`=0x71) applies on the host side.
- Auto-repeat keydowns are forwarded as repeated down frames.
- Host injects via `SendInput` with `KEYEVENTF_SCANCODE` (+`KEYEVENTF_KEYUP` /
  `KEYEVENTF_EXTENDEDKEY`).
- Stuck-key safety: on window blur / channel close the client sends key-up for
  every key it believes is held.

#### Opcode `0x04` — `WHEEL` (5 bytes)

| offset   | size | type | field  | meaning                                            |
| -------- | ---- | ---- | ------ | -------------------------------------------------- |
| `0`      | 1    | u8   | opcode | `0x04`                                              |
| `1 .. 3` | 2    | i16  | wheelY | vertical, **WHEEL_DELTA units** (`+120` = one notch up/away) |
| `3 .. 5` | 2    | i16  | wheelX | horizontal, WHEEL_DELTA units (`+` = right)         |

The client converts browser `WheelEvent` deltas (pixels/lines) into WHEEL_DELTA
units and flips the vertical sign (JS `+`=down, Windows `+`=up). Host injects via
`MOUSEEVENTF_WHEEL` / `MOUSEEVENTF_HWHEEL`.

### 2.3 Reserved / other opcodes

| opcode | meaning                                       |
| ------ | --------------------------------------------- |
| `0x05` | gamepad state (reserved)                      |
| `0x06` | `CLIPBOARD_TEXT` — sent on the **`clipboard`** channel, NOT `input` (see §2.4) |
| `0x10`–`0x14` | file transfer — sent on the **`file`** channel, NOT `input` (see §2.6) |
| other  | future use                                    |

---

## 2.4 Clipboard DataChannel

Separate from `input`, the **HOST (offerer)** also creates a DataChannel labeled
**exactly** `"clipboard"` with options `{ ordered: true }` (**reliable + ordered** —
clipboard text must arrive intact, unlike the unreliable `input` channel). It is
**bidirectional**: client → host and host → client. Gated by the host `CLIPBOARD`
env (set `CLIPBOARD=0` to disable).

#### Opcode `0x06` — `CLIPBOARD_TEXT` (variable length)

| offset (bytes) | size | type   | field  | meaning                                   |
| -------------- | ---- | ------ | ------ | ----------------------------------------- |
| `0`            | 1    | u8     | opcode | `0x06` = `CLIPBOARD_TEXT`                  |
| `1 .. 5`       | 4    | u32    | length | byte length of UTF-8 text (little-endian) |
| `5 .. 5+len`   | var  | UTF-8  | text   | clipboard text (no null terminator)       |

- Text only in v1 (images are a TODO). Host limit `CLIPBOARD_MAX_BYTES` (default 100 KB).
- **Echo-loop prevention:** each side remembers the last text it synced in EITHER
  direction and ignores an inbound value (or a local poll result) equal to it.
- Host watches its clipboard by polling (`CLIPBOARD_POLL_MS`, default 500 ms); the
  client polls via the Electron clipboard bridge (or `navigator.clipboard` in a browser).

---

## 2.5 Control DataChannel

The **HOST (offerer)** also creates a DataChannel labeled **exactly** `"control"`
with options `{ ordered: true }` (**reliable + ordered**). Unlike every other
channel it carries **JSON TEXT messages** — one object per message with a string
`"type"` — because control traffic is low-rate and forward-extensibility matters
more than compactness. `type` values are kebab-case; field names are camelCase
(matching the signaling protocol's conventions). A receiver **MUST ignore unknown
`type`s** (forward compat), never error on them.

#### host -> client

| type      | fields | meaning |
| --------- | ------ | ------- |
| `hello`   | `monitors: [{index, width, height, left, top, primary, current}]`, `encoder: string`, `fileTransfer: bool`, `abr: {floorKbps, ceilingKbps, adaptive}` | Sent once when the channel opens: capability/config snapshot. `monitors[].index` uses the host's `MONITOR=` ordering (primary first); `current` flags the display being captured. |
| `stats`   | `encoderKbps: int`, `lossPct: number`, `rttMs: number` | ~1 Hz while streaming: the encoder bitrate in force plus RTCP loss/RTT as the host sees them (drives the client HUD). Sent even with `ABR=0`. |
| `restart` | `reason: string` | The host is about to **tear down and rebuild** the WebRTC session (e.g. `"monitor-switch"`). The client must reset its `RTCPeerConnection` and answer the fresh offer that follows; the signaling socket stays connected throughout. |
| `error`   | `message: string` | A rejected control request (bad monitor index, consent not granted, ...). |

```json
{"type":"hello","monitors":[{"index":0,"width":2880,"height":1800,"left":0,"top":0,"primary":true,"current":true}],"encoder":"mfh264enc","fileTransfer":true,"abr":{"floorKbps":1500,"ceilingKbps":12000,"adaptive":true}}
{"type":"stats","encoderKbps":8500,"lossPct":1.2,"rttMs":12}
{"type":"restart","reason":"monitor-switch"}
```

#### client -> host

| type             | fields       | meaning |
| ---------------- | ------------ | ------- |
| `switch-monitor` | `index: int` | Capture + control the given monitor. The host replies `restart` and rebuilds the session on the new display (or `error` for a bad index / missing consent). |
| `set-bitrate`    | `kbps: int`  | Move the encoder/ABR bitrate **ceiling** live (clamped host-side to `[500..100000]`). `0` restores the host default (`BITRATE`). The choice **persists across session rebuilds**. With ABR on, AIMD keeps adapting *below* the new ceiling; with `ABR=0` the value is applied directly. |

#### Session-rebuild flow (`switch-monitor`)

A `webrtcbin` session cannot swap its capture source live, so a monitor switch is
a **session rebuild** over the still-open signaling connection:

1. client sends `{"type":"switch-monitor","index":n}`;
2. host validates, sends `{"type":"restart","reason":"monitor-switch"}`, waits
   ~200 ms for the message to flush;
3. host tears the session down and builds a fresh pipeline + webrtcbin on the new
   monitor → a **new offer** arrives via signaling;
4. the client — already reset by `restart`, or detecting the offer's **changed DTLS
   fingerprint** (`a=fingerprint:`) if the restart message was lost — answers with a
   **fresh** `RTCPeerConnection` (an existing pc cannot adopt a new DTLS identity;
   a same-fingerprint re-offer is an ICE restart and stays on the existing pc).

---

## 2.6 File DataChannel

The **HOST (offerer)** creates a DataChannel labeled **exactly** `"file"` with
options `{ ordered: true }` (**reliable + ordered**), unless `FILES=0`. M1
direction: **client → host** uploads (drag & drop onto the client window; saved
under the host's Downloads). The frames are direction-agnostic so host → client
can be added later without a protocol break.

All multi-byte fields **little-endian**. Because the channel is reliable AND
ordered, chunks carry **no offsets** — the byte position is implicit.

| opcode | name          | direction            | layout |
| ------ | ------------- | -------------------- | ------ |
| `0x10` | `FILE_OFFER`  | sender -> receiver   | `[u8 0x10][u32 id][u64 size][u16 nameLen][utf8 name]` |
| `0x11` | `FILE_ACCEPT` | receiver -> sender   | `[u8 0x11][u32 id]` — start sending chunks |
| `0x12` | `FILE_REJECT` | either               | `[u8 0x12][u32 id][u16 len][utf8 reason]` — refusal, mid-transfer abort, or sender cancel |
| `0x13` | `FILE_CHUNK`  | sender -> receiver   | `[u8 0x13][u32 id][bytes...]` (the client sends ≤ 16 KiB of payload per frame) |
| `0x14` | `FILE_DONE`   | both                 | `[u8 0x14][u32 id]` — sender→receiver: *all bytes sent*; receiver→sender: *byte count verified + file saved* (the completion ack) |

- `id` is chosen by the sender, unique among its in-flight transfers.
- **Host-side safety:** the offered `name` is reduced to a sanitized basename
  (last path component; `<>:"/\|?*` and control chars replaced; trailing dots/
  spaces trimmed; reserved DOS device names defused) — a hostile client cannot
  traverse paths. `size` is capped by `FILE_MAX_BYTES` (default 2 GiB). Receipt is
  gated by the same consent gate as input/clipboard (`REQUIRE_CONSENT`). Data is
  written to a `.part` temp file and renamed (collision-free, ` (n)` suffix) only
  after the received byte count equals the offered `size`.
- **Backpressure:** the client pauses while `bufferedAmount` exceeds ~4 MiB and
  resumes at the 1 MiB `bufferedAmountLowThreshold`.
- Receiving more bytes than offered, a write failure, or a size mismatch at
  `FILE_DONE` aborts the transfer with `FILE_REJECT` and deletes the temp file.

---

## 3. Media (reference)

- **Video codec:** H.264, 4:2:0 (M1). HEVC, 4:4:4, and AV1 are intentionally
  **not** used in M1. Payload type **96**.
- **Audio codec:** Opus, 48 kHz stereo. Payload type **97**. The host captures
  **system output** via WASAPI loopback (`wasapi2src loopback=true`). Audio is
  **additive**: if the audio elements are absent or `AUDIO=0` is set, the host
  streams **video-only** — audio can never break the video path.
- **Host pipeline tail (video):**
  `... -> h264parse config-interval=-1 -> rtph264pay aggregate-mode=zero-latency pt=96 mtu=1200 -> webrtcbin`.
  - **`mtu=1200`** is REQUIRED, not optional: it is the libwebrtc-standard RTP
    packet size and keeps keyframe fragments under the smallest common path MTU
    (e.g. a WireGuard/Tailscale tunnel is 1280). The GStreamer default of 1400
    fragments large keyframes at the IP layer; a single lost fragment then
    prevents the browser from ever assembling the keyframe — a fully-connected
    transport with a permanently **black screen** and a climbing `pliCount`.
- **Host pipeline tail (audio, when enabled):**
  `wasapi2src loopback=true low-latency=true -> audioconvert -> audioresample -> opusenc -> rtpopuspay pt=97 -> webrtcbin`.
- **Resolution:** the encoded video is scaled (on the GPU for d3d11 capture) to a
  cap — default **1920x1080**; `RES=native` streams the full desktop, `RES=WxH`
  sets an explicit cap. `BITRATE=<kbps>` tunes the encoder (default 12000).
- **Transport:** `webrtcbin` uses **`bundle-policy=max-bundle`** so video, audio,
  and the DataChannel share **one** ICE transport (the default policy can connect
  the data transport while a separate video transport fails). A fresh
  pipeline+webrtcbin is built **per client session** so a client may
  refresh/reconnect freely.
- **Client:** receives each track via `pc.ontrack`, adds it to one `MediaStream`,
  and renders it in a fullscreen `<video>` element (video shows muted; audio is
  unmuted on play, falling back to a click-to-unmute if the browser blocks it).
