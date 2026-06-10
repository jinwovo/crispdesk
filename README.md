# crispdesk

A **one-app, zero-popup WebRTC remote desktop** — the accessibility of Chrome
Remote Desktop ("just pair with a code, no router port forwarding") with
**Moonlight-class image quality** (hardware encode/decode, low latency, a sharp
picture instead of a mushy one). Run one host on the machine you want to control
and one client on the machine you're sitting at; they pair with a short code and
connect peer-to-peer through a small signaling server.

> **Status — working Milestone 1.** The full loop runs end-to-end over LAN and
> over Tailscale today: the host captures the screen, **hardware-encodes** H.264,
> streams **video + system audio** over WebRTC to a browser/Electron client that
> hardware-decodes and displays it, while the client sends **mouse, keyboard, and
> wheel** input back over a DataChannel. Cross-NAT-via-TURN (M1b) and adaptive
> bitrate (M2) are next.

---

## What makes it different

- **Zero runtime popups:** no firewall prompt, no UAC at connect time, no
  per-session dialog. (A one-time install-time elevation is needed later — see
  Caveats.)
- **No port forwarding:** connectivity uses STUN for NAT hole-punching, with a
  TURN relay (coturn on a VPS) for hard NATs / Korean CGNAT in **M1b**.
- **Universal hardware encode:** the host probes the machine at runtime and uses
  the best available H.264 encoder — **NVENC / Quick Sync / AMF / Media
  Foundation (incl. Qualcomm Adreno) / openh264 / x264** — so the *same* host
  binary runs on any GPU and falls back to software when needed. Verified on a
  Snapdragon X (ARM64-native) using the Adreno hardware encoder.
- **Sharp by default:** 1080p @ 12 Mbps H.264 with GPU scaling; resolution and
  bitrate are tunable per session.

---

## Architecture

Three components plus a small VPS for signaling (and TURN in M1b):

```
        +-----------------------------------------------------------+
        |                         VPS                                |
        |   rcd-signal (Node/TS + ws)      coturn (TURN, M1b)        |
        |   ws://<vps>:8080/ws             turn:<vps>:3478           |
        +----------------------+------------------+-----------------+
                               |  signaling (JSON over WS)
              +----------------+                  +----------------+
              |                                                    |
   +----------v------------+                        +-------------v---------+
   |   rcd-host (Rust)     |  WebRTC: H.264 video + |  rcd-client (Electron)|
   |   GStreamer webrtcbin |  Opus audio  ========>  |  hardware decode      |
   |   universal HW encode |  <-- "input" DataChannel|  <video> fullscreen   |
   |   OFFERER             |  (mouse / kbd / wheel)  |  ANSWERER, sends input|
   +-----------------------+                        +-----------------------+
```

- **`rcd-host/`** — Rust + GStreamer `webrtcbin`. The **offerer**: probes and
  captures the screen, hardware-encodes H.264 (+ Opus system audio), sends media,
  and injects received input. Requires GStreamer (see below).
- **`rcd-signal/`** — Node.js + TypeScript + `ws`. The **signaling relay**: rooms
  keyed by pairing code, max two peers, relays offer/answer/ICE verbatim. Also
  serves a **browser test client** at `http://<host>:8080/` for no-install
  second-device testing.
- **`rcd-client/`** — Electron + TypeScript. The **answerer**: renders the video
  fullscreen, plays audio, and forwards mouse/keyboard/wheel over the `"input"`
  DataChannel.

The signaling JSON and the input binary format are defined in
**[PROTOCOL.md](./PROTOCOL.md)** (the source of truth). HOST is the offerer;
CLIENT is the answerer.

---

## Prerequisites

