// rcd-client — RENDERER (the WebRTC ANSWERER).
//
// Implements the pinned rcd signaling protocol against the Node signaling
// server, and the "input" DataChannel wire format.
//
// Role in the negotiation (NORMATIVE): the HOST is the offerer, the CLIENT is
// the answerer. We:
//   1. open a WebSocket to the signaling server and send {type:"join", role:"client"}
//   2. wait for the host's {type:"offer"} -> setRemoteDescription -> createAnswer
//      -> {type:"answer"}
//   3. trickle ICE both directions as {type:"ice", ...}
//   4. show the incoming video track (pc.ontrack -> <video>.srcObject)
//   5. receive the host-created DataChannel labeled "input" (pc.ondatachannel)
//      and SEND MOUSE_MOVE_ABS messages over it on mousemove.

// Pure wire-format encoders/decoders + input maps. Extracted to wire.ts so they
// can be unit-tested without the DOM/RTCPeerConnection machinery (see test/).
import {
  encodeMouseMoveAbs,
  encodeMouseButton,
  encodeKey,
  encodeWheel,
  encodeClipboard,
  decodeClipboard,
  BUTTON_MAP,
  SCANCODES,
} from "./wire.js";

// ---------------------------------------------------------------------------
// Protocol message types (must match rcd-signal and rcd-host EXACTLY).
// ---------------------------------------------------------------------------

type Role = "host" | "client";

interface JoinMsg {
  type: "join";
  room: string;
  role: Role;
}
interface JoinedMsg {
  type: "joined";
  role: Role;
  peers: number;
}
interface PeerJoinedMsg {
  type: "peer-joined";
  role: Role;
}
interface PeerLeftMsg {
  type: "peer-left";
  role: Role;
}
interface ErrorMsg {
  type: "error";
  message: string;
}
interface OfferMsg {
  type: "offer";
  sdp: string;
}
interface AnswerMsg {
  type: "answer";
  sdp: string;
}
interface IceMsg {
  type: "ice";
  candidate: string;
  sdpMid: string | null;
  sdpMLineIndex: number | null;
}
interface IceServersMsg {
  type: "ice-servers";
  iceServers: RTCIceServer[];
}

type ServerMsg =
  | JoinedMsg
  | PeerJoinedMsg
  | PeerLeftMsg
  | ErrorMsg
  | OfferMsg
  | AnswerMsg
  | IceMsg
  | IceServersMsg;

// ---------------------------------------------------------------------------
// Input DataChannel wire format: see ./wire.ts for the encoders/decoders and the
// BUTTON_MAP / SCANCODES tables (imported above). The byte layouts there are the
// single source of truth and must match rcd-host's decoder EXACTLY.
// ---------------------------------------------------------------------------

/**
 * Local clipboard access. In Electron a preload bridge (window.rcd.clipboard) gives
 * unrestricted access via the main process; in a plain browser we fall back to the
 * async Clipboard API (which needs focus/permission and may reject — callers guard).
 */
interface ClipboardBridge {
  readText(): Promise<string>;
  writeText(text: string): Promise<void>;
}
function localClipboard(): ClipboardBridge {
  const bridge = (window as unknown as { rcd?: { clipboard?: ClipboardBridge } }).rcd?.clipboard;
  if (bridge) return bridge;
  return {
    readText: () => navigator.clipboard.readText(),
    writeText: (t: string) => navigator.clipboard.writeText(t),
  };
}

// ---------------------------------------------------------------------------
// ICE configuration.
// ---------------------------------------------------------------------------

function buildIceServers(): RTCIceServer[] {
  const servers: RTCIceServer[] = [{ urls: "stun:stun.l.google.com:19302" }];

  // TURN placeholder for M1b. When a coturn server is available on the VPS,
  // populate these (e.g. read from a settings UI / localStorage) and push the
  // entry below. Relay is required when both peers are behind symmetric NAT.
  //
  // const turnUrl = "turns:turn.example.com:5349";
  // const turnUser = "rcd";
  // const turnPass = "secret";
  // if (turnUrl) {
  //   servers.push({ urls: turnUrl, username: turnUser, credential: turnPass });
  // }
  // TODO(M1b): wire TURN credentials from config (turnUrl/turnUser/turnPass).

  return servers;
}

