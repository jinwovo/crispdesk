/**
 * rcd (remote control desktop) — SIGNALING SERVER
 * ------------------------------------------------
 * A tiny WebSocket relay that brokers a single WebRTC connection between:
 *   - the HOST   (Rust + GStreamer webrtcbin) — the OFFERER, sends video, receives input
 *   - the CLIENT (Electron)                   — the ANSWERER, shows video, sends input
 *
 * This server is the SOURCE OF TRUTH for the signaling protocol. The host and
 * the client must both speak exactly what is implemented here.
 *
 * Transport : WebSocket, endpoint path EXACTLY "/ws"  =>  ws://<server>:8080/ws
 * Rooms     : keyed by pairing code; max 2 peers (one host + one client).
 * Auth (M1) : STUB — compare the joined "room" code to env PAIRING_CODE.
 *             Real account/pairing auth is M4.
 *
 * The server NEVER trusts a client-claimed identity beyond its "role". When it
 * needs to forward a message it resolves the target as "the OTHER peer in my room".
 *
 * Negotiation direction (normative): HOST offers, CLIENT answers. The server
 * does not initiate or transform negotiation — it only relays offer/answer/ice
 * verbatim and notifies peers of join/leave events.
 */

import {
  createServer,
  type IncomingMessage,
  type ServerResponse,
} from "node:http";
import { readFile } from "node:fs/promises";
import * as path from "node:path";
import { fileURLToPath } from "node:url";
import { randomInt } from "node:crypto";

import {
  CODE_ALPHABET,
  generatePairingCode,
  turnCredentials,
  rateLimitCheck,
} from "./pairing.js";

import { WebSocketServer, WebSocket, type RawData } from "ws";

// ---------------------------------------------------------------------------
// Config (env with M1 defaults)
// ---------------------------------------------------------------------------

const PORT = Number(process.env.PORT ?? 8080);
const WS_PATH = "/ws"; // normative endpoint path

// --- Pairing model -----------------------------------------------------------
// DEV_MODE=true keeps the old fixed-code behaviour (room must equal PAIRING_CODE)
// for convenient local testing. Otherwise (default) the server runs DYNAMIC pairing:
// when a HOST joins it is assigned a fresh random code with a TTL, sent back via a
// `code-assigned` message; a CLIENT must redeem that code to join the host's room.
const DEV_MODE = process.env.DEV_MODE === "true";
const PAIRING_CODE = process.env.PAIRING_CODE ?? "123456"; // DEV_MODE only
const CODE_TTL_MS = Number(process.env.CODE_TTL_MS ?? 300_000); // 5 min
// Per-IP client-join rate limit (brute-force defence on short codes).
const JOIN_MAX_ATTEMPTS = Number(process.env.JOIN_MAX_ATTEMPTS ?? 10);
const JOIN_WINDOW_MS = Number(process.env.JOIN_WINDOW_MS ?? 60_000);
// 8 chars over the 31-symbol CODE_ALPHABET ≈ 40 bits of entropy — combined with the
// per-IP rate limit, far harder to brute-force than the old 6 (~29 bits). Override env.
const CODE_LENGTH = Number(process.env.CODE_LENGTH ?? 8);
// In production, the unauthenticated browser test client (GET /) should be OFF so a
// LAN device can't reach the control surface. Set DISABLE_TEST_PAGE=true to gate it.
const DISABLE_TEST_PAGE = process.env.DISABLE_TEST_PAGE === "true";

// --- ICE / TURN -------------------------------------------------------------
// The server issues TIME-LIMITED TURN credentials (coturn `use-auth-secret`
// scheme) so neither peer ships a static TURN password. Configure:
//   TURN_URLS=turn:nas.example.com:3478,turns:nas.example.com:5349
//   TURN_SECRET=<the same static-auth-secret configured in coturn>
//   TURN_TTL_SEC=3600
// With those set, each joining peer is sent an `ice-servers` message carrying a
// STUN entry plus a TURN entry whose username is `<expiry-unix>:rcd` and whose
// credential is base64(HMAC-SHA1(TURN_SECRET, username)). Without them, peers
// fall back to their own built-in STUN (LAN / Tailscale only).
const STUN_URL = process.env.STUN_URL ?? "stun:stun.l.google.com:19302";
const TURN_URLS = (process.env.TURN_URLS ?? "")
  .split(",")
  .map((s) => s.trim())
  .filter(Boolean);
