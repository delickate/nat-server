//! In-memory registry of online devices and the sessions between them.
//!
//! All state lives behind one mutex. Nothing awaits while holding it: messages
//! are queued with `try_send`, so the lock is only ever held briefly.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, MutexGuard},
};

use axum::body::Bytes;
use tokio::sync::{Notify, mpsc};
use uuid::Uuid;

use crate::protocol::{EndReason, ErrorCode, Role, ServerMessage};

/// Unanswered requests a device may have sent at once.
pub const MAX_PENDING_OUTGOING: usize = 3;
/// Unanswered requests a device may be asked at once.
pub const MAX_PENDING_INCOMING: usize = 5;
/// Active sessions (as viewer or host) per device.
pub const MAX_ACTIVE_SESSIONS: usize = 8;

pub enum Outbound {
    Json(ServerMessage),
    Binary(Bytes),
}

/// Handle used to push messages to one connected device.
#[derive(Clone)]
pub struct Peer {
    pub tx: mpsc::Sender<Outbound>,
    pub shutdown: Arc<Notify>,
}

impl Peer {
    /// Queues a message without waiting. A client whose queue is full cannot
    /// keep up; it is disconnected rather than silently losing messages.
    pub fn send(&self, message: Outbound) {
        if let Err(mpsc::error::TrySendError::Full(_)) = self.tx.try_send(message) {
            tracing::warn!("outbound queue full; disconnecting slow client");
            self.shutdown.notify_one();
        }
    }

