# rcd — remote control desktop

A **one-app, zero-popup WebRTC remote desktop**, in the spirit of Chrome Remote
Desktop but aiming for **Moonlight-class quality** (hardware encode/decode, low
latency, no manual port forwarding). You run one host on the machine you want to
control and one client on the machine you are sitting at; they pair with a short
code and connect peer-to-peer through a small signaling server.

> This repository is currently scaffolding **Milestone 1 (M1)**: the smallest
> end-to-end loop — the host captures the Windows screen, NVENC-encodes H.264,
> and streams it over WebRTC to an Electron client that hardware-decodes and
> displays it, while the client sends mouse-move input back over a DataChannel.

---

## What this is

- **Zero runtime popups:** no firewall prompts, no UAC at connect time, no
  per-session dialogs. (Install-time elevation is needed once later — see
  Caveats.)
- **No port forwarding:** connectivity uses STUN for NAT hole-punching, with a
  TURN relay (coturn on a VPS) added in **M1b** for hard NATs / CGNAT.
- **Peer-to-peer media:** video flows host -> client directly (or via TURN);
  the signaling server only brokers the handshake.

---

## Architecture

Three components plus a small VPS for signaling (and TURN in M1b):

```
        +-----------------------------------------------------------+
        |                         VPS                               |
        |   rcd-signal (Node/TS + ws)      coturn (TURN, M1b)        |
        |   ws://<vps>:8080/ws             turn:<vps>:3478           |
        +----------------------+------------------+-----------------+
                               |  signaling (JSON over WS)
              +----------------+                  +----------------+
              |                                                    |
   +----------v-----------+                          +-------------v---------+
   |   rcd-host (Rust)    |   WebRTC: H.264 video --> |  rcd-client (Electron)|
   |   GStreamer          | ========================>  |  hardware decode      |
   |   webrtcbin, NVENC   |   <-- "input" DataChannel  |  <video> fullscreen   |
   |   OFFERER, sends video|  (mouse, etc.)            |  ANSWERER, sends input|
   +----------------------+                          +-----------------------+
```

- **`rcd-host/`** — Rust + GStreamer `webrtcbin`. The **offerer**: captures the
  screen, NVENC-encodes H.264, sends video, and receives input. Needs GStreamer
  installed (see below).
- **`rcd-signal/`** — Node.js + TypeScript + `ws`. The **signaling relay**:
  rooms keyed by pairing code, max two peers, relays offer/answer/ICE verbatim.
- **`rcd-client/`** — Electron + TypeScript. The **answerer**: shows the video
  fullscreen and sends mouse input over the `"input"` DataChannel.

The exact signaling JSON and the input binary format are defined in
**[PROTOCOL.md](./PROTOCOL.md)** (the source of truth). HOST is the offerer;
CLIENT is the answerer.

---

## Toolchain prerequisites (this machine)

| Tool             | Status     | Notes                                                      |
| ---------------- | ---------- | ---------------------------------------------------------- |
| Rust / cargo     | ✅ present | 1.94. Builds `rcd-host`.                                   |
| Node.js / npm    | ✅ present | Node v24 / npm 11. Runs `rcd-signal` and `rcd-client`.     |
| Go               | ❌ not used | Intentionally — signaling is Node/TS, not Go.              |
| GStreamer        | ⚠️ install | **1.24+ MSVC** build required for the host (runtime + dev). |

`rcd-signal` and `rcd-client` run **today** with only `npm`. `rcd-host`
additionally requires a **GStreamer 1.24+ MSVC** install (both the runtime and
the development packages) with NVENC plugins, on the `PATH`, before building.

---

## Quickstart (M1)

Open three terminals.

### 1. Signaling server (`rcd-signal/`)

```powershell
cd C:\workspace\11_remoteControl\rcd-signal
npm install
npm run dev            # listens on ws://127.0.0.1:8080/ws ; PAIRING_CODE=123456
```

### 2. Client (`rcd-client/`)

```powershell
cd C:\workspace\11_remoteControl\rcd-client
npm install
npm start
# In the UI: Signaling URL = ws://127.0.0.1:8080/ws , Pairing Code = 123456 , click Connect.
```

### 3. Host (`rcd-host/`) — only after GStreamer is installed

```powershell
cd C:\workspace\11_remoteControl\rcd-host
cargo run -- preview     # local-only window: proves capture + NVENC encode works
cargo run -- stream      # joins signaling, offers WebRTC, streams to the client
```

`preview` proves the capture/encode pipeline on the host alone (no network);
`stream` runs the full WebRTC path against the signaling server and client.

Defaults: `SIGNAL_URL=ws://127.0.0.1:8080/ws`, `PAIRING_CODE=123456`,
`ENCODER=nvh264enc`, STUN `stun:stun.l.google.com:19302`.

---

## Milestone plan & acceptance criteria

### M1a — local end-to-end loop (same LAN / STUN)

- [ ] Host captures the screen and **hardware-encodes** H.264 — confirm the
      NVENC engine is active in **Task Manager** (GPU -> "Video Encode").
- [ ] Client decodes in **hardware** — confirm via WebRTC `getStats()` that the
      inbound video `decoderImplementation` is a HW decoder (not software).
- [ ] Client renders video fullscreen and mouse-move events are **logged on the
      host** over the `"input"` DataChannel.

### M1b — cross-NAT via TURN

- [ ] Stand up **coturn** on a VPS; wire `turnUrl` / `turnUser` / `turnPass`.
- [ ] Connect **across different NATs** with **no port forwarding** and **no
      popup**, including behind Korean CGNAT, using the TURN relay.

---

## Honest caveats

- **No adaptive bitrate yet.** M1 uses a fixed encoder bitrate. Congestion
  control / adaptive bitrate is the **next major work (M2)**.
- **TURN is required for many real networks.** STUN alone will not punch through
  symmetric NAT or **Korean CGNAT**; a VPS + coturn (M1b) is needed there.
- **Quality:** H.264 **4:2:0** in M1 — fine for desktop video but soft on small
  colored text / fine red detail. Higher chroma (4:4:4) and AV1 are later
  milestones.
- **Popups:** the app is **popup-free at runtime**, but full input injection
  across UAC-elevated windows and gamepad emulation will need a **one-time,
  install-time elevation** later (a service / driver install), not a per-session
  prompt.

---

## See also

- **[PROTOCOL.md](./PROTOCOL.md)** — signaling JSON + input binary format (source of truth).
- **[rcd-host/README.md](./rcd-host/README.md)** — Rust + GStreamer host.
- **[rcd-signal/README.md](./rcd-signal/README.md)** — Node/TS signaling server.
- **[rcd-client/README.md](./rcd-client/README.md)** — Electron client.