const TURN_SECRET = process.env.TURN_SECRET ?? "";
const TURN_TTL_SEC = Number(process.env.TURN_TTL_SEC ?? 3600);

interface IceServerEntry {
  urls: string[];
  username?: string;
  credential?: string;
}

/** Build the ICE server list for a peer, minting fresh ephemeral TURN credentials.
 *  Returns null when no TURN is configured (peers then use their default STUN). */
function makeIceServers(): IceServerEntry[] | null {
  if (TURN_URLS.length === 0 || TURN_SECRET === "") return null;
  const { username, credential } = turnCredentials(
    TURN_SECRET,
    TURN_TTL_SEC,
    Math.floor(Date.now() / 1000),
  );
  return [{ urls: [STUN_URL] }, { urls: TURN_URLS, username, credential }];
}

// ---------------------------------------------------------------------------
// Protocol message types
//
// These mirror the pinned protocol. Field names are NORMATIVE — the Rust host
// and the Electron client construct/parse these exact shapes.
// ---------------------------------------------------------------------------

type Role = "host" | "client";

/** client/host -> server */
interface JoinMessage {
  type: "join";
  room: string; // the pairing code
  role: Role;
}

/** server -> peer (ack of its own join) */
interface JoinedMessage {
  type: "joined";
  role: Role; // echoes back the role the peer claimed
  peers: number; // peers currently in the room (1 or 2)
}

/** server -> the OTHER peer when someone joins */
interface PeerJoinedMessage {
  type: "peer-joined";
  role: Role; // role of the peer that just joined
}

/** server -> the OTHER peer when someone leaves */
interface PeerLeftMessage {
  type: "peer-left";
  role: Role; // role of the peer that left
}

/** server -> peer on bad code / full room (followed by close) */
interface ErrorMessage {
  type: "error";
  message: string;
}

/** server -> host: the dynamically-issued pairing code (DEV_MODE off) */
interface CodeAssignedMessage {
  type: "code-assigned";
  code: string;
  expiresAt: number; // unix epoch ms
}

/** server -> peer: ICE servers (STUN + ephemeral-credential TURN) to use */
interface IceServersMessage {
  type: "ice-servers";
  iceServers: IceServerEntry[];
}

/** host -> client (relayed verbatim) */
interface OfferMessage {
  type: "offer";
  sdp: string;
}

/** client -> host (relayed verbatim) */
interface AnswerMessage {
  type: "answer";
  sdp: string;
}

/** either -> other (relayed verbatim) — trickle ICE, both directions */
interface IceMessage {
  type: "ice";
  candidate: string;
  sdpMid: string | null;
  sdpMLineIndex: number | null;
}

/** Anything the server is willing to relay between peers without inspecting deeply. */
type RelayMessage = OfferMessage | AnswerMessage | IceMessage;

/** Any inbound message the server understands from a peer. */
type InboundMessage = JoinMessage | RelayMessage;

/** Any outbound message the server may send to a peer. */
type OutboundMessage =
  | JoinedMessage
  | PeerJoinedMessage
  | PeerLeftMessage
  | ErrorMessage
  | CodeAssignedMessage
  | IceServersMessage
  | RelayMessage;

// ---------------------------------------------------------------------------
// Per-connection state
// ---------------------------------------------------------------------------

/**
 * State we attach to each WebSocket. `role`/`room` are populated only after a
 * successful "join". `id` is a process-local monotonic id used purely for logs.
 */
interface PeerState {
  id: number;
  role: Role | null;
  room: string | null;
  ip: string;
}

