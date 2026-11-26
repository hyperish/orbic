//! Durable room reducers, authenticated connection tickets and realtime carriers.
mod store;
mod transport;
pub use hyve::api::realtime::{Codec, Command, Event, RoomCursor, reconnect_delay};
use hyve::{Ctx, HyveError, HyveResult, Value};
use orbic_auth::CurrentSession;
use orbic_cache::Cache;
use orbic_core::{BackendComponent, Extract, Parts, Principal};
use orbic_db::{Db, Dialect};
use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};
pub use transport::{RoomServer, accept_websocket};

/// The next durable snapshot and event produced by a room command.
pub struct Change {
    /// Complete state to send a client when replay is unavailable.
    pub state: Value,
    /// Application event type (e.g. `note.created`).
    pub kind: String,
    /// Incremental data to deliver to current subscribers.
    pub payload: Value,
}
type Reducer = dyn Fn(&Value, &str, &Command) -> HyveResult<Change> + Send + Sync;

type HintHandler = dyn Fn(&str, &str, &Value) -> HyveResult<Value> + Send + Sync;
struct Definition {
    reduce: Arc<Reducer>,
    hint: Option<Arc<HintHandler>>,
}

/// Each replica registers the same room definitions at startup and shares
/// an application database and authoritative session cache.
#[derive(Clone)]
pub struct Rooms {
    db: Db,
    cache: Cache,
    retention: u32,
    definitions: Arc<RwLock<HashMap<String, Definition>>>,
}

/// Registered reducer and ticket issuer for a component route.
#[derive(Clone)]
pub struct Room {
    rooms: Rooms,
    name: String,
}

impl Rooms {
    /// Initialises durable room state. A positive retention count bounds the
    /// replay window per room. Deduplication survives pruning and restart.
    pub async fn new(db: Db, cache: Cache, retention: u32) -> HyveResult<Self> {
        if !(1..=4096).contains(&retention) {
            return Err(HyveError::invalid_request("room retention must be 1–4096"));
        }
        let pg = db.dialect() == Dialect::Postgres;
        db.tx(move |tx| async move {
            if pg {
                tx.execute_script("SELECT pg_advisory_xact_lock(1546712908441911)")
                    .await?;
            }
            tx.execute_script(store::DDL).await
        })
        .await?;
        Ok(Self {
            db,
            cache,
            retention,
            definitions: Default::default(),
        })
    }

    /// Registers a synchronous state transition. The framework commits its
    /// snapshot, event and request ID together under the room's database lock.
    pub fn define(
        &self,
        name: &str,
        reduce: impl Fn(&Value, &str, &Command) -> HyveResult<Change> + Send + Sync + 'static,
    ) -> HyveResult<Room> {
        if name.is_empty()
            || name.len() > 128
            || !name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        {
            return Err(HyveError::invalid_request("invalid room definition name"));
        }
        let mut definitions = self.definitions.write().expect("room definitions");
        if definitions.contains_key(name) {
            return Err(HyveError::invalid_request("duplicate room definition"));
        }
        definitions.insert(
            name.into(),
            Definition {
                reduce: Arc::new(reduce),
                hint: None,
            },
        );
        Ok(Room {
            rooms: self.clone(),
            name: name.into(),
        })
    }
}

impl Room {
    /// Receives a best-effort hint with the authenticated principal and concrete
    /// route path. Return the hint to deliver to the sender. Handlers must
    /// tolerate loss, duplication and reordering and must not commit durable mutations.
    pub fn on_hint(
        self,
        handler: impl Fn(&str, &str, &Value) -> HyveResult<Value> + Send + Sync + 'static,
    ) -> Self {
        self.rooms
            .definitions
            .write()
            .expect("room definitions")
            .get_mut(&self.name)
            .expect("registered room")
            .hint = Some(Arc::new(handler));
        self
    }
}
struct Instance(String);
impl Extract for Instance {
    fn extract(parts: &mut Parts) -> HyveResult<Self> {
        Ok(Self(parts.path.clone()))
    }
}

/// Adds a room ticket endpoint to the ordinary guarded HTTP pipeline.
pub trait RoomExt {
    /// Issues a 30-second, single-use ticket for this authenticated route
    /// instance. Mount under the normal session and room membership guards.
    fn room(self, path: &str, room: Room) -> Self;
}

impl RoomExt for BackendComponent {
    fn room(self, path: &str, room: Room) -> Self {
        self.post(
            path,
            move |Instance(path): Instance,
                  principal: Ctx<Principal>,
                  session: Ctx<CurrentSession>| {
                let room = room.clone();
                async move {
                    room.rooms
                        .ticket(
                            &room.name,
                            &path,
                            principal.as_str(),
                            session.key(),
                            session.family(),
                        )
                        .await
                }
            },
        )
    }
}
