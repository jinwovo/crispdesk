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

import { WebSocketServer, WebSocket, type RawData } from "ws";

// ---------------------------------------------------------------------------
// Config (env with M1 defaults)
// ---------------------------------------------------------------------------

const PORT = Number(process.env.PORT ?? 8080);
const PAIRING_CODE = process.env.PAIRING_CODE ?? "123456";
const WS_PATH = "/ws"; // normative endpoint path

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
}

let nextPeerId = 1;

/** Side-table mapping a live socket to its state (avoids augmenting the ws type). */
const peers = new Map<WebSocket, PeerState>();

/**
 * Rooms keyed by pairing code. Each room holds at most 2 sockets.
 * A room is created lazily on first join and deleted when it becomes empty.
 */
const rooms = new Map<string, Set<WebSocket>>();

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

function handleJoin(ws: WebSocket, state: PeerState, msg: JoinMessage): void {
  // Validate the message shape minimally.
  if (typeof msg.room !== "string" || (msg.role !== "host" && msg.role !== "client")) {
    rejectAndClose(ws, "invalid join: 'room' must be a string and 'role' must be 'host' or 'client'");
    return;
  }

  // M1 pairing auth STUB: the room code must equal PAIRING_CODE.
  // TODO(M4): replace with real account/pairing-code auth + per-pair rooms.
  if (msg.room !== PAIRING_CODE) {
    console.warn(`[join] ${describe(state)} rejected: wrong pairing code`);
    rejectAndClose(ws, "invalid pairing code");
    return;
  }

  // A peer may only join once.
  if (state.room !== null) {
    rejectAndClose(ws, "already joined a room");
    return;
  }

  // Get-or-create the room.
  let set = rooms.get(msg.room);
  if (!set) {
    set = new Set<WebSocket>();
    rooms.set(msg.room, set);
  }

  // Enforce max 2 peers per room.
  if (set.size >= 2) {
    console.warn(`[join] ${describe(state)} rejected: room '${msg.room}' is full`);
    rejectAndClose(ws, "room is full");
    return;
  }

  // Disallow two peers claiming the SAME role (we need exactly one host + one client).
  for (const peer of set) {
    const peerState = peers.get(peer);
    if (peerState?.role === msg.role) {
      console.warn(`[join] ${describe(state)} rejected: role '${msg.role}' already taken in room '${msg.room}'`);
      rejectAndClose(ws, `role '${msg.role}' already present in this room`);
      return;
    }
  }

  // Commit the join.
  state.role = msg.role;
  state.room = msg.room;
  set.add(ws);

  const peerCount = set.size;
  console.log(`[join] ${describe(state)} joined room '${msg.room}' (peers=${peerCount})`);

  // Ack the joining peer.
  send(ws, { type: "joined", role: msg.role, peers: peerCount });

  // Notify the OTHER peer (if any) that this one joined.
  const other = otherPeerIn(msg.room, ws);
  if (other) {
    send(other, { type: "peer-joined", role: msg.role });
  }
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

      // Drop empty rooms so codes can be reused cleanly.
      if (set.size === 0) {
        rooms.delete(state.room);
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
    <input id="pairingCode" type="text" value="123456" spellcheck="false" />
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

wss.on("connection", (ws: WebSocket) => {
  const state: PeerState = { id: nextPeerId++, role: null, room: null };
  peers.set(ws, state);
  console.log(`[conn] ${describe(state)} connected`);

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
  console.log(`  PAIRING_CODE = "${PAIRING_CODE}" (M1 stub; TODO(M4): real auth)`);
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

process.on("SIGINT", () => shutdown("SIGINT"));
process.on("SIGTERM", () => shutdown("SIGTERM"));