let nextPeerId = 1;

/** Side-table mapping a live socket to its state (avoids augmenting the ws type). */
const peers = new Map<WebSocket, PeerState>();

/**
 * Rooms keyed by pairing code. Each room holds at most 2 sockets.
 * A room is created lazily on first join and deleted when it becomes empty.
 */
const rooms = new Map<string, Set<WebSocket>>();

// --- Dynamic pairing state (DEV_MODE off) ------------------------------------
/** Active codes -> expiry (unix ms). A code's room name IS the code. */
const codeExpiry = new Map<string, number>();
/** Per-IP recent client-join attempt timestamps (rate limiting). */
const ipAttempts = new Map<string, number[]>();

/** Mint a fresh code not currently active or in use as a room. */
function mintCode(): string {
  for (let tries = 0; tries < 50; tries++) {
    const code = generatePairingCode(CODE_LENGTH, (max) => randomInt(max));
    if (!codeExpiry.has(code) && !rooms.has(code)) return code;
  }
  // Astronomically unlikely; widen with a timestamp suffix as a last resort.
  return `${CODE_ALPHABET[randomInt(CODE_ALPHABET.length)]}${Date.now().toString(36).toUpperCase()}`;
}

/** Drop codes whose TTL has elapsed (and whose room is gone). */
function pruneExpiredCodes(): void {
  const now = Date.now();
  for (const [code, exp] of codeExpiry) {
    if (exp < now) codeExpiry.delete(code);
  }
}

