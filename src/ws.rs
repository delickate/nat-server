//! One WebSocket connection per online device.

use std::{
    collections::VecDeque,
    sync::{Arc, atomic::Ordering},
    time::{Duration, Instant},
};

use axum::{
    body::Bytes,
    extract::{
        State,
        ws::{Message, WebSocket, WebSocketUpgrade},
    },
    http::StatusCode,
    response::{IntoResponse, Response},
};
use futures_util::{SinkExt, StreamExt};
use tokio::{
    sync::{Notify, mpsc},
    time::{MissedTickBehavior, interval, sleep, timeout},
};
use uuid::Uuid;

use crate::{
    AppState,
    hub::{Outbound, Peer},
    identity::device_id_from_secret,
    protocol::{BINARY_HEADER_LEN, ClientMessage, ErrorCode, PROTOCOL_VERSION, ServerMessage},
};

/// Largest single message accepted (screen frames will need room).
pub const MAX_MESSAGE_BYTES: usize = 8 * 1024 * 1024;
const REGISTER_TIMEOUT: Duration = Duration::from_secs(10);
/// Server pings every `PING_INTERVAL`; a client silent for `IDLE_TIMEOUT` is dropped.
const PING_INTERVAL: Duration = Duration::from_secs(25);
const IDLE_TIMEOUT: Duration = Duration::from_secs(75);
const OUTBOUND_QUEUE: usize = 512;
const WRITER_DRAIN_TIMEOUT: Duration = Duration::from_secs(5);
/// Connection requests one device may send per window.
const REQUEST_RATE_MAX: usize = 10;
const REQUEST_RATE_WINDOW: Duration = Duration::from_secs(60);

pub async fn ws_handler(State(state): State<AppState>, ws: WebSocketUpgrade) -> Response {
    let Some(slot) = ConnectionSlot::acquire(&state) else {
        return (StatusCode::SERVICE_UNAVAILABLE, "server is at capacity").into_response();
    };
    ws.max_message_size(MAX_MESSAGE_BYTES)
        .max_frame_size(MAX_MESSAGE_BYTES)
        .on_upgrade(move |socket| async move {
            handle_socket(socket, state).await;
            drop(slot);
        })
}

/// Counts open connections against `max_connections`.
struct ConnectionSlot(AppState);

impl ConnectionSlot {
    fn acquire(state: &AppState) -> Option<Self> {
        let max = state.config.max_connections;
        let mut current = state.connections.load(Ordering::Acquire);
        loop {
            if current >= max {
                return None;
            }
            match state.connections.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Some(Self(state.clone())),
                Err(actual) => current = actual,
            }
        }
    }
}

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        self.0.connections.fetch_sub(1, Ordering::AcqRel);
    }
}

fn to_frame(message: &ServerMessage) -> Message {
    Message::Text(
        serde_json::to_string(message)
            .expect("server messages always serialize")
            .into(),
    )
}

/// Waits for a valid `register` message and returns the proven device ID.
async fn handshake(socket: &mut WebSocket) -> Option<Uuid> {
    let text = match timeout(REGISTER_TIMEOUT, socket.recv()).await {
        Ok(Some(Ok(Message::Text(text)))) => text,
        _ => return None,
    };

    let result = match serde_json::from_str::<ClientMessage>(text.as_str()) {
        Ok(ClientMessage::Register { protocol_version, device_secret, client_version }) => {
            if protocol_version != PROTOCOL_VERSION {
                tracing::info!(protocol_version, %client_version, "rejected unsupported client");
                Err(ErrorCode::UnsupportedVersion)
            } else {
                device_id_from_secret(&device_secret).ok_or(ErrorCode::InvalidSecret)
            }
        }
        _ => Err(ErrorCode::NotRegistered),
    };

    match result {
        Ok(device_id) => Some(device_id),
        Err(code) => {
            let _ = socket.send(to_frame(&ServerMessage::error(code, None))).await;
            let _ = socket.send(Message::Close(None)).await;
            None
        }
    }
}

