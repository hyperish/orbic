//! `orbic::realtime` — Dual-Rail (WebTransport priority + WebSocket fallback) realtime rooms,
//! short-lived single-use `?ticket=` authentication, monotonic `seq` ordering, idempotent `id` dedup,
//! and reconnection log replay (`docs/04-realtime-communication.md`).

use crate::auth::User;
use hyve::prelude::*;
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};

/// Physical transport underlying a realtime connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RealtimeTransport {
    /// Primary HTTP/3 QUIC WebTransport rail.
    WebTransport,
    /// Automatic fallback RFC 6455 WebSocket rail when UDP/QUIC is blocked.
    WebSocketFallback,
}

/// Client-to-server realtime message envelope (`docs/04-realtime-communication.md` §2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientFrame {
    /// Event type formatted `<target>.<action>` (e.g. `"project.rename"`).
    pub msg_type: String,
    /// Client-generated idempotency ID preserved across reconnects / fallback switches.
    pub id: String,
    /// Serialized message payload.
    pub payload: String,
}

/// Server-to-client realtime message envelope (`docs/04-realtime-communication.md` §2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerFrame {
    /// Past/state event type (e.g. `"project.renamed"`).
    pub msg_type: String,
    /// Server-generated event ID.
    pub id: String,
    /// Monotonically increasing synchronization sequence number.
    pub seq: u64,
    /// Server UTC timestamp.
    pub ts: Timestamp,
    /// Payload content.
    pub payload: String,
    /// Echoed client request `id`, when responding to a client command.
    pub request_id: Option<String>,
}

#[derive(Default)]
struct RoomState {
    next_seq: u64,
    tickets: HashMap<String, Id<User>>,
    seen_client_ids: HashSet<String>,
    history: Vec<ServerFrame>,
    connections: HashMap<String, (Id<User>, RealtimeTransport)>,
}

/// Stateful room coordinator supporting ticket handshake, dedup, fallback continuity, and `seq` replay.
#[derive(Clone, Default)]
pub struct RealtimeHub {
    state: Arc<Mutex<RoomState>>,
}

impl RealtimeHub {
    /// Creates an empty [`RealtimeHub`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Mints a short-lived, single-use `?ticket=` token bound to `user_id`.
    #[must_use]
    pub fn issue_ticket(&self, user_id: Id<User>) -> String {
        let ticket = format!("tkt_{}", Ulid::generate());
        if let Ok(mut guard) = self.state.lock() {
            guard.tickets.insert(ticket.clone(), user_id);
        }
        ticket
    }

    /// Consumes `ticket` atomically during handshake; reusing or spoofing fails with `401`.
    pub fn connect_with_ticket(
        &self,
        connection_id: &str,
        ticket: &str,
        transport: RealtimeTransport,
    ) -> HyveResult<Id<User>> {
        let mut guard = self
            .state
            .lock()
            .map_err(|_| HyveError::internal("realtime hub lock poisoned"))?;
        let Some(user_id) = guard.tickets.remove(ticket) else {
            return Err(HyveError::unauthenticated(
                "realtime ticket missing, expired, or already consumed",
            ));
        };
        guard
            .connections
            .insert(connection_id.to_owned(), (user_id, transport));
        Ok(user_id)
    }

    /// Switches an existing connection (or reconnects) between WebTransport and WebSocket fallback
    /// and returns all missed [`ServerFrame`]s strictly after `last_seq`.
    pub fn reconnect_from_seq(
        &self,
        connection_id: &str,
        ticket: &str,
        transport: RealtimeTransport,
        last_seq: u64,
    ) -> HyveResult<Vec<ServerFrame>> {
        let _user_id = self.connect_with_ticket(connection_id, ticket, transport)?;
        let guard = self
            .state
            .lock()
            .map_err(|_| HyveError::internal("realtime hub lock poisoned"))?;
        Ok(guard
            .history
            .iter()
            .filter(|f| f.seq > last_seq)
            .cloned()
            .collect())
    }

    /// Processes a client message envelope with idempotent client-`id` deduplication.
    ///
    /// Returns `Some(ServerFrame)` when newly applied, or `None` if a duplicate `id`
    /// was already processed (including across a WebTransport -> WebSocket fallback retry).
    pub fn publish_client_frame(
        &self,
        frame: ClientFrame,
        broadcast_type: &str,
    ) -> HyveResult<Option<ServerFrame>> {
        let mut guard = self
            .state
            .lock()
            .map_err(|_| HyveError::internal("realtime hub lock poisoned"))?;
        if !guard.seen_client_ids.insert(frame.id.clone()) {
            return Ok(None);
        }
        guard.next_seq += 1;
        let event = ServerFrame {
            msg_type: broadcast_type.to_owned(),
            id: format!("evt_{}", Ulid::generate()),
            seq: guard.next_seq,
            ts: Timestamp::now(),
            payload: frame.payload,
            request_id: Some(frame.id),
        };
        guard.history.push(event.clone());
        Ok(Some(event))
    }

    /// Returns the total count of deduplicated events broadcast in this room.
    #[must_use]
    pub fn total_events(&self) -> usize {
        self.state.lock().map(|g| g.history.len()).unwrap_or(0)
    }
}