// ---------------------------------------------------------------------------
// Connection controller. Owns one WS + one RTCPeerConnection at a time so we
// can fully tear down and reconnect cleanly.
// ---------------------------------------------------------------------------

class ClientConnection {
  private ws: WebSocket | null = null;
  private pc: RTCPeerConnection | null = null;
  private inputChannel: RTCDataChannel | null = null;
  // Clipboard sync over the reliable "clipboard" channel.
  private clipboardChannel: RTCDataChannel | null = null;
  private clipboardTimer: ReturnType<typeof setInterval> | null = null;
  /** Last clipboard text synced in EITHER direction (echo-loop guard). */
  private lastClipboard: string | null = null;
  /** ICE servers from the signaling server (STUN + ephemeral TURN); null = defaults. */
  private iceServers: RTCIceServer[] | null = null;

  // mousemove -> rAF throttle state. We coalesce moves to at most one send per
  // animation frame to avoid flooding the unreliable DataChannel.
  private pendingMove: { x: number; y: number } | null = null;
  private rafScheduled = false;
  // Inbound-video stats poller (black-screen diagnosis).
  private statsTimer: ReturnType<typeof setInterval> | null = null;
  // The single MediaStream we attach all inbound tracks (video + audio) to.
  private remoteStream: MediaStream | null = null;
  // One-shot click-to-unmute handler, installed only if unmuted autoplay is blocked.
  private unmuteOnClick: (() => void) | null = null;
  // Installed DOM listeners (for clean removal) + held keys (stuck-key safety).
  private inputListenersInstalled = false;
  private domCleanup: Array<() => void> = [];
  private pressedKeys = new Map<string, readonly [number, boolean]>();

  // ICE-restart recovery state. On a transient 'failed'/'disconnected' we first
  // ask the browser to restart ICE (the host re-offers on renegotiation) and only
  // fully reset the peer connection if it has not recovered after a grace period.
  // A one-shot timer prevents reconnect storms (no repeated restarts).
  private iceRestartTimer: ReturnType<typeof setTimeout> | null = null;
  /** True between a restartIce() attempt and its resolution (recovered or reset). */
  private iceRestarting = false;
  /** Grace period (ms) to wait for an ICE restart to recover before hard reset. */
  private static readonly ICE_RESTART_GRACE_MS = 4000;

  constructor(
    private readonly signalUrl: string,
    private readonly pairingCode: string,
    private readonly video: HTMLVideoElement,
    private readonly setStatus: (s: string) => void,
    private readonly onClosed: () => void,
  ) {}

  start(): void {
    this.setStatus(`connecting to ${this.signalUrl} ...`);
    let ws: WebSocket;
    try {
      ws = new WebSocket(this.signalUrl);
    } catch (err) {
      this.setStatus(`bad signaling URL: ${String(err)}`);
      this.onClosed();
      return;
    }
    this.ws = ws;

    ws.onopen = () => {
      this.setStatus("signaling connected — joining room");
      this.send({ type: "join", room: this.pairingCode, role: "client" });
    };

    ws.onmessage = (ev) => {
      let msg: ServerMsg;
      try {
        msg = JSON.parse(ev.data as string) as ServerMsg;
      } catch {
        console.warn("[rcd] ignoring non-JSON signaling message", ev.data);
        return;
      }
      void this.handleSignal(msg);
    };

    ws.onerror = () => {
      // The 'close' handler runs right after and drives teardown/status.
      console.warn("[rcd] signaling socket error");
    };

    ws.onclose = () => {
      this.setStatus("signaling closed");
      this.teardown();
      this.onClosed();
    };
  }

