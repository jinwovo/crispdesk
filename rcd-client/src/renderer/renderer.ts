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
  type ControlMsg,
  type FileReply,
  type MonitorInfo,
  type AbrInfo,
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

  // --- "control" channel (JSON): session control + host telemetry (HUD). ---
  private controlChannel: RTCDataChannel | null = null;
  /** Host capability/config snapshot from the control `hello`. */
  private hostInfo: { encoder: string; abr: AbrInfo } | null = null;
  private hostFileTransfer = false;
  /** Latest ~1 Hz host telemetry (`stats` control message). */
  private hostStats: { encoderKbps: number; lossPct: number; rttMs: number } | null = null;
  /** Monitor list from the host's `hello` (kept to re-render the picker on errors). */
  private monitors: MonitorInfo[] = [];
  /** Previous cumulative counters for per-second rate computation (HUD). */
  private prevRates: { ts: number; bytes: number; frames: number } | null = null;

  // --- "file" channel: drag & drop uploads to the host. ---
  private fileChannel: RTCDataChannel | null = null;
  private nextTransferId = 1;
  /** Replies (accept/reject/done) that arrived while nothing was awaiting them. */
  private fileReplies = new Map<number, FileReply>();
  private fileReplyWaiters = new Map<number, (r: FileReply) => void>();
  private fileSendQueue: File[] = [];
  private fileSending = false;
  private dropCleanup: Array<() => void> = [];
  private static readonly FILE_CHUNK_BYTES = 16 * 1024;
  private static readonly FILE_BUFFER_HIGH = 4 * 1024 * 1024;
  private static readonly FILE_BUFFER_LOW = 1 * 1024 * 1024;

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
      } else if (ev.channel.label === "control") {
        this.attachControlChannel(ev.channel);
      } else if (ev.channel.label === "file") {
        this.attachFileChannel(ev.channel);
      } else {
        console.warn("[rcd] unexpected data channel:", ev.channel.label);
      }
    };

    return pc;
  }

  /**
   * Poll RTP/transport stats once per second: a compact line in the status bar
   * (keeps the black-screen self-diagnosis: bytes>0+decoded=0 = decoder rejects;
   * bytes=0 = transport dead; decoded>0 = rendering problem) plus the full HUD
   * panel (rates, RTT, path, host encoder telemetry).
   */
  private startStats(): void {
    if (this.statsTimer) return;
    if (!this.pc) return;
    this.statsTimer = setInterval(() => this.pollStats(), 1000);
  }

  private pollStats(): void {
    const pc = this.pc;
    if (!pc) return;
    void pc.getStats().then((report) => {
      // RTCStatsReport is maplike — use .get() for id lookups instead of copying
      // the whole report into a fresh Map every second.
      const byId = report as unknown as ReadonlyMap<string, unknown>;
      const get = (id: unknown): Record<string, unknown> | null =>
        typeof id === "string"
          ? ((byId.get(id) as Record<string, unknown> | undefined) ?? null)
          : null;
      let inb: Record<string, unknown> | null = null;
      let transport: Record<string, unknown> | null = null;
      let fallbackPair: Record<string, unknown> | null = null;
      report.forEach((s) => {
        const st = s as unknown as Record<string, unknown>;
        if (st.type === "inbound-rtp" && st.kind === "video") inb = st;
        else if (st.type === "transport" && typeof st.selectedCandidatePairId === "string")
          transport = st;
        else if (st.type === "candidate-pair" && st.nominated === true && st.state === "succeeded")
          fallbackPair = st;
      });
      const t = transport as Record<string, unknown> | null;
      const pair = (t ? get(t.selectedCandidatePairId) : null) ?? fallbackPair;

      const num = (o: Record<string, unknown> | null, k: string): number => {
        const v = o?.[k];
        return typeof v === "number" ? v : 0;
      };

      // Per-second rates from cumulative counters.
      const now = performance.now();
      const bytes = num(inb, "bytesReceived");
      const frames = num(inb, "framesDecoded");
      let mbps = 0;
      let fps = 0;
      if (this.prevRates && now > this.prevRates.ts) {
        const dt = (now - this.prevRates.ts) / 1000;
        mbps = ((bytes - this.prevRates.bytes) * 8) / dt / 1e6;
        fps = (frames - this.prevRates.frames) / dt;
      }
      this.prevRates = { ts: now, bytes, frames };

      const w = num(inb, "frameWidth");
      const h = num(inb, "frameHeight");
      const rttMs = num(pair, "currentRoundTripTime") * 1000;
      const lost = num(inb, "packetsLost");
      const pli = num(inb, "pliCount");
      const jitterMs = num(inb, "jitter") * 1000;

      // Path: TURN relay if either selected candidate is of type "relay".
      let path = "";
      if (pair) {
        const cand = (k: string): string => {
          const c = get((pair as Record<string, unknown>)[k]);
          return typeof c?.candidateType === "string" ? (c.candidateType as string) : "";
        };
        const l = cand("localCandidateId");
        const r = cand("remoteCandidateId");
        path = l === "relay" || r === "relay" ? "TURN 릴레이" : "P2P 직결";
      }

      // Compact status line (self-diagnosis essentials survive).
      if (inb) {
        this.setStatus(
          `${w}x${h} @${fps.toFixed(0)}fps · ${mbps.toFixed(1)}Mbps · ` +
            `RTT ${rttMs.toFixed(0)}ms · loss ${lost} · pli ${pli}` +
            (bytes === 0 ? " · (no RTP!)" : frames === 0 ? " · (no decode!)" : ""),
        );
      }

      // Full HUD rows (client-measured + host-reported).
      const rows: Array<[string, string]> = [
        ["해상도", w && h ? `${w}×${h} @ ${fps.toFixed(0)}fps` : "—"],
        ["수신", `${mbps.toFixed(1)} Mbps`],
        ["RTT", pair ? `${rttMs.toFixed(0)} ms` : "—"],
        ["경로", path || "—"],
        ["손실 / PLI", `${lost} / ${pli}`],
        ["지터", `${jitterMs.toFixed(0)} ms`],
      ];
      if (this.hostInfo) {
        rows.push(["인코더", this.hostInfo.encoder]);
      }
      if (this.hostStats) {
        rows.push([
          "인코더 비트레이트",
          `${this.hostStats.encoderKbps} kbps (손실 ${this.hostStats.lossPct}%)`,
        ]);
      }
      getHud().setRows(rows);
    });
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

  /** The `a=fingerprint:` line of an SDP — identifies the peer's DTLS identity. */
  private static fingerprintOf(sdp: string): string | null {
    const m = sdp.match(/^a=fingerprint:.*$/m);
    return m ? m[0].trim() : null;
  }

  private async handleOffer(msg: OfferMsg): Promise<void> {
    // An offer from a NEW host webrtcbin (session rebuild: monitor switch, host-side
    // restart) must start from a clean peer connection — an existing pc cannot adopt
    // a different DTLS identity. The `restart` control message already resets the pc
    // eagerly; this fingerprint check is the authoritative fallback when that message
    // was lost. A same-fingerprint re-offer (ICE restart) still renegotiates on the
    // existing pc.
    if (this.pc) {
      const newFp = ClientConnection.fingerprintOf(msg.sdp);
      const curFp = this.pc.remoteDescription
        ? ClientConnection.fingerprintOf(this.pc.remoteDescription.sdp)
        : null;
      if (newFp !== null && curFp !== null && newFp !== curFp) {
        this.resetPeerConnection();
      }
    }
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
  // Control channel (JSON): hello/stats/restart/error from the host;
  // switch-monitor / set-bitrate to the host. Drives the HUD.
  // -------------------------------------------------------------------------

  private attachControlChannel(channel: RTCDataChannel): void {
    this.controlChannel = channel;
    channel.onmessage = (ev) => {
      if (typeof ev.data !== "string") return;
      const msg = parseControlMessage(ev.data);
      if (msg) this.handleControl(msg);
    };
    channel.onopen = () => {
      // Quality presets picked in the HUD go to the host as `set-bitrate`.
      getHud().onBitrate = (kbps) => {
        this.sendControl(controlSetBitrate(kbps));
        this.setStatus(kbps === 0 ? "비트레이트: 자동(호스트 기본)" : `비트레이트 상한: ${kbps / 1000} Mbps`);
      };
    };
    channel.onclose = () => {
      if (this.controlChannel === channel) this.controlChannel = null;
    };
  }

  private sendControl(json: string): void {
    const ch = this.controlChannel;
    if (ch && ch.readyState === "open") ch.send(json);
  }

  private handleControl(msg: ControlMsg): void {
    switch (msg.type) {
      case "hello":
        this.hostInfo = { encoder: msg.encoder, abr: msg.abr };
        this.hostFileTransfer = msg.fileTransfer;
        this.monitors = msg.monitors;
        this.renderMonitors();
        break;
      case "stats":
        this.hostStats = msg;
        break;
      case "restart":
        // The host is tearing down + rebuilding the WebRTC session (e.g. monitor
        // switch). Reset now and answer the fresh offer that follows; the
        // signaling socket stays up throughout. (If this message is lost, the
        // offer's changed DTLS fingerprint triggers the same reset.)
        this.setStatus(`세션 재시작(${msg.reason}) — 새 offer 대기`);
        this.resetPeerConnection();
        break;
      case "error":
        this.setStatus(`호스트 거부: ${msg.message}`);
        // A rejected switch-monitor leaves its button disabled with a pending
        // marker — re-render the picker so the user can try again.
        this.renderMonitors();
        break;
    }
  }

  /** (Re)render the HUD monitor picker from the last `hello` list. */
  private renderMonitors(): void {
    if (this.monitors.length === 0) return;
    getHud().setMonitors(this.monitors, (index) => {
      this.setStatus(`모니터 ${index + 1}(으)로 전환 중…`);
      this.sendControl(controlSwitchMonitor(index));
    });
  }

  // -------------------------------------------------------------------------
  // File channel: drag & drop files onto the stage -> chunked upload with
  // backpressure; the host replies accept/reject and acks completion.
  // -------------------------------------------------------------------------

  private attachFileChannel(channel: RTCDataChannel): void {
    channel.binaryType = "arraybuffer";
    channel.bufferedAmountLowThreshold = ClientConnection.FILE_BUFFER_LOW;
    this.fileChannel = channel;
    channel.onmessage = (ev) => {
      if (!(ev.data instanceof ArrayBuffer)) return;
      const reply = decodeFileReply(ev.data);
      if (!reply) return;
      const waiter = this.fileReplyWaiters.get(reply.id);
      if (waiter) {
        waiter(reply);
      } else {
        this.fileReplies.set(reply.id, reply); // e.g. a mid-transfer reject
      }
    };
    channel.onopen = () => this.installDropTargets();
    channel.onclose = () => {
      if (this.fileChannel === channel) this.fileChannel = null;
    };
  }

  private installDropTargets(): void {
    if (this.dropCleanup.length > 0) return;
    const target = document.getElementById("stage") ?? document.body;
    const onDragOver = (ev: Event): void => {
      ev.preventDefault();
      const de = ev as DragEvent;
      if (de.dataTransfer) de.dataTransfer.dropEffect = "copy";
    };
    const onDrop = (ev: Event): void => {
      ev.preventDefault();
      const files = (ev as DragEvent).dataTransfer?.files;
      if (files && files.length > 0) this.enqueueFiles(Array.from(files));
    };
    target.addEventListener("dragover", onDragOver);
    target.addEventListener("drop", onDrop);
    this.dropCleanup.push(() => {
      target.removeEventListener("dragover", onDragOver);
      target.removeEventListener("drop", onDrop);
    });
  }

  private removeDropTargets(): void {
    for (const cleanup of this.dropCleanup) cleanup();
    this.dropCleanup = [];
  }

  private enqueueFiles(files: File[]): void {
    if (!this.fileChannel || this.fileChannel.readyState !== "open") {
      this.setStatus("파일 전송 불가: 파일 채널이 열려있지 않음");
      return;
    }
    // Only trust the flag once the hello actually arrived (channel-open order
    // between "file" and "control" is not guaranteed).
    if (this.hostInfo && !this.hostFileTransfer) {
      this.setStatus("호스트가 파일 수신을 비활성화함 (FILES=0)");
      return;
    }
    this.fileSendQueue.push(...files);
    void this.drainFileQueue();
  }

  /** Send queued files one at a time (the ordered channel is shared). */
  private async drainFileQueue(): Promise<void> {
    if (this.fileSending) return;
    this.fileSending = true;
    try {
      let file = this.fileSendQueue.shift();
      while (file) {
        await this.sendOneFile(file);
        file = this.fileSendQueue.shift();
      }
    } finally {
      this.fileSending = false;
    }
  }

  private async sendOneFile(file: File): Promise<void> {
    const id = this.nextTransferId++;
    const row = getHud().fileRow(file.name);
    const ch = this.fileChannel;
    if (!ch || ch.readyState !== "open") {
      row.finish(false, "채널 닫힘");
      return;
    }
    try {
      ch.send(encodeFileOffer(id, file.size, file.name));
      const verdict = await this.waitFileReply(id, 15_000);
      if (verdict.kind !== "accept") {
        row.finish(false, verdict.kind === "reject" ? verdict.reason : "거부됨");
        return;
      }

      // Read in ~1 MiB blocks (one async Blob round-trip per block), then slice
      // into 16 KiB channel frames — per-frame Blob reads cap throughput far
      // below what the link can carry.
      const BLOCK_BYTES = 1024 * 1024;
      let offset = 0;
      let lastPaint = 0;
      while (offset < file.size) {
        const block = new Uint8Array(
          await file.slice(offset, offset + BLOCK_BYTES).arrayBuffer(),
        );
        if (block.byteLength === 0) throw new Error("파일을 읽을 수 없음 (변경/삭제됨?)");
        for (let p = 0; p < block.byteLength; p += ClientConnection.FILE_CHUNK_BYTES) {
          if (ch.readyState !== "open") throw new Error("연결 끊김");
          // A mid-transfer reject from the host (write error, size breach) aborts.
          const mid = this.fileReplies.get(id);
          if (mid?.kind === "reject") {
            this.fileReplies.delete(id);
            throw new Error(mid.reason);
          }
          if (ch.bufferedAmount > ClientConnection.FILE_BUFFER_HIGH) {
            await this.waitBufferedLow(ch);
          }
          ch.send(
            encodeFileChunk(
              id,
              block.subarray(p, Math.min(p + ClientConnection.FILE_CHUNK_BYTES, block.byteLength)),
            ),
          );
        }
        offset += block.byteLength;
        const now = performance.now();
        if (now - lastPaint > 150 || offset >= file.size) {
          lastPaint = now;
          row.setProgress(file.size > 0 ? offset / file.size : 1);
        }
      }

      ch.send(encodeFileDone(id));
      const fin = await this.waitFileReply(id, 30_000);
      if (fin.kind === "done") {
        row.finish(true, "저장 완료");
      } else {
        row.finish(false, fin.kind === "reject" ? fin.reason : "확인 실패");
      }
    } catch (err) {
      const reason = err instanceof Error ? err.message : String(err);
      // Tell the host so it frees the transfer slot and deletes the .part file —
      // otherwise a client-side failure leaks the slot for the whole session.
      this.trySendFileReject(id, reason);
      row.finish(false, reason);
    } finally {
      this.fileReplies.delete(id);
      this.fileReplyWaiters.delete(id);
    }
  }

  /** Best-effort client-side cancel so the host aborts + cleans up the transfer. */
  private trySendFileReject(id: number, reason: string): void {
    const ch = this.fileChannel;
    if (ch && ch.readyState === "open") {
      try {
        ch.send(encodeFileReject(id, reason));
      } catch {
        /* channel died mid-send — host cleans up on session teardown */
      }
    }
  }

  /** Await the next host reply for transfer `id` (or one that already arrived). */
  private waitFileReply(id: number, timeoutMs: number): Promise<FileReply> {
    const existing = this.fileReplies.get(id);
    if (existing) {
      this.fileReplies.delete(id);
      return Promise.resolve(existing);
    }
    return new Promise<FileReply>((resolve, reject) => {
      const timer = setTimeout(() => {
        this.fileReplyWaiters.delete(id);
        reject(new Error("호스트 응답 시간 초과"));
      }, timeoutMs);
      this.fileReplyWaiters.set(id, (reply) => {
        clearTimeout(timer);
        this.fileReplyWaiters.delete(id);
        resolve(reply);
      });
    });
  }

  /** Resolve when the channel's send buffer drains below the low-water mark. */
  private waitBufferedLow(ch: RTCDataChannel): Promise<void> {
    return new Promise((resolve) => {
      let poll: ReturnType<typeof setInterval> | null = null;
      const done = (): void => {
        ch.removeEventListener("bufferedamountlow", done);
        if (poll) clearInterval(poll);
        resolve();
      };
      ch.addEventListener("bufferedamountlow", done);
      // Poll as a backstop: the event can be missed if the buffer already drained.
      poll = setInterval(() => {
        if (ch.readyState !== "open" || ch.bufferedAmount <= ch.bufferedAmountLowThreshold) {
          done();
        }
      }, 200);
    });
  }

  /** Fail any transfer still waiting on a host reply (session went away). */
  private failPendingFileTransfers(reason: string): void {
    const waiters = [...this.fileReplyWaiters.values()];
    this.fileReplyWaiters.clear();
    this.fileReplies.clear();
    this.fileSendQueue = [];
    for (const w of waiters) {
      w({ kind: "reject", id: 0, reason });
    }
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
    // Control/file channels die with the pc; fail in-flight transfers cleanly.
    this.failPendingFileTransfers("세션 종료");
    this.removeDropTargets();
    this.controlChannel = null;
    this.fileChannel = null;
    this.hostStats = null;
    this.monitors = [];
    this.prevRates = null;
    getHud().clearSession();
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
// HUD overlay (stats / monitor picker / bitrate presets / file progress).
//
// Built ENTIRELY from the renderer so every page that loads renderer.js gets it
// — the Electron index.html, rcd-signal's served test page, and any future page
// — without duplicating HTML. Interactions here never reach the host input path
// (mouse listeners live on the <video>; keyboard pauses while a form control has
// focus via typingInUi()).
// ---------------------------------------------------------------------------

const HUD_CSS = `
  #rcdHudBtn { position:absolute; top:8px; right:8px; z-index:20; width:34px; height:30px;
    border:1px solid #33333a; border-radius:6px; background:rgba(27,27,31,.8); color:#eee;
    font-size:15px; cursor:pointer; opacity:.45; }
  #rcdHudBtn:hover, #rcdHudBtn.open { opacity:1; }
  #rcdHud { position:absolute; top:44px; right:8px; z-index:20; min-width:250px; max-width:320px;
    background:rgba(20,20,24,.93); border:1px solid #33333a; border-radius:8px; padding:10px 12px;
    font-size:12px; color:#ddd; display:none; }
  #rcdHud.open { display:block; }
  #rcdHud h4 { margin:8px 0 4px; font-size:11px; text-transform:uppercase; letter-spacing:.06em;
    color:#8a8a95; font-weight:600; }
  #rcdHud h4:first-child { margin-top:0; }
  #rcdHud table { width:100%; border-collapse:collapse; }
  #rcdHud td { padding:1px 0; vertical-align:top; }
  #rcdHud td:first-child { color:#9a9aa5; padding-right:10px; white-space:nowrap; }
  #rcdHud td:last-child { text-align:right; font-variant-numeric:tabular-nums; }
  .rcdMonBtn { margin:2px 4px 2px 0; padding:3px 8px; font-size:12px; border-radius:4px;
    border:1px solid #3a3a44; background:#26262c; color:#ddd; cursor:pointer; }
  .rcdMonBtn.current { border-color:#2d6cdf; background:#20304f; cursor:default; }
  #rcdHud select { width:100%; margin-top:2px; background:#101014; color:#eee;
    border:1px solid #33333a; border-radius:4px; padding:3px 4px; font-size:12px; }
  #rcdHud .rcdHint { color:#77777f; margin-top:6px; font-size:11px; }
  #rcdFiles { position:absolute; left:8px; bottom:8px; z-index:20; display:flex;
    flex-direction:column; gap:4px; max-width:340px; font-size:12px; }
  .rcdFileRow { background:rgba(20,20,24,.93); border:1px solid #33333a; border-radius:6px;
    padding:5px 9px; color:#ddd; display:flex; gap:8px; align-items:center; }
  .rcdFileRow .n { overflow:hidden; text-overflow:ellipsis; white-space:nowrap; max-width:200px; }
  .rcdFileRow .p { margin-left:auto; font-variant-numeric:tabular-nums; color:#9a9aa5; }
  .rcdFileRow.ok .p { color:#4caf7d; }
  .rcdFileRow.err .p { color:#e06c75; }
`;

/** A live progress row for one outgoing file. */
interface FileRowHandle {
  setProgress(frac: number): void;
  finish(ok: boolean, label: string): void;
}

class HudOverlay {
  private readonly panel: HTMLDivElement;
  private readonly button: HTMLButtonElement;
  private readonly statsTable: HTMLTableElement;
  private readonly monitorsBox: HTMLDivElement;
  private readonly filesBox: HTMLDivElement;
  /** Value cells keyed by row label — rows update in place between rebuilds. */
  private readonly rowCells = new Map<string, HTMLTableCellElement>();
  private rowSignature = "";
  /** Set by the connection while its control channel is open. */
  onBitrate: ((kbps: number) => void) | null = null;

  constructor(container: HTMLElement) {
    const style = document.createElement("style");
    style.textContent = HUD_CSS;
    document.head.appendChild(style);

    this.button = document.createElement("button");
    this.button.id = "rcdHudBtn";
    this.button.type = "button";
    this.button.title = "통계 / 세션 설정";
    this.button.textContent = "📊";
    this.button.addEventListener("click", () => {
      const open = this.panel.classList.toggle("open");
      this.button.classList.toggle("open", open);
    });

    this.panel = document.createElement("div");
    this.panel.id = "rcdHud";

    const statsTitle = document.createElement("h4");
    statsTitle.textContent = "통계";
    this.statsTable = document.createElement("table");

    const monTitle = document.createElement("h4");
    monTitle.textContent = "모니터";
    this.monitorsBox = document.createElement("div");
    this.monitorsBox.textContent = "연결 후 표시됩니다";

    const brTitle = document.createElement("h4");
    brTitle.textContent = "품질 (비트레이트 상한)";
    const select = document.createElement("select");
    for (const [label, kbps] of [
      ["자동 (호스트 기본)", 0],
      ["3 Mbps — 절약", 3000],
      ["5 Mbps", 5000],
      ["8 Mbps", 8000],
      ["12 Mbps — 기본 상한", 12000],
      ["20 Mbps — 고품질", 20000],
      ["30 Mbps — 최고", 30000],
    ] as const) {
      const opt = document.createElement("option");
      opt.value = String(kbps);
      opt.textContent = label;
      select.appendChild(opt);
    }
    select.addEventListener("change", () => {
      this.onBitrate?.(Number(select.value));
    });

    const hint = document.createElement("div");
    hint.className = "rcdHint";
    hint.textContent = "파일을 화면에 드래그하면 호스트 다운로드 폴더로 전송됩니다.";

    this.panel.append(statsTitle, this.statsTable, monTitle, this.monitorsBox, brTitle, select, hint);

    this.filesBox = document.createElement("div");
    this.filesBox.id = "rcdFiles";

    container.append(this.button, this.panel, this.filesBox);
  }

  /** Fill the stats table with `rows` of [label, value]. Rows are rebuilt only when
   *  the label set changes; otherwise values update in place (no per-second DOM
   *  churn next to a 60fps video). */
  setRows(rows: Array<[string, string]>): void {
    if (!this.panel.classList.contains("open")) return; // no work while hidden
    const signature = rows.map(([k]) => k).join("|");
    if (signature !== this.rowSignature) {
      this.rowSignature = signature;
      this.rowCells.clear();
      this.statsTable.replaceChildren(
        ...rows.map(([k, v]) => {
          const tr = document.createElement("tr");
          const kd = document.createElement("td");
          kd.textContent = k;
          const vd = document.createElement("td");
          vd.textContent = v;
          tr.append(kd, vd);
          this.rowCells.set(k, vd);
          return tr;
        }),
      );
      return;
    }
    for (const [k, v] of rows) {
      const cell = this.rowCells.get(k);
      if (cell && cell.textContent !== v) cell.textContent = v;
    }
  }

  /** Render the monitor picker from the host's `hello`. */
  setMonitors(monitors: MonitorInfo[], onSwitch: (index: number) => void): void {
    this.monitorsBox.replaceChildren(
      ...monitors.map((m) => {
        const b = document.createElement("button");
        b.type = "button";
        b.className = "rcdMonBtn" + (m.current ? " current" : "");
        b.textContent = `${m.index + 1}: ${m.width}×${m.height}${m.primary ? " ★" : ""}`;
        b.title = m.current ? "현재 캡처 중" : "이 모니터로 전환";
        if (!m.current) {
          b.addEventListener("click", () => {
            // Lock the whole row while the switch is in flight — a second click on
            // another monitor would queue a second full session rebuild.
            for (const el of this.monitorsBox.querySelectorAll("button")) {
              (el as HTMLButtonElement).disabled = true;
            }
            b.textContent += " …";
            onSwitch(m.index);
          });
        } else {
          b.disabled = true;
        }
        return b;
      }),
    );
  }

  /** Connection went away: clear per-session content (panel stays available). */
  clearSession(): void {
    this.statsTable.replaceChildren();
    this.rowCells.clear();
    this.rowSignature = "";
    this.monitorsBox.replaceChildren();
    this.monitorsBox.textContent = "연결 후 표시됩니다";
    this.onBitrate = null;
  }

  /** Add a progress row for one outgoing file. */
  fileRow(name: string): FileRowHandle {
    const row = document.createElement("div");
    row.className = "rcdFileRow";
    const n = document.createElement("span");
    n.className = "n";
    n.textContent = name;
    const p = document.createElement("span");
    p.className = "p";
    p.textContent = "0%";
    row.append(n, p);
    this.filesBox.appendChild(row);
    return {
      setProgress: (frac: number): void => {
        p.textContent = `${Math.min(100, Math.round(frac * 100))}%`;
      },
      finish: (ok: boolean, label: string): void => {
        row.classList.add(ok ? "ok" : "err");
        p.textContent = ok ? `✓ ${label}` : `✗ ${label}`;
        setTimeout(() => row.remove(), ok ? 6000 : 12000);
      },
    };
  }
}

let hudSingleton: HudOverlay | null = null;

/** The page-wide HUD singleton (main() creates it eagerly at startup; lazy here
 *  only so module-load order can never bite). */
function getHud(): HudOverlay {
  if (!hudSingleton) {
    hudSingleton = new HudOverlay(document.getElementById("stage") ?? document.body);
  }
  return hudSingleton;
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

  // Build the HUD up front so the 📊 toggle is discoverable before connecting.
  getHud();

  // Block the default drop behaviour EVERYWHERE (Electron would otherwise
  // navigate to the dropped file's URL, replacing the app). The stage's own
  // drop handler (installed while the file channel is open) does the upload.
  window.addEventListener("dragover", (ev) => ev.preventDefault());
  window.addEventListener("drop", (ev) => ev.preventDefault());

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