| Tool          | Notes                                                                 |
| ------------- | --------------------------------------------------------------------- |
| Rust / cargo  | Builds `rcd-host`. On Windows-on-ARM use the `aarch64-pc-windows-msvc` toolchain + the MSVC ARM64 C++ build tools. |
| Node.js / npm | Runs `rcd-signal` and `rcd-client`.                                    |
| GStreamer     | **1.24+ MSVC** (runtime + development), on `PATH`. The 1.28 ARM64 unified installer bundles `pkg-config`. |

`rcd-signal` and `rcd-client` run with only `npm`. `rcd-host` additionally needs
a GStreamer MSVC install on the `PATH` (set `PKG_CONFIG_PATH` to its
`lib/pkgconfig`) before `cargo build`.

---

## Quickstart

Open three terminals.

### 1. Signaling server (`rcd-signal/`)

```powershell
cd rcd-signal
npm install
npm run dev            # ws://0.0.0.0:8080/ws ; PAIRING_CODE=123456
                       # browser test client: http://<this-machine-ip>:8080/
```

### 2. Host (`rcd-host/`)

```powershell
cd rcd-host
cargo run -- probe       # print what THIS machine supports (encoder + capture)
cargo run -- preview     # local-only window: proves capture + HW encode works
cargo run -- stream      # joins signaling, offers WebRTC, streams to the client
```

Useful host env vars:

| Var          | Default                  | Effect                                                    |
| ------------ | ------------------------ | --------------------------------------------------------- |
| `RES`        | `1920x1080`              | Encode resolution cap. `native` = full desktop, or `WxH`. |
| `BITRATE`    | `12000`                  | Encoder/ABR ceiling in kbps.                              |
| `BITRATE_MIN`| `1500`                   | Adaptive-bitrate floor in kbps.                           |
| `ABR`        | on                       | `ABR=0` pins a fixed bitrate (disables adaptation).       |
| `MONITOR`    | primary                  | Monitor index to capture + control (enumerated at startup).|
| `AUDIO`      | on                       | `AUDIO=0` disables system-audio capture.                  |
| `CLIPBOARD`  | on                       | `CLIPBOARD=0` disables clipboard sync.                    |
| `ENCODER`    | (auto-probe)             | Force an encoder, e.g. `x264enc`.                         |
| `CAPTURE`    | (auto-probe)             | Force a capture source.                                   |
| `RTP_MTU`    | `1200`                   | RTP packet size — **leave at 1200** (see PROTOCOL.md).    |
| `SIGNAL_URL` | `ws://127.0.0.1:8080/ws` | Signaling server URL.                                     |
| `PAIRING_CODE` | `123456`               | Fixed code, **only** when the server runs `DEV_MODE=true`. |

Signaling server env: `DEV_MODE=true` uses a fixed `PAIRING_CODE`; otherwise each
host is issued a random code (`CODE_TTL_MS`, default 5 min) shown in the host log,
with per-IP join rate-limiting (`JOIN_MAX_ATTEMPTS`/`JOIN_WINDOW_MS`).

### TURN relay for cross-NAT (M1b — code wired, needs a coturn server)

Without a TURN relay, connections only work on the same LAN or via an overlay like
Tailscale. To connect across NATs/CGNAT, run **coturn** (e.g. on a NAS or VPS) and
point the signaling server at it. The server then mints **ephemeral** TURN
credentials per session (coturn `use-auth-secret` scheme) and pushes them to both
peers via the `ice-servers` message — no static TURN password ships in any client.

```ini
# /etc/turnserver.conf (coturn)
listening-port=3478
tls-listening-port=5349
use-auth-secret
static-auth-secret=<LONG_RANDOM_SECRET>   # must match TURN_SECRET below
realm=crispdesk
# (for TLS) cert=/path/fullchain.pem  pkey=/path/privkey.pem
```

```powershell
# signaling server env
$env:TURN_URLS = "turn:<nas-host>:3478,turns:<nas-host>:5349"
$env:TURN_SECRET = "<LONG_RANDOM_SECRET>"   # same as static-auth-secret
$env:TURN_TTL_SEC = "3600"
```