/** Record a client-join attempt for `ip`; return false if over the rate limit. */
function allowJoinAttempt(ip: string): boolean {
  const { allowed, recent } = rateLimitCheck(
    ipAttempts.get(ip) ?? [],
    Date.now(),
    JOIN_WINDOW_MS,
    JOIN_MAX_ATTEMPTS,
  );
  ipAttempts.set(ip, recent);
  return allowed;
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

function send(ws: WebSocket, msg: OutboundMessage): void {
  if (ws.readyState !== WebSocket.OPEN) return;
  ws.send(JSON.stringify(msg));
}

/** Send an error then close the socket. 1008 = policy violation. */
function rejectAndClose(ws: WebSocket, message: string): void {
  send(ws, { type: "error", message });
  ws.close(1008, message);
}

/** Resolve "the OTHER peer in my room" — the heart of the relay. */
function otherPeerIn(room: string, self: WebSocket): WebSocket | null {
  const set = rooms.get(room);
  if (!set) return null;
  for (const ws of set) {
    if (ws !== self) return ws;
  }
  return null;
}

function describe(state: PeerState): string {
  return `peer#${state.id}(role=${state.role ?? "?"},room=${state.room ?? "?"})`;
}

// ---------------------------------------------------------------------------
// Message handling
// ---------------------------------------------------------------------------

/**
 * Commit a validated peer into `room` as `role`: enforce max-2 + one-per-role, set
 * state, ack `joined`, and notify the other peer. Returns the new peer count, or null
 * if rejected (the socket has already been closed in that case).
 */
function commitJoin(
  ws: WebSocket,
  state: PeerState,
  room: string,
  role: Role,
): number | null {
  let set = rooms.get(room);
  if (!set) {
    set = new Set<WebSocket>();
    rooms.set(room, set);
  }
  if (set.size >= 2) {
    rejectAndClose(ws, "room is full");
    return null;
  }
  for (const peer of set) {
    if (peers.get(peer)?.role === role) {
      rejectAndClose(ws, `role '${role}' already present in this room`);
      return null;
    }
  }

  state.role = role;
  state.room = room;
  set.add(ws);
  const peerCount = set.size;

  send(ws, { type: "joined", role, peers: peerCount });
  // Hand the peer its ICE servers (incl. fresh ephemeral TURN creds) up front, so
  // it has them before the host begins negotiating. No-op if TURN isn't configured.
  const ice = makeIceServers();
  if (ice) send(ws, { type: "ice-servers", iceServers: ice });

  const other = otherPeerIn(room, ws);
  if (other) {
    send(other, { type: "peer-joined", role });
  }
  return peerCount;
}

function handleJoin(ws: WebSocket, state: PeerState, msg: JoinMessage): void {
  // Validate the message shape minimally.
  if (typeof msg.room !== "string" || (msg.role !== "host" && msg.role !== "client")) {
    rejectAndClose(ws, "invalid join: 'room' must be a string and 'role' must be 'host' or 'client'");
    return;
  }
  // A peer may only join once.
  if (state.room !== null) {
    rejectAndClose(ws, "already joined a room");
    return;
  }

  // --- DEV_MODE: fixed code, original behaviour ---
  if (DEV_MODE) {
    if (msg.room !== PAIRING_CODE) {
      console.warn(`[join] ${describe(state)} rejected: wrong pairing code (dev)`);
      rejectAndClose(ws, "invalid pairing code");
      return;
    }
    const n = commitJoin(ws, state, msg.room, msg.role);
    if (n !== null) console.log(`[join] ${describe(state)} joined room '${msg.room}' (peers=${n})`);
    return;
  }

  // --- DYNAMIC pairing (default) ---
  if (msg.role === "host") {
    // Server mints a fresh code and binds it to this host's room (= the code).
    const code = mintCode();
    const n = commitJoin(ws, state, code, "host");
    if (n === null) return;
    const expiresAt = Date.now() + CODE_TTL_MS;
    codeExpiry.set(code, expiresAt);
    send(ws, { type: "code-assigned", code, expiresAt });
    console.log(`[join] host ${describe(state)} assigned code ${code} (ttl ${CODE_TTL_MS}ms)`);
    return;
  }

  // Client: rate-limit, then redeem an active, unexpired code.
  if (!allowJoinAttempt(state.ip)) {
    console.warn(`[join] rate-limited client from ${state.ip}`);
    rejectAndClose(ws, "too many join attempts; try again later");
    return;
  }
  // Check this specific code's existence/expiry FIRST so the error is accurate
  // (a background timer handles bulk pruning — see below).
  const code = msg.room.trim().toUpperCase();
  const exp = codeExpiry.get(code);
  if (exp === undefined) {
    rejectAndClose(ws, "invalid pairing code");
    return;
  }
  if (exp < Date.now()) {
    codeExpiry.delete(code);
    rejectAndClose(ws, "pairing code expired");
    return;
  }
  const set = rooms.get(code);
  const hasHost = set !== undefined && [...set].some((p) => peers.get(p)?.role === "host");
  if (!hasHost) {
    rejectAndClose(ws, "host not connected for this code");
    return;
  }
  const n = commitJoin(ws, state, code, "client");
  if (n !== null) console.log(`[join] client ${describe(state)} joined '${code}' (peers=${n})`);
}

function handleRelay(ws: WebSocket, state: PeerState, msg: RelayMessage): void {
  // Must have joined a room before relaying.
  if (state.room === null) {
    rejectAndClose(ws, "must 'join' before sending offer/answer/ice");
    return;
  }

  const other = otherPeerIn(state.room, ws);
  if (!other) {
    // No peer to relay to yet — drop. Peers should only negotiate once both are present.
    console.warn(`[relay] ${describe(state)} sent '${msg.type}' but no other peer in room; dropping`);
    return;
  }

  // Relay verbatim. The server does not inspect/transform SDP or ICE.
  console.log(`[relay] ${describe(state)} -> other: '${msg.type}'`);
  send(other, msg);
}

function handleMessage(ws: WebSocket, state: PeerState, data: RawData): void {
  // Robust JSON parse: never let a malformed frame crash the server.
  let parsed: unknown;
  try {
    parsed = JSON.parse(data.toString());
  } catch {
    rejectAndClose(ws, "invalid JSON");
    return;
  }

  if (typeof parsed !== "object" || parsed === null || typeof (parsed as { type?: unknown }).type !== "string") {
    rejectAndClose(ws, "message must be a JSON object with a string 'type'");
    return;
  }

  const msg = parsed as InboundMessage;

  switch (msg.type) {
    case "join":
      handleJoin(ws, state, msg);
      break;
    case "offer":
    case "answer":
    case "ice":
      handleRelay(ws, state, msg);
      break;
    default:
      // Unknown type: ignore but log. We do not close — forward-compat with
      // future message types added by newer host/client builds.
      console.warn(`[msg] ${describe(state)} sent unknown type '${(msg as { type: string }).type}'; ignoring`);
      break;
  }
}

function handleClose(ws: WebSocket, state: PeerState): void {
  peers.delete(ws);

  if (state.room !== null) {
    const set = rooms.get(state.room);
    if (set) {
      set.delete(ws);

      // Notify the remaining peer (if any) that this one left.
      const other = otherPeerIn(state.room, ws);
      if (other && state.role) {
        send(other, { type: "peer-left", role: state.role });
      }

      // Drop empty rooms so codes can be reused cleanly, and expire the code.
      if (set.size === 0) {
        rooms.delete(state.room);
        codeExpiry.delete(state.room);
      }
    }
  }

  console.log(`[close] ${describe(state)} disconnected (rooms=${rooms.size})`);
}

// ---------------------------------------------------------------------------
// Server bootstrap
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Browser test client (no-install second-device testing).
//
//   GET /            -> standalone page that reuses the Electron client's compiled
//                       renderer.js VERBATIM (it only uses browser APIs), so any
//                       phone/laptop on the LAN can connect, view, and control.
//   GET /renderer.js -> served from ../rcd-client/dist/renderer/renderer.js
//
// The page defaults its signaling URL to wherever it was served from, so on a
// second device you just open http://<host-ip>:8080/ and press Connect.
// ---------------------------------------------------------------------------

const RENDERER_JS = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  "../../rcd-client/dist/renderer/renderer.js",
);

