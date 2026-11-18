//! End-to-end tests: a real server on localhost and real WebSocket clients.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use nat_server::{
    Config,
    identity::device_id_from_secret,
    protocol::{EndReason, ErrorCode, PROTOCOL_VERSION, Role},
};
use serde_json::{Value, json};
use tokio::{net::TcpListener, time::timeout};
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async, tungstenite::Message};
use uuid::Uuid;

type Socket = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

async fn start_server(config: Config) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, nat_server::router(config)).await.unwrap();
    });
    format!("ws://{addr}/ws")
}

fn new_secret() -> String {
    format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple())
}

struct Client {
    socket: Socket,
    id: Uuid,
}

impl Client {
    async fn connect(url: &str) -> Self {
        Self::connect_with_secret(url, &new_secret()).await
    }

    async fn connect_with_secret(url: &str, secret: &str) -> Self {
        let (socket, _) = connect_async(url).await.unwrap();
        let mut client = Self { socket, id: device_id_from_secret(secret).unwrap() };
        client
            .send(json!({
                "type": "register",
                "protocol_version": PROTOCOL_VERSION,
                "client_version": "test",
                "device_secret": secret,
            }))
            .await;
        let registered = client.recv().await;
        assert_eq!(registered["type"], "registered");
        assert_eq!(registered["device_id"], client.id.to_string());
        client
    }

    async fn send(&mut self, message: Value) {
        self.socket.send(Message::Text(message.to_string().into())).await.unwrap();
    }

    async fn recv_frame(&mut self) -> Message {
        loop {
            let frame = timeout(Duration::from_secs(5), self.socket.next())
                .await
                .expect("timed out waiting for a message")
                .expect("socket closed")
                .unwrap();
            if !matches!(frame, Message::Ping(_) | Message::Pong(_)) {
                return frame;
            }
        }
    }

    async fn recv(&mut self) -> Value {
        match self.recv_frame().await {
            Message::Text(text) => serde_json::from_str(text.as_str()).unwrap(),
            other => panic!("expected text, got {other:?}"),
        }
    }

    async fn expect_error(&mut self, code: ErrorCode) {
        let message = self.recv().await;
        assert_eq!(message["type"], "error", "{message}");
        assert_eq!(message["code"], serde_json::to_value(code).unwrap(), "{message}");
    }

    /// Viewer asks host; returns the session ID after both sides were told.
    async fn request(&mut self, host: &mut Client) -> String {
        self.send(json!({"type": "connect_request", "target_id": host.id})).await;
        let sent = self.recv().await;
        assert_eq!(sent["type"], "request_sent");
        let incoming = host.recv().await;
        assert_eq!(incoming["type"], "incoming_request");
        assert_eq!(incoming["from_id"], self.id.to_string());
        assert_eq!(incoming["session_id"], sent["session_id"]);
        sent["session_id"].as_str().unwrap().to_owned()
    }
}

#[tokio::test]
async fn full_session_lifecycle() {
    let url = start_server(Config::default()).await;
    let mut viewer = Client::connect(&url).await;
    let mut host = Client::connect(&url).await;

    let session_id = viewer.request(&mut host).await;

    // Relay is refused until the host approves.
    viewer
        .send(json!({"type": "relay", "session_id": session_id, "payload": {"x": 1}}))
        .await;
    viewer.expect_error(ErrorCode::UnknownSession).await;

    host.send(json!({"type": "connect_response", "session_id": session_id, "accept": true}))
        .await;
    let started = viewer.recv().await;
    assert_eq!(started["type"], "session_started");
    assert_eq!(started["role"], serde_json::to_value(Role::Viewer).unwrap());
    assert_eq!(started["peer_id"], host.id.to_string());
    let started = host.recv().await;
    assert_eq!(started["role"], serde_json::to_value(Role::Host).unwrap());

    // JSON relay both ways.
    viewer
        .send(json!({"type": "relay", "session_id": session_id, "payload": {"mouse": [10, 20]}}))
        .await;
    let relayed = host.recv().await;
    assert_eq!(relayed["type"], "relay");
    assert_eq!(relayed["payload"]["mouse"], json!([10, 20]));

    // Binary relay keeps the bytes unchanged.
    let mut frame = Uuid::parse_str(&session_id).unwrap().as_bytes().to_vec();
    frame.extend_from_slice(b"screen-bytes");
    host.socket.send(Message::Binary(frame.clone().into())).await.unwrap();
    match viewer.recv_frame().await {
        Message::Binary(data) => assert_eq!(data.as_ref(), frame.as_slice()),
        other => panic!("expected binary, got {other:?}"),
    }

    viewer.send(json!({"type": "end_session", "session_id": session_id})).await;
    let ended = host.recv().await;
    assert_eq!(ended["type"], "session_ended");
    assert_eq!(ended["reason"], serde_json::to_value(EndReason::PeerEnded).unwrap());
}