  /** Send a JSON message to the signaling server if the socket is open. */
  private send(msg: JoinMsg | AnswerMsg | IceMsg): void {
    if (this.ws && this.ws.readyState === WebSocket.OPEN) {
      this.ws.send(JSON.stringify(msg));
    }
  }

  private async handleSignal(msg: ServerMsg): Promise<void> {
    switch (msg.type) {
      case "joined":
        this.setStatus(
          `joined room (peers=${msg.peers}) — waiting for host offer`,
        );
        break;

      case "peer-joined":
        // As the answerer we do nothing proactive; we wait for the host's offer.
        this.setStatus(`peer joined (${msg.role}) — waiting for host offer`);
        break;

      case "peer-left":
        this.setStatus(`peer left (${msg.role})`);
        // Host went away: drop the peer connection but keep signaling open so a
        // returning host can re-offer.
        this.resetPeerConnection();
        break;

      case "error":
        this.setStatus(`error: ${msg.message}`);
        // Server closes after an error (e.g. wrong pairing code); teardown
        // happens in ws.onclose.
        break;

      case "offer":
        await this.handleOffer(msg);
        break;

      case "answer":
        // We are the answerer; receiving an answer is unexpected. Ignore.
        console.warn("[rcd] unexpected 'answer' received by client; ignoring");
        break;

      case "ice":
        await this.handleRemoteIce(msg);
        break;

      case "ice-servers":
        // Server handed us STUN + ephemeral TURN creds. Use them when we create the
        // peer connection (arrives before the host's offer).
        this.iceServers = msg.iceServers;
        console.log("[rcd] received ICE servers from signaling", msg.iceServers.length);
        break;

      default: {
        // Exhaustiveness guard.
        const _never: never = msg;
        console.warn("[rcd] unknown signaling message", _never);
      }
    }
  }

  /** Lazily create the RTCPeerConnection and wire its event handlers. */
  private ensurePeerConnection(): RTCPeerConnection {
    if (this.pc) return this.pc;

    // Prefer the signaling server's ICE servers (incl. TURN relay creds) when present;
    // otherwise fall back to the built-in STUN-only list (LAN / Tailscale).
    const pc = new RTCPeerConnection({
      iceServers: this.iceServers ?? buildIceServers(),
    });
    this.pc = pc;

    // Trickle our local ICE candidates to the host.
    pc.onicecandidate = (ev) => {
      if (ev.candidate) {
        this.send({
          type: "ice",
          candidate: ev.candidate.candidate,
          sdpMid: ev.candidate.sdpMid,
          sdpMLineIndex: ev.candidate.sdpMLineIndex,
        });
      }
    };

    pc.onconnectionstatechange = () => {
      const state = pc.connectionState;
      if (state === "connected") {
        // Recovered (possibly via an in-flight ICE restart): clear restart state.
        this.clearIceRestartTimer();
        this.iceRestarting = false;
        this.setStatus(`peer: ${state}`);
        this.startStats();
        return;
      }
      if (state === "closed") {
        // Terminal: nothing to recover. Hard reset (keeps signaling alive).
        this.resetPeerConnection();
        return;
      }
      if (state === "failed" || state === "disconnected") {
        // Transient or hard ICE failure. Attempt an ICE restart FIRST (the host
        // re-offers on renegotiation) and only fall back to a full reset if we are
        // still not connected after a short grace period. One-shot guarded.
        this.attemptIceRestart();
        return;
      }
      // "connecting" / "new" — just surface the state.
      this.setStatus(`peer: ${state}`);
    };

    // Incoming media: video AND (optionally) audio arrive as SEPARATE tracks.
    // We keep ONE MediaStream we control and add every inbound track to it, so a
    // second (audio) track never displaces the video regardless of how the peer
    // groups msids. Then we try to play UNMUTED; if the browser blocks unmuted
    // autoplay we fall back to muted playback (video always shows) and unmute on
    // the next user click.
    this.remoteStream = new MediaStream();
    pc.ontrack = (ev) => {
      const s = this.remoteStream;
      if (s && !s.getTracks().includes(ev.track)) {
        s.addTrack(ev.track);
      }
      if (this.video.srcObject !== s) {
        this.video.srcObject = s;
      }
      // Latency: shrink the receive jitter buffer for the live video track. A screen
      // feed tolerates the occasional reorder far better than added latency. Both
      // properties are best-effort + browser-specific, hence the guarded assignment.
      if (ev.track.kind === "video") {
        try {
          (ev.receiver as unknown as { jitterBufferTarget?: number }).jitterBufferTarget = 50;
        } catch {
          /* unsupported */
        }
        try {
          (ev.receiver as unknown as { playoutDelayHint?: number }).playoutDelayHint = 0;
        } catch {
          /* unsupported */
        }
      }
      this.tryPlayWithAudio();
    };

    // The HOST creates the DataChannels; we receive them here.
    pc.ondatachannel = (ev) => {
      if (ev.channel.label === "input") {
        this.attachInputChannel(ev.channel);
      } else if (ev.channel.label === "clipboard") {
        this.attachClipboardChannel(ev.channel);
      } else {
        console.warn("[rcd] unexpected data channel:", ev.channel.label);
      }
    };

    return pc;
  }