const TEST_PAGE = `<!DOCTYPE html>
<html lang="ko">
<head>
<meta charset="utf-8" />
<meta name="viewport" content="width=device-width, initial-scale=1" />
<title>rcd — browser test client</title>
<style>
  :root { color-scheme: dark; }
  html, body { margin:0; height:100%; background:#000; color:#eee;
    font-family: system-ui, sans-serif; overflow:hidden; }
  body { display:flex; flex-direction:column; }
  #bar { flex:0 0 auto; display:flex; flex-wrap:wrap; align-items:center; gap:8px;
    padding:6px 10px; background:#1b1b1f; border-bottom:1px solid #2a2a30; font-size:13px; }
  #bar input[type=text] { background:#101014; color:#eee; border:1px solid #33333a;
    border-radius:4px; padding:4px 6px; font-size:13px; }
  #signalUrl { width:230px; } #pairingCode { width:80px; }
  #connectBtn { background:#2d6cdf; color:#fff; border:none; border-radius:4px;
    padding:5px 14px; cursor:pointer; font-size:13px; }
  #status { margin-left:auto; opacity:.85; }
  #stage { flex:1 1 auto; position:relative; min-height:0; }
  #remote { position:absolute; inset:0; width:100%; height:100%;
    object-fit:contain; background:#000; }
</style>
</head>
<body>
  <div id="bar">
    <input id="signalUrl" type="text" spellcheck="false" />
    <input id="pairingCode" type="text" placeholder="code from host" spellcheck="false" style="text-transform:uppercase" />
    <button id="connectBtn" type="button">Connect</button>
    <label><input id="forwardInput" type="checkbox" checked /> input</label>
    <span id="status">idle</span>
  </div>
  <div id="stage">
    <video id="remote" autoplay muted playsinline></video>
  </div>
  <script>
    // Default the signaling URL to wherever this page was served from.
    document.getElementById("signalUrl").value =
      (location.protocol === "https:" ? "wss://" : "ws://") + location.host + "/ws";
  </script>
  <script type="module" src="/renderer.js"></script>
</body>
</html>
`;