#[tokio::test]
async fn host_can_reject() {
    let url = start_server(Config::default()).await;
    let mut viewer = Client::connect(&url).await;
    let mut host = Client::connect(&url).await;

    let session_id = viewer.request(&mut host).await;
    host.send(json!({"type": "connect_response", "session_id": session_id, "accept": false}))
        .await;
    let rejected = viewer.recv().await;
    assert_eq!(rejected["type"], "session_rejected");
}

#[tokio::test]
async fn only_the_host_can_answer() {
    let url = start_server(Config::default()).await;
    let mut viewer = Client::connect(&url).await;
    let mut host = Client::connect(&url).await;

    let session_id = viewer.request(&mut host).await;
    viewer
        .send(json!({"type": "connect_response", "session_id": session_id, "accept": true}))
        .await;
    viewer.expect_error(ErrorCode::UnknownSession).await;
}

#[tokio::test]
async fn request_errors() {
    let url = start_server(Config::default()).await;
    let mut viewer = Client::connect(&url).await;

    viewer.send(json!({"type": "connect_request", "target_id": Uuid::new_v4()})).await;
    viewer.expect_error(ErrorCode::TargetOffline).await;

    viewer.send(json!({"type": "connect_request", "target_id": viewer.id})).await;
    viewer.expect_error(ErrorCode::SelfConnect).await;

    viewer.send(json!({"type": "nonsense"})).await;
    viewer.expect_error(ErrorCode::MalformedMessage).await;

    viewer.send(json!({"type": "ping"})).await;
    assert_eq!(viewer.recv().await["type"], "pong");
}

#[tokio::test]
async fn invalid_secret_is_refused() {
    let url = start_server(Config::default()).await;
    let (socket, _) = connect_async(&url).await.unwrap();
    let mut client = Client { socket, id: Uuid::nil() };
    client
        .send(json!({
            "type": "register",
            "protocol_version": PROTOCOL_VERSION,
            "client_version": "test",
            "device_secret": "not-hex",
        }))
        .await;
    client.expect_error(ErrorCode::InvalidSecret).await;
}

#[tokio::test]
async fn unanswered_request_times_out() {
    let url = start_server(Config {
        request_timeout: Duration::from_millis(200),
        ..Config::default()
    })
    .await;
    let mut viewer = Client::connect(&url).await;
    let mut host = Client::connect(&url).await;

    viewer.request(&mut host).await;
    for client in [&mut viewer, &mut host] {
        let ended = client.recv().await;
        assert_eq!(ended["type"], "session_ended");
        assert_eq!(ended["reason"], serde_json::to_value(EndReason::Timeout).unwrap());
    }
}

#[tokio::test]
async fn disconnect_ends_sessions() {
    let url = start_server(Config::default()).await;
    let mut viewer = Client::connect(&url).await;
    let mut host = Client::connect(&url).await;

    let session_id = viewer.request(&mut host).await;
    host.send(json!({"type": "connect_response", "session_id": session_id, "accept": true}))
        .await;
    viewer.recv().await;
    host.recv().await;

    host.socket.close(None).await.unwrap();
    let ended = viewer.recv().await;
    assert_eq!(ended["type"], "session_ended");
    assert_eq!(ended["reason"], serde_json::to_value(EndReason::PeerDisconnected).unwrap());
}

#[tokio::test]
async fn second_login_replaces_first() {
    let url = start_server(Config::default()).await;
    let secret = new_secret();
    let mut first = Client::connect_with_secret(&url, &secret).await;
    let _second = Client::connect_with_secret(&url, &secret).await;
    first.expect_error(ErrorCode::Replaced).await;
}

#[tokio::test]
async fn too_many_pending_requests() {
    let url = start_server(Config::default()).await;
    let mut viewer = Client::connect(&url).await;
    let mut host = Client::connect(&url).await;

    for _ in 0..nat_server::hub::MAX_PENDING_OUTGOING {
        viewer.request(&mut host).await;
    }
    viewer.send(json!({"type": "connect_request", "target_id": host.id})).await;
    viewer.expect_error(ErrorCode::TooManyRequests).await;
}