  /**
   * Poll inbound-video RTP stats once per second and surface them in the status
   * bar. This makes a black screen self-diagnosing:
   *   bytes>0, decoded=0           -> packets arrive but the decoder rejects them
   *                                   (H.264 profile/level mismatch) — black.
   *   bytes=0                       -> media transport not actually delivering RTP.
   *   decoded>0 but still black     -> a rendering/CSS problem, not WebRTC.
   */
  private startStats(): void {
    if (this.statsTimer) return;
    const pc = this.pc;
    if (!pc) return;
    this.statsTimer = setInterval(() => {
      void pc.getStats().then((report) => {
        let line = "";
        report.forEach((s) => {
          if (s.type === "inbound-rtp" && (s as { kind?: string }).kind === "video") {
            const r = s as unknown as {
              bytesReceived?: number;
              framesDecoded?: number;
              framesReceived?: number;
              frameWidth?: number;
              frameHeight?: number;
              pliCount?: number;
            };
            const kb = Math.round((r.bytesReceived ?? 0) / 1024);
            line =
              `video: ${kb}KB recv, ` +
              `${r.framesReceived ?? 0} frames, ${r.framesDecoded ?? 0} decoded, ` +
              `${r.frameWidth ?? 0}x${r.frameHeight ?? 0}, pli=${r.pliCount ?? 0}`;
          }
        });
        if (line) this.setStatus(line);
      });
    }, 1000);
  }

  private stopStats(): void {
    if (this.statsTimer) {
      clearInterval(this.statsTimer);
      this.statsTimer = null;
    }
  }

  /**
   * Start playback, preferring AUDIBLE play. Browsers block unmuted autoplay
   * without a user gesture; if that happens we keep the video playing MUTED (so
   * the picture is never lost) and arm a one-shot click handler that unmutes.
   */
  private tryPlayWithAudio(): void {
    const v = this.video;
    v.muted = false;
    v.play().catch(() => {
      // Unmuted play rejected: fall back to muted so video still shows...
      v.muted = true;
      void v.play().catch(() => {
        /* ignored: still interrupted; will retry on next track/gesture */
      });
      // ...and unmute on the next click anywhere (a real user gesture).
      if (!this.unmuteOnClick) {
        this.unmuteOnClick = () => {
          this.video.muted = false;
          void this.video.play().catch(() => {});
          if (this.unmuteOnClick) {
            window.removeEventListener("click", this.unmuteOnClick);
            this.unmuteOnClick = null;
          }
        };
        window.addEventListener("click", this.unmuteOnClick);
      }
    });
  }