function handleHttp(req: IncomingMessage, res: ServerResponse): void {
  const u = (req.url ?? "/").split("?")[0];

  // Production hardening: the browser test client is unauthenticated, so it can be
  // disabled entirely (the native Electron client connects over /ws regardless).
  if (DISABLE_TEST_PAGE && (u === "/" || u === "/index.html" || u === "/renderer.js")) {
    res.writeHead(404, { "content-type": "text/plain; charset=utf-8" });
    res.end("not found");
    return;
  }

  if (req.method === "GET" && (u === "/" || u === "/index.html")) {
    res.writeHead(200, { "content-type": "text/html; charset=utf-8" });
    res.end(TEST_PAGE);
    return;
  }

  if (req.method === "GET" && u === "/renderer.js") {
    readFile(RENDERER_JS)
      .then((js) => {
        res.writeHead(200, {
          "content-type": "text/javascript; charset=utf-8",
        });
        res.end(js);
      })
      .catch(() => {
        res.writeHead(404, { "content-type": "text/plain; charset=utf-8" });
        res.end("renderer.js not found — run `npm run build` in rcd-client first");
      });
    return;
  }

  res.writeHead(404, { "content-type": "text/plain; charset=utf-8" });
  res.end("not found");
}

const httpServer = createServer(handleHttp);
const wss = new WebSocketServer({ server: httpServer, path: WS_PATH });

wss.on("connection", (ws: WebSocket, req: IncomingMessage) => {
  const ip = req.socket.remoteAddress ?? "unknown";
  const state: PeerState = { id: nextPeerId++, role: null, room: null, ip };
  peers.set(ws, state);
  console.log(`[conn] ${describe(state)} connected from ${ip}`);

  ws.on("message", (data) => handleMessage(ws, state, data));
  ws.on("close", () => handleClose(ws, state));
  ws.on("error", (err) => {
    // Log socket errors; the 'close' handler performs the actual cleanup.
    console.error(`[error] ${describe(state)}:`, err.message);
  });
});

httpServer.on("error", (err) => {
  console.error("[server] fatal:", err);
  process.exit(1);
});

httpServer.listen(PORT, () => {
  console.log(`rcd-signal listening on ws://0.0.0.0:${PORT}${WS_PATH}`);
  console.log(`  browser test client: http://<this-machine-ip>:${PORT}/`);
  if (DEV_MODE) {
    console.log(`  DEV_MODE on: fixed PAIRING_CODE = "${PAIRING_CODE}"`);
  } else {
    console.log(
      `  dynamic pairing: each host is issued a ${CODE_LENGTH}-char code ` +
        `(TTL ${CODE_TTL_MS}ms, rate-limit ${JOIN_MAX_ATTEMPTS}/${JOIN_WINDOW_MS}ms)`,
    );
  }
  if (TURN_URLS.length > 0 && TURN_SECRET !== "") {
    console.log(`  TURN: issuing ephemeral creds for ${TURN_URLS.join(", ")} (ttl ${TURN_TTL_SEC}s)`);
  } else {
    console.log(`  TURN: not configured (STUN only — LAN/Tailscale). Set TURN_URLS + TURN_SECRET.`);
  }
});

// Graceful shutdown so `npm start` / Ctrl-C does not leak the listener.
function shutdown(signal: string): void {
  console.log(`\n[server] received ${signal}, shutting down`);
  for (const ws of peers.keys()) {
    try {
      ws.close(1001, "server shutting down"); // 1001 = going away
    } catch {
      /* ignore */
    }
  }
  wss.close(() => {
    httpServer.close(() => process.exit(0));
  });
  // Failsafe: force-exit if close hangs.
  setTimeout(() => process.exit(0), 2000).unref();
}

// Periodically prune expired pairing codes (memory housekeeping).
setInterval(pruneExpiredCodes, 60_000).unref();

process.on("SIGINT", () => shutdown("SIGINT"));
process.on("SIGTERM", () => shutdown("SIGTERM"));
