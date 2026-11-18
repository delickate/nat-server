//! Rendezvous and relay server for Delickate Remote Access.
//!
//! Devices keep a WebSocket open to `/ws`. A viewer asks for a session by
//! device ID, the host approves or denies it, and while the session is active
//! the server relays traffic between the two.

pub mod hub;
pub mod identity;
pub mod protocol;
mod ws;

use std::{
    env,
    sync::{Arc, atomic::AtomicUsize},
    time::Duration,
};

use axum::{Json, Router, extract::State, routing::get};

pub use ws::MAX_MESSAGE_BYTES;

#[derive(Debug, Clone)]
pub struct Config {
    /// Open WebSocket connections allowed at once.
    pub max_connections: usize,
    /// How long a host has to answer a connection request.
    pub request_timeout: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_connections: 5_000,
            request_timeout: Duration::from_secs(60),
        }
    }
}

impl Config {
    /// Reads `MAX_CONNECTIONS` and `REQUEST_TIMEOUT_SECS`, falling back to defaults.
    pub fn from_env() -> Result<Self, String> {
        let mut config = Self::default();
        if let Ok(value) = env::var("MAX_CONNECTIONS") {
            config.max_connections = value
                .parse()
                .map_err(|_| format!("MAX_CONNECTIONS must be a number, got {value:?}"))?;
        }
        if let Ok(value) = env::var("REQUEST_TIMEOUT_SECS") {
            let secs: u64 = value
                .parse()
                .map_err(|_| format!("REQUEST_TIMEOUT_SECS must be a number, got {value:?}"))?;
            config.request_timeout = Duration::from_secs(secs);
        }
        Ok(config)
    }
}

#[derive(Clone)]
pub struct AppState {
    hub: Arc<hub::Hub>,
    config: Arc<Config>,
    connections: Arc<AtomicUsize>,
}

pub fn router(config: Config) -> Router {
    let state = AppState {
        hub: Arc::new(hub::Hub::new()),
        config: Arc::new(config),
        connections: Arc::new(AtomicUsize::new(0)),
    };
    Router::new()
        .route("/", get(|| async { "Delickate Remote Access server. Clients connect to /ws.\n" }))
        .route("/health", get(health))
        .route("/ws", get(ws::ws_handler))
        .with_state(state)
}

async fn health(State(state): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "status": "ok",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol_version": protocol::PROTOCOL_VERSION,
        "online_devices": state.hub.online_devices(),
    }))
}