  private async handleOffer(msg: OfferMsg): Promise<void> {
    const pc = this.ensurePeerConnection();
    try {
      await pc.setRemoteDescription({ type: "offer", sdp: msg.sdp });
      const answer = await pc.createAnswer();
      await pc.setLocalDescription(answer);
      // localDescription is fully populated after setLocalDescription resolves.
      this.send({ type: "answer", sdp: pc.localDescription!.sdp });
      this.setStatus("answer sent — connecting media");
    } catch (err) {
      this.setStatus(`negotiation failed: ${String(err)}`);
      console.error("[rcd] handleOffer error", err);
    }
  }

  private async handleRemoteIce(msg: IceMsg): Promise<void> {
    const pc = this.pc;
    if (!pc) {
      console.warn("[rcd] ICE received before peer connection exists");
      return;
    }
    try {
      await pc.addIceCandidate({
        candidate: msg.candidate,
        sdpMid: msg.sdpMid,
        sdpMLineIndex: msg.sdpMLineIndex,
      });
    } catch (err) {
      console.warn("[rcd] addIceCandidate failed", err);
    }
  }

  // -------------------------------------------------------------------------
  // Input channel: send normalized mouse position within the displayed video
  // content rectangle (accounting for object-fit:contain letterboxing).
  // -------------------------------------------------------------------------

  private attachInputChannel(channel: RTCDataChannel): void {
    channel.binaryType = "arraybuffer";
    this.inputChannel = channel;

    channel.onopen = () => {
      this.setStatus("input channel open — mouse/keyboard forwarded to host");
      this.installInputListeners();
    };
    channel.onclose = () => {
      this.removeInputListeners();
    };
  }

  // -------------------------------------------------------------------------
  // Clipboard sync (reliable "clipboard" channel). Bidirectional text.
  // -------------------------------------------------------------------------

  private attachClipboardChannel(channel: RTCDataChannel): void {
    channel.binaryType = "arraybuffer";
    this.clipboardChannel = channel;
    channel.onopen = () => this.startClipboardSync();
    channel.onclose = () => this.stopClipboardSync();
    channel.onmessage = (ev) => {
      if (!(ev.data instanceof ArrayBuffer)) return;
      const text = decodeClipboard(ev.data);
      if (text === null || text === this.lastClipboard) return; // malformed or our own echo
      // Only mark as synced AFTER the local write actually succeeds — otherwise a
      // failed write would leave the echo guard claiming a value the clipboard never
      // got (and the poller would never re-sync it). Self-healing: on failure
      // lastClipboard is unchanged, so the next identical message retries the write.
      void localClipboard()
        .writeText(text)
        .then(() => {
          this.lastClipboard = text;
        })
        .catch((e) => console.warn("[rcd] clipboard writeText failed:", e));
    };
  }

  /** Poll the local clipboard and forward genuine local changes to the host. */
  private startClipboardSync(): void {
    if (this.clipboardTimer) return;
    const clip = localClipboard();
    this.clipboardTimer = setInterval(() => {
      void clip
        .readText()
        .then((text) => {
          if (text === this.lastClipboard) return; // unchanged or just-received echo
          this.lastClipboard = text;
          const ch = this.clipboardChannel;
          if (ch && ch.readyState === "open") ch.send(encodeClipboard(text));
        })
        .catch(() => {
          /* browser: no focus/permission — clipboard read not available, ignore */
        });
    }, 500);
  }

  private stopClipboardSync(): void {
    if (this.clipboardTimer) {
      clearInterval(this.clipboardTimer);
      this.clipboardTimer = null;
    }
  }