The host logs `ICE: add-turn-server ... (added=true)` once it receives them. Verify
end-to-end with [trickle-ice](https://webrtc.github.io/samples/src/content/peerconnection/trickle-ice/).

### 3. Client

Either run the Electron app:

```powershell
cd rcd-client
npm install
npm start              # UI: Signaling URL, Pairing Code, Connect
```

…or, for a second device with nothing installed, just open
**`http://<host-ip>:8080/`** in a browser and press **Connect**.

---

## Milestone plan

### M1a — local end-to-end loop (LAN / STUN) — ✅ done

- [x] Host captures the screen and **hardware-encodes** H.264 (verified Adreno HW
      encode on Snapdragon X via CPU-accounting).
- [x] Client renders video fullscreen; **mouse, keyboard, and wheel** are injected
      on the host over the `"input"` DataChannel.
- [x] **System audio** (Opus) streams alongside video.
- [x] Works over **Tailscale** between two devices (after the RTP-MTU fix).

### M2 — adaptive bitrate — ✅ done

- [x] Drive the encoder bitrate from `webrtcbin` RTCP loss feedback (AIMD with
      hysteresis; see `rcd-host/src/abr.rs`). `ABR=0` to disable.

### Beyond the original plan — ✅ done

- [x] **System audio** (Opus loopback) and **bidirectional clipboard** sync.
- [x] **Multi-monitor** capture/control selection (`MONITOR`).
- [x] **Dynamic pairing codes** (random, TTL, per-IP rate-limit) replacing the fixed code.
- [x] **Robustness:** signaling auto-reconnect + keepalive; host stuck-key release on
      disconnect; per-monitor **DPI awareness**; client **jitter-buffer** latency tuning.

### M1b — cross-NAT via TURN — **code wired; needs a coturn deployment**

- [x] Server mints **ephemeral** TURN credentials and pushes STUN+TURN to both peers
      via `ice-servers`; host applies them to `webrtcbin` (`added=true` verified),
      client passes them to `RTCPeerConnection`. See "TURN relay" above.
- [ ] Stand up **coturn** on a NAS/VPS (`TURN_URLS` + `TURN_SECRET`) and confirm a real
      cross-NAT / CGNAT connection with **no port forwarding**. (Today without coturn:
      same-LAN or via Tailscale only.)

### Toward a sellable product — not yet started

- [ ] Signed installers (Windows code-sign, macOS notarize) + auto-update.
- [ ] Host as a **Windows service** (control the UAC/secure desktop, run on boot).
- [ ] Account/identity + consent UI; **wss/TLS** signaling; config UI (no env vars).
- [ ] Replace GPL `x264enc` for proprietary distribution; address H.264 royalties.
- [ ] Automated tests + CI; file transfer; client-driven monitor switching.

---

## Honest caveats

- **No adaptive bitrate yet.** A fixed encoder bitrate is used; congestion
  control is M2.
- **TURN is required for many real networks.** STUN alone won't punch through
  symmetric NAT or Korean CGNAT; a VPS + coturn (M1b) is needed there. Tailscale
  is only a convenient test transport today.
- **Quality:** H.264 **4:2:0** — fine for desktop video, slightly soft on small
  saturated-colored text. 4:4:4 / AV1 are later milestones.
- **Popups:** popup-free at runtime, but injecting input across UAC-elevated
  windows and gamepad emulation will need a **one-time, install-time** elevation
  (a service / signed driver) later — not a per-session prompt.

---

## See also

- **[PROTOCOL.md](./PROTOCOL.md)** — signaling JSON + input binary format + media (source of truth).
- **[rcd-host/README.md](./rcd-host/README.md)** — Rust + GStreamer host.
- **[rcd-signal/README.md](./rcd-signal/README.md)** — Node/TS signaling server.
- **[rcd-client/README.md](./rcd-client/README.md)** — Electron client.
