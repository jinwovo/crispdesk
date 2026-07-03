# crispdesk

A **one-app, zero-popup WebRTC remote desktop** — the accessibility of Chrome
Remote Desktop ("just pair with a code, no router port forwarding") with
**Moonlight-class image quality** (hardware encode/decode, low latency, a sharp
picture instead of a mushy one). Run one host on the machine you want to control
and one client on the machine you're sitting at; they pair with a short code and
connect peer-to-peer through a small signaling server.

> **Status — working Milestone 1+.** The full loop runs end-to-end over LAN and
> over Tailscale today: the host captures the screen, **hardware-encodes** H.264,
> streams **video + system audio** over WebRTC to a browser/Electron client that
> hardware-decodes and displays it, while the client sends **mouse, keyboard, and
> wheel** input back over a DataChannel. On top of that: **adaptive bitrate** (M2),
> bidirectional **clipboard sync**, an in-session **stats HUD** with live **quality
> presets**, **client-driven monitor switching**, and **drag & drop file transfer**
> to the host. Cross-NAT-via-TURN (M1b) is code-wired and needs a coturn server.

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
- **In-session controls (📊 HUD):** the client overlay shows live stats
  (resolution/fps, receive bitrate, RTT, loss, P2P-vs-TURN path, host encoder
  telemetry), switches the **captured monitor** without reconnecting by hand, and
  moves the **bitrate ceiling** live (`자동`/3/5/8/12/20/30 Mbps presets).
- **File transfer:** drag files onto the client window — they land in the host's
  Downloads folder (size-capped, name-sanitized, consent-gated; `FILES=0` to
  disable).

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
cargo run -- serve       # NO separate server: run signaling in-process + stream
cargo run -- stream      # use an EXTERNAL signaling server (rcd-signal) instead
```

**Two apps only (`serve`):** like Moonlight, you can skip the separate signaling
server. `cargo run -- serve` runs the relay **inside the host** on `PORT` (8080) and
prints a **PIN**. On the other machine, point the client at
`ws://<host-ip>:8080/ws` (the host's LAN or Tailscale IP) and enter the PIN — that's
it. Over **Tailscale this needs no port forwarding and no NAS**. (For arbitrary
internet behind CGNAT without a VPN, you still need a reachable relay — see TURN.)

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
| `FILES`      | on                       | `FILES=0` disables file transfer (drag & drop to host).   |
| `FILE_DIR`   | `%USERPROFILE%\Downloads`| Where received files are saved.                           |
| `FILE_MAX_BYTES` | 2 GiB                | Per-file size cap for received files.                     |
| `REQUIRE_CONSENT` | off                 | `REQUIRE_CONSENT=1` blocks injected input, inbound clipboard, file receipt, and control commands until the seated user approves a per-session consent dialog. |
| `LOCK_ON_DISCONNECT` | off              | `LOCK_ON_DISCONNECT=1` locks the workstation when the remote peer leaves. |
| `AUDIT_LOG`  | `%LOCALAPPDATA%\crispdesk\audit.jsonl` | Session-event JSONL path; `AUDIT_LOG=off` disables. |
| `ENCODER`    | (auto-probe)             | Force an encoder, e.g. `x264enc`.                         |
| `ALLOW_GPL`  | off                      | `ALLOW_GPL=1` lets the probe auto-select the **GPL** `x264enc` (excluded by default for proprietary builds; license-clean `openh264enc` is the software fallback). |
| `CAPTURE`    | (auto-probe)             | Force a capture source.                                   |
| `RTP_MTU`    | `1200`                   | RTP packet size — **leave at 1200** (see PROTOCOL.md).    |
| `SIGNAL_URL` | `ws://127.0.0.1:8080/ws` | Signaling server URL.                                     |
| `PAIRING_CODE` | `123456`               | Fixed code, **only** when the server runs `DEV_MODE=true`. |

Signaling server env: `DEV_MODE=true` uses a fixed `PAIRING_CODE`; otherwise each
host is issued a random code (`CODE_TTL_MS`, default 5 min) shown in the host log,
with per-IP join rate-limiting (`JOIN_MAX_ATTEMPTS`/`JOIN_WINDOW_MS`). Hardening:
`TLS_CERT`+`TLS_KEY` (PEM paths) enable **wss://** (falls back to `ws://` if unset/
unreadable); `WS_MAX_PAYLOAD` caps frame size (default 256 KiB); `ALLOWED_ORIGINS`
(comma list) restricts browser WS upgrades; `DISABLE_TEST_PAGE=true` hides the
unauthenticated browser client.

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

## Testing & CI

```powershell
cd rcd-signal; npm test    # pairing code / TURN credential / rate-limit / origin allowlist (9)
cd rcd-client; npm test    # wire-format encoders/decoders (input/clipboard/file), control JSON, maps (26)
cd rcd-host;   cargo test   # clipboard/file codecs, filename sanitize, control JSON, ABR, TURN URI, aspect-fit (13)
```

GitHub Actions (`.github/workflows/ci.yml`) builds + tests all three components on
every push/PR: the Node side on Ubuntu, the Rust host on a native ARM64 Windows
runner with GStreamer installed.

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
- [x] **Control channel** (`"control"`, JSON): stats HUD (client + host telemetry),
      **client-driven monitor switching** (session rebuild over live signaling), live
      **bitrate-ceiling presets** that survive rebuilds. See PROTOCOL.md §2.5.
- [x] **File transfer** (`"file"`): drag & drop client→host with backpressure,
      sanitized names, size cap, consent gating, `.part`+rename. See PROTOCOL.md §2.6.

### M1b — cross-NAT via TURN — **code wired; needs a coturn deployment**

- [x] Server mints **ephemeral** TURN credentials and pushes STUN+TURN to both peers
      via `ice-servers`; host applies them to `webrtcbin` (`added=true` verified),
      client passes them to `RTCPeerConnection`. See "TURN relay" above.
- [ ] Stand up **coturn** on a NAS/VPS (`TURN_URLS` + `TURN_SECRET`) and confirm a real
      cross-NAT / CGNAT connection with **no port forwarding**. (Today without coturn:
      same-LAN or via Tailscale only.)

### Toward a sellable product

- [x] **License-clean default**: GPL `x264enc` excluded from auto-selection unless
      `ALLOW_GPL=1`; `openh264enc` is the software fallback. (H.264 MPEG-LA royalty
      strategy still TBD.)
- [x] **Auth/transport hardening**: 8-char (~40-bit) pairing code, per-IP rate limit,
      `DISABLE_TEST_PAGE`, **wss/TLS** (`TLS_CERT`/`TLS_KEY`), WS frame cap, Origin
      allowlist; host-side **consent dialog** (`REQUIRE_CONSENT`), **audit log**, and
      `LOCK_ON_DISCONNECT`.
- [x] **Automated tests + CI** foundation (48 tests; see Testing above).
- [x] **electron-builder** packaging config (`npm run dist` after installing it).
- [x] **File transfer** (client→host) and **client-driven monitor switching**.
- [ ] Code-signing cert + notarization + auto-update (needs a cert).
- [ ] Host as a **Windows service** (control the UAC/secure desktop, run on boot).
- [ ] Account/identity (persistent), config UI; H.264 royalty strategy.
- [ ] Host→client file transfer; clipboard images; macOS host.

---

## Honest caveats

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