  /** Send one binary input frame if the channel is open and forwarding is enabled. */
  private sendInput(buf: ArrayBuffer): void {
    // Optional "input" checkbox = view-only mode. Crucial for same-machine mirror
    // testing, where injected events would bounce the real cursor out of the video.
    const toggle = document.getElementById(
      "forwardInput",
    ) as HTMLInputElement | null;
    if (toggle && !toggle.checked) return;
    const ch = this.inputChannel;
    if (ch && ch.readyState === "open") {
      ch.send(buf);
    }
  }

  private installInputListeners(): void {
    if (this.inputListenersInstalled) return;
    this.inputListenersInstalled = true;

    const on = (
      target: Window | HTMLElement,
      type: string,
      fn: (ev: Event) => void,
      opts?: AddEventListenerOptions,
    ): void => {
      target.addEventListener(type, fn as EventListener, opts);
      this.domCleanup.push(() =>
        target.removeEventListener(type, fn as EventListener, opts),
      );
    };

    // --- mouse move (rAF-coalesced) ---
    on(this.video, "mousemove", (ev) => {
      const me = ev as MouseEvent;
      const norm = this.normalizeToVideoContent(me.clientX, me.clientY);
      if (!norm) return; // pointer is in the letterbox margin
      this.pendingMove = norm;
      if (!this.rafScheduled) {
        this.rafScheduled = true;
        requestAnimationFrame(() => this.flushMove());
      }
    });

    // --- mouse buttons (over the video content; suppress the local context menu) ---
    on(this.video, "mousedown", (ev) => {
      const me = ev as MouseEvent;
      if (!this.normalizeToVideoContent(me.clientX, me.clientY)) return;
      me.preventDefault();
      this.sendInput(encodeMouseButton(BUTTON_MAP[me.button] ?? 0, true));
    });
    on(this.video, "mouseup", (ev) => {
      const me = ev as MouseEvent;
      me.preventDefault();
      this.sendInput(encodeMouseButton(BUTTON_MAP[me.button] ?? 0, false));
    });
    on(this.video, "contextmenu", (ev) => ev.preventDefault());

    // --- wheel (convert browser deltas to Windows WHEEL_DELTA units) ---
    on(
      this.video,
      "wheel",
      (ev) => {
        const we = ev as WheelEvent;
        we.preventDefault();
        // deltaMode 0 = pixels (~100/notch in Chromium) -> *1.2 ≈ ±120/notch;
        // deltaMode 1 = lines (3/notch) -> *40. Clamp to i16.
        const k = we.deltaMode === 1 ? 40 : 1.2;
        const clamp = (v: number): number =>
          Math.max(-32767, Math.min(32767, Math.round(v)));
        const dy = clamp(-we.deltaY * k); // JS +down vs Windows +up
        const dx = clamp(we.deltaX * k); //  +right on both sides
        if (dy !== 0 || dx !== 0) this.sendInput(encodeWheel(dy, dx));
      },
      { passive: false },
    );

    // --- keyboard (window-level; paused while typing in the control bar) ---
    const typingInUi = (): boolean => {
      const a = document.activeElement;
      return (
        a instanceof HTMLInputElement ||
        a instanceof HTMLTextAreaElement ||
        a instanceof HTMLSelectElement
      );
    };
    on(window, "keydown", (ev) => {
      const ke = ev as KeyboardEvent;
      if (typingInUi()) return;
      const sc = SCANCODES[ke.code];
      if (!sc) return;
      ke.preventDefault();
      if (!ke.repeat) this.pressedKeys.set(ke.code, sc);
      this.sendInput(encodeKey(sc[0], true, sc[1])); // repeats forwarded = autorepeat
    });
    on(window, "keyup", (ev) => {
      const ke = ev as KeyboardEvent;
      const sc = SCANCODES[ke.code];
      if (!sc) return;
      this.pressedKeys.delete(ke.code);
      if (typingInUi()) return;
      ke.preventDefault();
      this.sendInput(encodeKey(sc[0], false, sc[1]));
    });
    // Stuck-key safety: if the window loses focus mid-press, release everything.
    on(window, "blur", () => this.releaseAllKeys());
  }

