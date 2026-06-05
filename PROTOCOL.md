# rcd Protocol Specification

This document is the **source of truth** for the wire protocols used by `rcd`
(remote control desktop) in **Milestone 1 (M1)**. All three components —
`rcd-host` (Rust + GStreamer), `rcd-signal` (Node.js + TypeScript), and
`rcd-client` (Electron + TypeScript) — MUST conform to the field names, byte
layouts, and directions described here **exactly**.

There are two protocols:

1. The **signaling protocol** — JSON messages over a WebSocket, used to broker a
   WebRTC connection between a host and a client.
2. The **input protocol** — a compact binary format sent over a WebRTC
   DataChannel, used by the client to send input events to the host.

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

### 2.3 Reserved opcodes

| opcode | reserved for  |
| ------ | ------------- |
| `0x05` | gamepad state |
| `0x06+`| future use    |

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