    pub fn json(&self, message: ServerMessage) {
        self.send(Outbound::Json(message));
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionState {
    Pending,
    Active,
}

struct Session {
    viewer: Uuid,
    host: Uuid,
    state: SessionState,
}

impl Session {
    fn involves(&self, device: Uuid) -> bool {
        self.viewer == device || self.host == device
    }

    fn other(&self, device: Uuid) -> Option<Uuid> {
        if self.viewer == device {
            Some(self.host)
        } else if self.host == device {
            Some(self.viewer)
        } else {
            None
        }
    }
}

struct Device {
    conn_id: u64,
    peer: Peer,
}

#[derive(Default)]
struct Inner {
    devices: HashMap<Uuid, Device>,
    sessions: HashMap<Uuid, Session>,
    next_conn_id: u64,
}

impl Inner {
    fn send_to(&self, device: Uuid, message: ServerMessage) {
        if let Some(entry) = self.devices.get(&device) {
            entry.peer.json(message);
        }
    }

    /// Ends every session `device` takes part in, telling the other side.
    fn end_sessions_of(&mut self, device: Uuid, reason: EndReason) {
        let ids: Vec<Uuid> = self
            .sessions
            .iter()
            .filter(|(_, session)| session.involves(device))
            .map(|(id, _)| *id)
            .collect();
        for session_id in ids {
            if let Some(session) = self.sessions.remove(&session_id)
                && let Some(other) = session.other(device)
            {
                self.send_to(other, ServerMessage::SessionEnded { session_id, reason });
            }
        }
    }

    fn count(&self, filter: impl Fn(&Session) -> bool) -> usize {
        self.sessions.values().filter(|session| filter(session)).count()
    }
}

#[derive(Default)]
pub struct Hub {
    inner: Mutex<Inner>,
}

impl Hub {
    pub fn new() -> Self {
        Self::default()
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        // A panic while holding the lock leaves plain data behind; keep serving.
        self.inner.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn online_devices(&self) -> usize {
        self.lock().devices.len()
    }

    /// Marks `device_id` online. If it was already online from another
    /// connection, that connection is told why and closed, and its sessions end.
    pub fn register(&self, device_id: Uuid, peer: Peer) -> u64 {
        let mut inner = self.lock();
        inner.next_conn_id += 1;
        let conn_id = inner.next_conn_id;
        if let Some(old) = inner.devices.insert(device_id, Device { conn_id, peer }) {
            old.peer.json(ServerMessage::error(ErrorCode::Replaced, None));
            old.peer.shutdown.notify_one();
            inner.end_sessions_of(device_id, EndReason::PeerDisconnected);
        }
        conn_id
    }

    /// Marks the device offline, unless a newer connection has replaced it.
    pub fn unregister(&self, device_id: Uuid, conn_id: u64) {
        let mut inner = self.lock();
        if inner
            .devices
            .get(&device_id)
            .is_some_and(|device| device.conn_id == conn_id)
        {
            inner.devices.remove(&device_id);
            inner.end_sessions_of(device_id, EndReason::PeerDisconnected);
        }
    }

    /// Creates a pending session and asks the target to approve it.
    pub fn request(&self, viewer: Uuid, target: Uuid) -> Result<Uuid, ErrorCode> {
        if viewer == target {
            return Err(ErrorCode::SelfConnect);
        }
        let mut inner = self.lock();
        if !inner.devices.contains_key(&target) {
            return Err(ErrorCode::TargetOffline);
        }
        let pending = |s: &Session| s.state == SessionState::Pending;
        if inner.count(|s| pending(s) && s.viewer == viewer) >= MAX_PENDING_OUTGOING
            || inner.count(|s| pending(s) && s.host == target) >= MAX_PENDING_INCOMING
        {
            return Err(ErrorCode::TooManyRequests);
        }
        let active = |s: &Session| s.state == SessionState::Active;
        if inner.count(|s| active(s) && s.involves(viewer)) >= MAX_ACTIVE_SESSIONS
            || inner.count(|s| active(s) && s.involves(target)) >= MAX_ACTIVE_SESSIONS
        {
            return Err(ErrorCode::TooManySessions);
        }

        let session_id = Uuid::new_v4();
        inner.sessions.insert(
            session_id,
            Session {
                viewer,
                host: target,
                state: SessionState::Pending,
            },
        );
        inner.send_to(viewer, ServerMessage::RequestSent { session_id, target_id: target });
        inner.send_to(target, ServerMessage::IncomingRequest { session_id, from_id: viewer });
        Ok(session_id)
    }

    /// The host approves or denies a pending request.
    pub fn respond(&self, host: Uuid, session_id: Uuid, accept: bool) -> Result<(), ErrorCode> {
        let mut inner = self.lock();
        let session = inner
            .sessions
            .get_mut(&session_id)
            .filter(|s| s.host == host && s.state == SessionState::Pending)
            .ok_or(ErrorCode::UnknownSession)?;
        let viewer = session.viewer;

        if accept {
            session.state = SessionState::Active;
            inner.send_to(
                viewer,
                ServerMessage::SessionStarted { session_id, peer_id: host, role: Role::Viewer },
            );
            inner.send_to(
                host,
                ServerMessage::SessionStarted { session_id, peer_id: viewer, role: Role::Host },
            );
        } else {
            inner.sessions.remove(&session_id);
            inner.send_to(viewer, ServerMessage::SessionRejected { session_id });
        }
        Ok(())
    }

    /// Either participant cancels a pending request or ends an active session.
    pub fn end(&self, device: Uuid, session_id: Uuid) -> Result<(), ErrorCode> {
        let mut inner = self.lock();
        let other = inner
            .sessions
            .get(&session_id)
            .and_then(|s| s.other(device))
            .ok_or(ErrorCode::UnknownSession)?;
        inner.sessions.remove(&session_id);
        inner.send_to(
            other,
            ServerMessage::SessionEnded { session_id, reason: EndReason::PeerEnded },
        );
        Ok(())
    }

    /// Drops a request nobody answered in time.
    pub fn expire(&self, session_id: Uuid) {
        let mut inner = self.lock();
        if inner
            .sessions
            .get(&session_id)
            .is_some_and(|s| s.state == SessionState::Pending)
        {
            let session = inner.sessions.remove(&session_id).expect("checked above");
            for device in [session.viewer, session.host] {
                inner.send_to(
                    device,
                    ServerMessage::SessionEnded { session_id, reason: EndReason::Timeout },
                );
            }
        }
    }

    /// The other participant of an active session `sender` belongs to.
    pub fn relay_peer(&self, sender: Uuid, session_id: Uuid) -> Option<Peer> {
        let inner = self.lock();
        let other = inner
            .sessions
            .get(&session_id)
            .filter(|s| s.state == SessionState::Active)?
            .other(sender)?;
        inner.devices.get(&other).map(|device| device.peer.clone())
    }
}