  /** Send key-up for every key we believe is held (focus loss / teardown). */
  private releaseAllKeys(): void {
    for (const [, sc] of this.pressedKeys) {
      this.sendInput(encodeKey(sc[0], false, sc[1]));
    }
    this.pressedKeys.clear();
  }

  private removeInputListeners(): void {
    this.releaseAllKeys();
    for (const cleanup of this.domCleanup) cleanup();
    this.domCleanup = [];
    this.inputListenersInstalled = false;
    this.pendingMove = null;
    this.rafScheduled = false;
  }

  /** Send at most one MOUSE_MOVE_ABS per animation frame. */
  private flushMove(): void {
    this.rafScheduled = false;
    const move = this.pendingMove;
    this.pendingMove = null;
    if (!move) return;
    this.sendInput(encodeMouseMoveAbs(move.x, move.y));
  }

  /**
   * Convert a viewport pixel position over the <video> element into normalized
   * 0..1 coordinates within the actual displayed video CONTENT, compensating for
   * the letterbox/pillarbox bars produced by object-fit:contain.
   * Returns null if the pointer is over the black margins (outside content).
   */
  private normalizeToVideoContent(
    clientX: number,
    clientY: number,
  ): { x: number; y: number } | null {
    const rect = this.video.getBoundingClientRect();
    const vw = this.video.videoWidth;
    const vh = this.video.videoHeight;
    if (vw === 0 || vh === 0 || rect.width === 0 || rect.height === 0) {
      return null; // no frame dimensions yet
    }

    // object-fit:contain scales the video by the smaller axis ratio so the
    // whole frame fits; the leftover space becomes symmetric letterbox bars.
    const scale = Math.min(rect.width / vw, rect.height / vh);
    const contentW = vw * scale;
    const contentH = vh * scale;
    const offsetX = (rect.width - contentW) / 2;
    const offsetY = (rect.height - contentH) / 2;

    // Position relative to the content rectangle's top-left.
    const px = clientX - rect.left - offsetX;
    const py = clientY - rect.top - offsetY;

    if (px < 0 || py < 0 || px > contentW || py > contentH) {
      return null; // in the letterbox margin -> not a meaningful host coordinate
    }

    return {
      x: Math.min(1, Math.max(0, px / contentW)),
      y: Math.min(1, Math.max(0, py / contentH)),
    };
  }

  // -------------------------------------------------------------------------
  // Reconnect: ICE restart with a one-shot fallback to a full reset.
  // -------------------------------------------------------------------------

  /**
   * Recover from a transient 'failed'/'disconnected' connection state by asking
   * the browser to restart ICE. The HOST re-offers on renegotiation, so we just
   * trigger the gathering and wait. Guarded so only ONE restart is in flight at a
   * time (no reconnect storms); if the connection has not recovered after a short
   * grace period we fall back to a full peer-connection reset.
   */
  private attemptIceRestart(): void {
    const pc = this.pc;
    if (!pc) return;
    // Already recovering? Let the existing grace timer decide; don't pile up.
    if (this.iceRestarting) {
      this.setStatus("reconnecting… (waiting for media)");
      return;
    }
    this.iceRestarting = true;
    this.setStatus("reconnecting… (restarting ICE)");

    // restartIce() is widely available but guard for older/edge runtimes; if it is
    // missing we simply rely on the grace-period fallback below.
    const restart = (pc as unknown as { restartIce?: () => void }).restartIce;
    if (typeof restart === "function") {
      try {
        restart.call(pc);
      } catch (err) {
        console.warn("[rcd] restartIce() threw; will fall back to reset", err);
      }
    } else {
      console.warn("[rcd] restartIce() unavailable; relying on grace-period reset");
    }

    // One-shot fallback: if still not connected after the grace period, hard reset.
    this.clearIceRestartTimer();
    this.iceRestartTimer = setTimeout(() => {
      this.iceRestartTimer = null;
      const cur = this.pc;
      if (cur && cur.connectionState === "connected") {
        // Recovered between scheduling and firing — onconnectionstatechange will
        // have cleared iceRestarting; nothing to do.
        return;
      }
      this.setStatus("reconnect failed — resetting connection");
      this.iceRestarting = false;
      this.resetPeerConnection();
    }, ClientConnection.ICE_RESTART_GRACE_MS);
  }