async fn handle_socket(mut socket: WebSocket, state: AppState) {
    let Some(device_id) = handshake(&mut socket).await else {
        return;
    };

    let (tx, mut rx) = mpsc::channel::<Outbound>(OUTBOUND_QUEUE);
    let shutdown = Arc::new(Notify::new());
    let peer = Peer { tx, shutdown: shutdown.clone() };
    let conn_id = state.hub.register(device_id, peer.clone());
    peer.json(ServerMessage::Registered {
        device_id,
        server_version: env!("CARGO_PKG_VERSION").to_owned(),
    });
    tracing::info!(%device_id, online = state.hub.online_devices(), "device online");

    let (mut sink, mut stream) = socket.split();

    // Writer: drains the outbound queue and keeps the connection alive. It
    // stops once every sender is gone (the hub dropped us and the reader ended).
    let writer_shutdown = shutdown.clone();
    let mut writer = tokio::spawn(async move {
        let mut ping = interval(PING_INTERVAL);
        ping.set_missed_tick_behavior(MissedTickBehavior::Delay);
        ping.tick().await;
        loop {
            let frame = tokio::select! {
                next = rx.recv() => match next {
                    Some(Outbound::Json(message)) => to_frame(&message),
                    Some(Outbound::Binary(data)) => Message::Binary(data),
                    None => break,
                },
                _ = ping.tick() => Message::Ping(Bytes::new()),
            };
            if sink.send(frame).await.is_err() {
                break;
            }
        }
        let _ = sink.send(Message::Close(None)).await;
        writer_shutdown.notify_one();
    });

    let mut limiter = RateLimiter::default();
    loop {
        let next = tokio::select! {
            _ = shutdown.notified() => break,
            next = timeout(IDLE_TIMEOUT, stream.next()) => next,
        };
        let Ok(Some(Ok(message))) = next else {
            break;
        };
        match message {
            Message::Text(text) => {
                handle_text(&state, device_id, &peer, &mut limiter, text.as_str());
            }
            Message::Binary(data) => handle_binary(&state, device_id, &peer, data),
            Message::Close(_) => break,
            Message::Ping(_) | Message::Pong(_) => {}
        }
    }

    state.hub.unregister(device_id, conn_id);
    drop(peer);
    if timeout(WRITER_DRAIN_TIMEOUT, &mut writer).await.is_err() {
        writer.abort();
    }
    tracing::info!(%device_id, online = state.hub.online_devices(), "device offline");
}

fn handle_text(
    state: &AppState,
    me: Uuid,
    peer: &Peer,
    limiter: &mut RateLimiter,
    text: &str,
) {
    let message = match serde_json::from_str::<ClientMessage>(text) {
        Ok(message) => message,
        Err(_) => {
            peer.json(ServerMessage::error(ErrorCode::MalformedMessage, None));
            return;
        }
    };

    let (result, session_id) = match message {
        ClientMessage::Register { .. } => (Err(ErrorCode::AlreadyRegistered), None),
        ClientMessage::ConnectRequest { target_id } => {
            let result = if limiter.allow() {
                state.hub.request(me, target_id).map(|session_id| {
                    tracing::info!(viewer = %me, host = %target_id, %session_id, "connection requested");
                    let hub = state.hub.clone();
                    let wait = state.config.request_timeout;
                    tokio::spawn(async move {
                        sleep(wait).await;
                        hub.expire(session_id);
                    });
                })
            } else {
                Err(ErrorCode::RateLimited)
            };
            (result, None)
        }
        ClientMessage::ConnectResponse { session_id, accept } => {
            let result = state.hub.respond(me, session_id, accept);
            if result.is_ok() {
                tracing::info!(host = %me, %session_id, accept, "connection answered");
            }
            (result, Some(session_id))
        }
        ClientMessage::EndSession { session_id } => {
            (state.hub.end(me, session_id), Some(session_id))
        }
        ClientMessage::Relay { session_id, payload } => {
            let result = match state.hub.relay_peer(me, session_id) {
                Some(other) => {
                    other.json(ServerMessage::Relay { session_id, payload });
                    Ok(())
                }
                None => Err(ErrorCode::UnknownSession),
            };
            (result, Some(session_id))
        }
        ClientMessage::Ping => {
            peer.json(ServerMessage::Pong);
            (Ok(()), None)
        }
    };

    if let Err(code) = result {
        peer.json(ServerMessage::error(code, session_id));
    }
}

/// Binary frames: 16-byte session ID, then opaque payload, forwarded unchanged.
fn handle_binary(state: &AppState, me: Uuid, peer: &Peer, data: Bytes) {
    let Some(session_id) = data
        .get(..BINARY_HEADER_LEN)
        .and_then(|header| Uuid::from_slice(header).ok())
    else {
        peer.json(ServerMessage::error(ErrorCode::MalformedMessage, None));
        return;
    };
    match state.hub.relay_peer(me, session_id) {
        Some(other) => other.send(Outbound::Binary(data)),
        None => peer.json(ServerMessage::error(ErrorCode::UnknownSession, Some(session_id))),
    }
}

/// Sliding-window limit on connection requests from one connection.
#[derive(Default)]
struct RateLimiter {
    recent: VecDeque<Instant>,
}

impl RateLimiter {
    fn allow(&mut self) -> bool {
        let now = Instant::now();
        while self
            .recent
            .front()
            .is_some_and(|at| now.duration_since(*at) >= REQUEST_RATE_WINDOW)
        {
            self.recent.pop_front();
        }
        if self.recent.len() >= REQUEST_RATE_MAX {
            return false;
        }
        self.recent.push_back(now);
        true
    }
}
