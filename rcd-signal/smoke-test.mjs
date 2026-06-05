/**
 * rcd-signal smoke test — verifies the signaling relay end-to-end WITHOUT
 * needing GStreamer or Electron. Connects two ws clients (a fake "host" and a
 * fake "client"), runs the M1 join -> peer-joined -> offer -> answer -> ice
 * sequence, and asserts every message is relayed verbatim with field names /
 * casing preserved (especially sdpMid / sdpMLineIndex).
 *
 * Usage: start the server (node dist/server.js) on :8080, then `node smoke-test.mjs`.
 * Exits 0 on PASS, 1 on FAIL. Has a hard 5s timeout so it can never hang.
 */
import { WebSocket } from "ws";

const URL = process.env.SIGNAL_URL ?? "ws://127.0.0.1:8080/ws";
const CODE = process.env.PAIRING_CODE ?? "123456";

const passed = [];
let done = false;

function finish(code, failMsg) {
  if (done) return;
  done = true;
  clearTimeout(timer);
  if (failMsg) console.error("FAIL:", failMsg);
  try { host.close(); } catch {}
  try { client.close(); } catch {}
  if (code === 0) {
    console.log("PASS — signaling relay works end-to-end:");
    for (const p of passed) console.log("  ✓ " + p);
  }
  // give sockets a tick to close, then exit
  setTimeout(() => process.exit(code), 50);
}

const timer = setTimeout(() => finish(1, "timed out (5s) waiting for expected messages"), 5000);

let hostJoined = false, clientJoined = false, hostSawPeerJoined = false;
let clientGotOffer = false, hostGotAnswer = false, hostGotIce = false;

const host = new WebSocket(URL);
const client = new WebSocket(URL);

host.on("error", (e) => finish(1, "host ws error: " + e.message));
client.on("error", (e) => finish(1, "client ws error: " + e.message));

host.on("open", () => host.send(JSON.stringify({ type: "join", room: CODE, role: "host" })));
// Join the client a beat later so host-first ordering is deterministic.
client.on("open", () => setTimeout(
  () => client.send(JSON.stringify({ type: "join", room: CODE, role: "client" })), 200));

host.on("message", (d) => handle("host", JSON.parse(d.toString())));
client.on("message", (d) => handle("client", JSON.parse(d.toString())));

function maybeDone() {
  if (hostJoined && clientJoined && hostSawPeerJoined && clientGotOffer && hostGotAnswer && hostGotIce) {
    finish(0);
  }
}

function handle(who, msg) {
  if (who === "host" && msg.type === "joined" && msg.peers === 1) {
    hostJoined = true; passed.push("host joined room, peers=1");
  }
  if (who === "client" && msg.type === "joined" && msg.peers === 2) {
    clientJoined = true; passed.push("client joined room, peers=2");
  }
  if (who === "host" && msg.type === "peer-joined" && msg.role === "client") {
    hostSawPeerJoined = true; passed.push("host notified of peer-joined(client)");
    host.send(JSON.stringify({ type: "offer", sdp: "SDP_OFFER_TEST" }));
  }
  if (who === "client" && msg.type === "offer" && msg.sdp === "SDP_OFFER_TEST") {
    clientGotOffer = true; passed.push("client received host's offer verbatim");
    client.send(JSON.stringify({ type: "answer", sdp: "SDP_ANSWER_TEST" }));
    client.send(JSON.stringify({ type: "ice", candidate: "CAND_TEST", sdpMid: "0", sdpMLineIndex: 0 }));
  }
  if (who === "host" && msg.type === "answer" && msg.sdp === "SDP_ANSWER_TEST") {
    hostGotAnswer = true; passed.push("host received client's answer verbatim");
  }
  if (who === "host" && msg.type === "ice" &&
      msg.candidate === "CAND_TEST" && msg.sdpMid === "0" && msg.sdpMLineIndex === 0) {
    hostGotIce = true; passed.push("host received ice verbatim (sdpMid/sdpMLineIndex preserved)");
  }
  maybeDone();
}