  private clearIceRestartTimer(): void {
    if (this.iceRestartTimer) {
      clearTimeout(this.iceRestartTimer);
      this.iceRestartTimer = null;
    }
  }

  // -------------------------------------------------------------------------
  // Teardown.
  // -------------------------------------------------------------------------

  /** Drop just the peer connection (e.g. host left), keep signaling alive. */
  private resetPeerConnection(): void {
    this.clearIceRestartTimer();
    this.iceRestarting = false;
    this.stopStats();
    this.stopClipboardSync();
    this.clipboardChannel = null;
    this.lastClipboard = null;
    this.removeInputListeners();
    if (this.inputChannel) {
      try {
        this.inputChannel.close();
      } catch {
        /* ignore */
      }
      this.inputChannel = null;
    }
    if (this.pc) {
      this.pc.onicecandidate = null;
      this.pc.ontrack = null;
      this.pc.ondatachannel = null;
      this.pc.onconnectionstatechange = null;
      try {
        this.pc.close();
      } catch {
        /* ignore */
      }
      this.pc = null;
    }
    if (this.video.srcObject) {
      this.video.srcObject = null;
    }
    this.remoteStream = null;
    if (this.unmuteOnClick) {
      window.removeEventListener("click", this.unmuteOnClick);
      this.unmuteOnClick = null;
    }
  }

  /** Full teardown: peer connection + signaling socket. */
  teardown(): void {
    this.resetPeerConnection();
    if (this.ws) {
      this.ws.onopen = null;
      this.ws.onmessage = null;
      this.ws.onerror = null;
      this.ws.onclose = null;
      try {
        this.ws.close();
      } catch {
        /* ignore */
      }
      this.ws = null;
    }
  }
}

// ---------------------------------------------------------------------------
// Wire up the control bar.
// ---------------------------------------------------------------------------

function main(): void {
  const signalUrlInput = document.getElementById(
    "signalUrl",
  ) as HTMLInputElement;
  const pairingCodeInput = document.getElementById(
    "pairingCode",
  ) as HTMLInputElement;
  const connectBtn = document.getElementById("connectBtn") as HTMLButtonElement;
  const statusEl = document.getElementById("status") as HTMLSpanElement;
  const video = document.getElementById("remote") as HTMLVideoElement;

  const setStatus = (s: string): void => {
    statusEl.textContent = s;
    console.log("[rcd]", s);
  };

  let conn: ClientConnection | null = null;

  const setConnected = (connected: boolean): void => {
    connectBtn.textContent = connected ? "Disconnect" : "Connect";
    signalUrlInput.disabled = connected;
    pairingCodeInput.disabled = connected;
  };

  const disconnect = (): void => {
    if (conn) {
      conn.teardown();
      conn = null;
    }
    setConnected(false);
    setStatus("idle");
  };

  connectBtn.addEventListener("click", () => {
    if (conn) {
      disconnect();
      return;
    }
    const url = signalUrlInput.value.trim();
    const code = pairingCodeInput.value.trim();
    if (!url || !code) {
      setStatus("enter a signaling URL and pairing code");
      return;
    }
    setConnected(true);
    conn = new ClientConnection(url, code, video, setStatus, () => {
      // onClosed: signaling/peer fully closed -> reset UI for a fresh connect.
      conn = null;
      setConnected(false);
    });
    conn.start();
  });

  // Ensure sockets/peer connection are released if the window goes away.
  window.addEventListener("beforeunload", () => {
    if (conn) conn.teardown();
  });
}

main();
