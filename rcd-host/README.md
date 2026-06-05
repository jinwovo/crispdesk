# rcd-host (Rust + GStreamer)

The HOST side of **rcd**: captures the Windows desktop, hardware-encodes H.264, streams it
over WebRTC (`webrtcbin`) to the Electron client, and receives mouse input over a DataChannel
labeled `input`. The host is the WebRTC **offerer**.

Built with Rust + [gstreamer-rs](https://gitlab.freedesktop.org/gstreamer/gstreamer-rs) (the
`0.23` family — requires GStreamer **≥1.24**; the installed 1.28.x satisfies it, no crate bump
needed).

## Universal by design

The encoder and capture source are **auto-detected at runtime** (`src/probe.rs`), so the *same
binary* works on any Windows GPU and always runs:

```
ENCODER ladder:  nvh264enc → qsvh264enc → amfh264enc → mfh264enc → openh264enc → x264enc
                 (NVIDIA)    (Intel QSV)  (AMD AMF)   (MediaFnd)   (software, always-present)
CAPTURE ladder:  d3d11screencapturesrc(wgc) → d3d11screencapturesrc(dxgi) → gdiscreencapturesrc
```

Each candidate is validated by actually running a tiny pipeline (a registered element can still
fail), and the winner builds the real pipeline. No hardware encoder? It falls through to
software (`openh264enc`/`x264enc`) — lower quality / higher CPU, but it **always works**.

On this Snapdragon/Adreno machine the interesting row is **`mfh264enc` (Media Foundation)** —
the only possible HW-encode path on ARM, and only reachable from a **native ARM64** process.
`cargo run -- probe` is the on-device test for whether it actually binds to the Adreno encoder.

Force a choice and skip the probe: `ENCODER=x264enc`, `CAPTURE=gdiscreencapturesrc`.

## Toolchain (this machine: ARM64-native)

Pinned to **`aarch64-pc-windows-msvc`** via `rust-toolchain.toml`, matching the native ARM64
GStreamer. Why: (a) Windows GStreamer is MSVC-ABI (a `-gnu` Rust won't link), (b) only a native
ARM64 process can reach the Adreno HW encoder via Media Foundation, (c) the compiler itself runs
without the x64-emulation tax.

> For a normal **x86_64 GPU host PC** (NVIDIA/AMD/Intel): switch `rust-toolchain.toml` to
> `stable-x86_64-pc-windows-msvc` (already installed here too) and install the x86_64 MSVC
> GStreamer instead.

## Prerequisites (one-time, needs admin)

1. **GStreamer 1.28.x ARM64** — single unified installer (runtime + development merged since
   1.28): `gstreamer-1.0-msvc-arm64-<ver>.exe` from
   <https://gstreamer.freedesktop.org/download/#windows>. If the installer offers a component
   choice, pick the **full/complete** set (we need d3d11, mediafoundation, webrtc, openh264/x264,
   libav plugins).
   - Note: the ARM64 package ships no Rust/Python/introspection extras — fine, we only use C
     elements.
2. **MSVC ARM64 build tools** — *verified missing on this machine*. Open **Visual Studio
   Installer** → Build Tools 2022 → **Modify** → Individual components → search "ARM64" → check
   **"MSVC v143 - VS 2022 C++ ARM64/ARM64EC build tools (Latest)"** → Modify. Without this there
   is no ARM64-targeting `link.exe` and `cargo build` cannot link.
3. **pkg-config** — gstreamer-rs locates GStreamer through it. First check whether the GStreamer
   1.28 install brought one (`Get-Command pkg-config` after install / look in its `bin`). If not:
   `choco install pkgconfiglite` (x86 binary; runs fine under emulation — it's only a build tool).
4. Environment (PowerShell; make permanent via System Properties). The ARM64 install root env
   var is typically `GSTREAMER_1_0_ROOT_MSVC_ARM64` — confirm the actual path the installer used:
   ```powershell
   $env:GSTREAMER_1_0_ROOT_MSVC_ARM64 = "C:\gstreamer\1.0\msvc_arm64"   # adjust to actual
   $env:PATH            = "$env:GSTREAMER_1_0_ROOT_MSVC_ARM64\bin;$env:PATH"
   $env:GST_PLUGIN_PATH = "$env:GSTREAMER_1_0_ROOT_MSVC_ARM64\lib\gstreamer-1.0"
   $env:PKG_CONFIG_PATH = "$env:GSTREAMER_1_0_ROOT_MSVC_ARM64\lib\pkgconfig"
   ```
5. Restart the terminal, then verify:
   ```powershell
   gst-inspect-1.0 --version
   gst-inspect-1.0 d3d11screencapturesrc   # capture (Adreno: genuinely untested!)
   gst-inspect-1.0 mfh264enc               # the Adreno HW-encode hope
   gst-inspect-1.0 openh264enc             # software floor (must exist)
   pkg-config --modversion gstreamer-1.0
   ```

## Build & run

```powershell
cargo build                 # compiles against the installed GStreamer (native ARM64)

cargo run -- probe          # Step 0: print which encoder + capture THIS PC supports
cargo run -- preview        # Step 1: capture → encode → decode → window (no network)
cargo run -- stream         # Step 2: join signaling, negotiate WebRTC, stream + receive input
```

`probe` and `preview` need nothing else; `stream` needs the `rcd-signal` server (and the
`rcd-client`) running. If `probe` selects `mfh264enc`, confirm real HW encode via Task Manager →
Performance → GPU → **Video Encode** during `preview`.

## Environment variables

| Var | Default | Notes |
|-----|---------|-------|
| `ENCODER` | *(auto)* | Force an encoder element, skip the probe (e.g. `x264enc`). |
| `CAPTURE` | *(auto)* | Force a capture source, skip the probe (e.g. `gdiscreencapturesrc`). |
| `SIGNAL_URL` | `ws://127.0.0.1:8080/ws` | Signaling server (path **must** be `/ws`). |
| `PAIRING_CODE` | `123456` | Room / pairing code (M1 stub). |
| `STUN` | `stun://stun.l.google.com:19302` | Note the `stun://` scheme webrtcbin expects. |
| `TURN` | *(none)* | `turn://user:pass@host:3478` (M1b). |
| `RUST_LOG` | `rcd_host=info,warn` | Rust logs. |
| `GST_DEBUG` | *(none)* | GStreamer logs, e.g. `3` or `webrtcbin:5`. |

## On-device TODOs (can't be settled without the installed GStreamer + GPU)

- **Encoder tuning props** in `src/probe.rs` are *unverified starting guesses* (rc-mode / preset /
  bitrate-units differ per element & version). Verify with `gst-inspect-1.0 <element>` and adjust;
  a wrong prop name fails the *real* pipeline (the probe itself never applies tuning).
- **webrtcbin API** in `src/webrtc.rs` (create-offer promise, create-data-channel, signal
  signatures) is version-sensitive — reconcile against the official gstreamer-rs `webrtc` example
  on first `cargo build` (search `TODO(on-device)`).
- **d3d11 capture on Adreno** is genuinely untested — if it fails, the capture probe falls back to
  `gdiscreencapturesrc`. Capture, not just encode, is a real unknown here.
- **mfh264enc → Adreno binding** is THE open question for HW quality on this machine.
- **Input injection**: absolute mouse move works via `SetCursorPos` (`src/input.rs`); buttons /
  keys / wheel / multi-monitor / DPI are next.
