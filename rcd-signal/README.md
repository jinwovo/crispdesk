# rcd-signal

Signaling server for **rcd** (remote control desktop). A tiny WebSocket relay
that brokers a single WebRTC connection between the **host** (Rust + GStreamer,
the offerer) and the **client** (Electron, the answerer).

This server is the **source of truth** for the signaling protocol — see
`../PROTOCOL.md` for the normative message shapes. The host and client must both
speak exactly what this server implements.

## Requirements

- Node.js 18+ (verified on Node v24 / npm 11). No other toolchain needed.

## Install & run

```sh
npm install      # install ws + TypeScript toolchain
npm run build    # tsc -> compiles src/server.ts to dist/server.js
npm start        # node dist/server.js
```

Or in one step during development:

```sh
npm run dev      # build + run
```

On start you should see:

```
rcd-signal listening on ws://0.0.0.0:8080/ws
  PAIRING_CODE = "123456" (M1 stub; TODO(M4): real auth)
```

## Environment variables

| Var            | Default     | Meaning                                              |
| -------------- | ----------- | ---------------------------------------------------- |
| `PORT`         | `8080`      | TCP port the WebSocket server listens on.            |
| `PAIRING_CODE` | `"123456"`  | M1 pairing-code stub. Joins with a different `room` code are rejected. |

Example:

```sh
# PowerShell
$env:PORT=9000; $env:PAIRING_CODE="abcdef"; npm start

# bash
PORT=9000 PAIRING_CODE=abcdef npm start
```

## Endpoint

```
ws://<server>:<PORT>/ws        (path is exactly "/ws")
```

## What it does

- **Rooms** are keyed by the pairing code; each room holds at most **2 peers**
  (one `host` + one `client`).
- On `join` it validates the code against `PAIRING_CODE` (M1 stub), acks with
  `joined`, and notifies the other peer with `peer-joined`.
- It **relays** `offer` / `answer` / `ice` verbatim to *the other peer in the
  same room* — it never inspects or transforms SDP/ICE.
- On disconnect it notifies the remaining peer with `peer-left` and drops empty
  rooms.
- Wrong pairing code, full room, or a duplicate role -> `error` message + close.

The server never trusts a client-claimed identity beyond its `role`; the relay
target is always resolved as "the other peer in my room".

## Roadmap notes

- **M1b**: TURN (coturn) is configured on the client/host side; the signaling
  server is unaffected.
- **M4**: replace the `PAIRING_CODE` stub with real account/pairing auth.
