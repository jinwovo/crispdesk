//! Embedded signaling server (the "no separate server" path).
//!
//! Lets the HOST run signaling IN-PROCESS so a LAN / Tailscale setup is literally just
//! two apps: the host here, and a client elsewhere. The host's own webrtc client
//! connects to `ws://127.0.0.1:<port>/ws`; a remote client connects to
//! `ws://<host-ip>:<port>/ws` and presents the PIN. This is a minimal single-room relay
//! that speaks the SAME `SignalMessage` protocol as rcd-signal, so `webrtc.rs` and the
//! client are unchanged.
//!
//! Scope: exactly one host + one client. The host peer (loopback) is trusted; a CLIENT
//! must present the PIN. We relay offer/answer/ice and, on join, send the peer its
//! ice-servers (STUN) and — to the host — a `code-assigned` so the existing host code
//! prints the PIN for the user to read out.

use std::sync::Arc;

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use tokio::net::TcpListener;
use tokio::sync::{mpsc, Mutex};
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::signaling::{IceServerCfg, Role, SignalMessage};

type PeerTx = mpsc::UnboundedSender<SignalMessage>;

#[derive(Default)]
struct Room {
    host: Option<PeerTx>,
    client: Option<PeerTx>,
}

/// Run the embedded signaling server until the process exits. `pin` is what a remote
/// client must present; the local host peer is accepted without it.
pub async fn serve(port: u16, pin: String) -> Result<()> {
    let listener = TcpListener::bind(("0.0.0.0", port)).await?;
    tracing::info!("embedded signaling listening on 0.0.0.0:{port} (path /ws)");
    let room = Arc::new(Mutex::new(Room::default()));

    loop {
        let (stream, addr) = listener.accept().await?;
        let room = room.clone();
        let pin = pin.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_conn(stream, room, pin).await {
                tracing::debug!("embedded signaling conn {addr} ended: {e}");
            }
        });
    }
}

async fn handle_conn(stream: tokio::net::TcpStream, room: Arc<Mutex<Room>>, pin: String) -> Result<()> {
    let ws = tokio_tungstenite::accept_async(stream).await?;
    let (mut sink, mut source) = ws.split();

    // Outbound pump: relayed messages -> this socket.
    let (tx, mut rx) = mpsc::unbounded_channel::<SignalMessage>();
    let pump = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            match serde_json::to_string(&msg) {
                Ok(json) => {
                    if sink.send(WsMessage::Text(json.into())).await.is_err() {
                        break;
                    }
                }
                Err(e) => tracing::warn!("embedded: serialize failed: {e}"),
            }
        }
    });

    let mut my_role: Option<Role> = None;

    while let Some(frame) = source.next().await {
        let WsMessage::Text(text) = frame? else {
            continue;
        };
        let msg: SignalMessage = match serde_json::from_str(&text) {
            Ok(m) => m,
            Err(e) => {
                tracing::debug!("embedded: bad message: {e}");
                continue;
            }
        };

        match msg {
            SignalMessage::Join { room: code, role } => {
                // A remote CLIENT must present the PIN; the local host is trusted.
                if role == Role::Client && code != pin {
                    let _ = tx.send(SignalMessage::Error {
                        message: "invalid pairing code".into(),
                    });
                    break;
                }
                my_role = Some(role);

                let mut r = room.lock().await;
                match role {
                    Role::Host => r.host = Some(tx.clone()),
                    Role::Client => r.client = Some(tx.clone()),
                }
                let peers = r.host.is_some() as i64 + r.client.is_some() as i64;

                let _ = tx.send(SignalMessage::Joined { role, peers });
                // STUN-only ICE servers (LAN/Tailscale needs no TURN).
                let _ = tx.send(SignalMessage::IceServers {
                    ice_servers: vec![IceServerCfg {
                        urls: vec!["stun:stun.l.google.com:19302".into()],
                        username: None,
                        credential: None,
                    }],
                });
                // Let the host print the PIN through its existing code-assigned handler.
                if role == Role::Host {
                    let _ = tx.send(SignalMessage::CodeAssigned {
                        code: pin.clone(),
                        expires_at: 0,
                    });
                }
                // Notify the OTHER peer that this one joined.
                let other = match role {
                    Role::Host => r.client.as_ref(),
                    Role::Client => r.host.as_ref(),
                };
                if let Some(o) = other {
                    let _ = o.send(SignalMessage::PeerJoined { role });
                }
            }

            // Relay SDP/ICE to the other peer verbatim.
            relayed @ (SignalMessage::Offer { .. }
            | SignalMessage::Answer { .. }
            | SignalMessage::Ice { .. }) => {
                let r = room.lock().await;
                let other = match my_role {
                    Some(Role::Host) => r.client.as_ref(),
                    Some(Role::Client) => r.host.as_ref(),
                    None => None,
                };
                if let Some(o) = other {
                    let _ = o.send(relayed);
                }
            }

            other => tracing::debug!("embedded: ignoring {other:?}"),
        }
    }

    // Disconnect: drop self and tell the other peer.
    {
        let mut r = room.lock().await;
        match my_role {
            Some(Role::Host) => {
                r.host = None;
                if let Some(o) = r.client.as_ref() {
                    let _ = o.send(SignalMessage::PeerLeft { role: Role::Host });
                }
            }
            Some(Role::Client) => {
                r.client = None;
                if let Some(o) = r.host.as_ref() {
                    let _ = o.send(SignalMessage::PeerLeft { role: Role::Client });
                }
            }
            None => {}
        }
    }
    pump.abort();
    Ok(())
}

/// Generate a short PIN from OS-seeded randomness (RandomState seeds from the OS), using
/// the unambiguous alphabet. No external RNG dependency.
pub fn generate_pin(len: usize) -> String {
    use std::hash::{BuildHasher, Hasher};
    const ALPHABET: &[u8] = b"23456789ABCDEFGHJKMNPQRSTUVWXYZ";
    let mut out = String::with_capacity(len);
    for i in 0..len {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_usize(i);
        let r = h.finish();
        out.push(ALPHABET[(r % ALPHABET.len() as u64) as usize] as char);
    }
    out
}
