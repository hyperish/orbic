use hyve::{
    HyveError, HyveResult, RequestId, Scope, Value,
    serde::{Binary, FORMAT_FINGERPRINT},
    trace::{Span, TraceContext, TraceParent},
};
use orbic_db::{Bind, Db, Sql, Tx, bind, projection};
use std::future::Future;

const DDL: &str = "
CREATE TABLE IF NOT EXISTS orbic_bus_topics (topic TEXT PRIMARY KEY, seq BIGINT NOT NULL);
CREATE TABLE IF NOT EXISTS orbic_bus_events (
 topic TEXT NOT NULL, seq BIGINT NOT NULL, producer TEXT NOT NULL, event_id TEXT NOT NULL,
 payload BYTEA NOT NULL, trace_parent TEXT, request_id TEXT,
 PRIMARY KEY(topic,seq), UNIQUE(topic,producer,event_id));
CREATE TABLE IF NOT EXISTS orbic_bus_offsets (
 topic TEXT NOT NULL, group_name TEXT NOT NULL, seq BIGINT NOT NULL, PRIMARY KEY(topic,group_name));";
projection!(Position, "orbic_bus_offsets", seq: i64);
projection!(Previous, "orbic_bus_events", seq: i64, payload: Vec<u8>);
projection!(Stored, "orbic_bus_events", seq: i64, producer: String, event_id: String, payload: Vec<u8>,
    trace_parent: Option<String>, request_id: Option<String>);

fn query<T>(sql: &'static str, args: Vec<Bind>) -> Sql<T> {
    Sql::new(sql, &["sqlite", "postgres"], &[], args)
}
fn name(s: &str) -> HyveResult<()> {
    if s.is_empty() || s.len() > 128 || s.chars().any(char::is_control) {
        return Err(HyveError::invalid_request(
            "event names and IDs must be 1–128 bytes without control characters",
        ));
    }
    Ok(())
}
/// One committed event in a named producer's topic.
pub struct ServiceEvent {
    /// Monotonic sequence within this topic.
    pub seq: i64,
    /// Producer-supplied stable ID.
    pub id: String,
    /// Namespace of that ID.
    pub producer: String,
    /// Archived application payload restored after schema validation.
    pub payload: Value,
}

/// SQL outbox and consumer groups sharing the application's database.
#[derive(Clone)]
pub struct EventBus {
    db: Db,
}

impl EventBus {
    /// Initializes the event tables in the app's SQLite or PostgreSQL database.
    pub async fn new(db: Db) -> HyveResult<Self> {
        db.execute_script(DDL).await?;
        Ok(Self { db })
    }
    /// Appends independently. Reusing an ID and payload returns the original sequence.
    pub async fn publish(
        &self,
        topic: &str,
        producer: &str,
        id: &str,
        payload: &Value,
    ) -> HyveResult<i64> {
        self.db
            .tx(|tx| async move { Self::publish_in(&tx, topic, producer, id, payload).await })
            .await
    }
    /// Writes an event alongside the producer's business change in the same transaction.
    pub async fn publish_in(
        tx: &Tx,
        topic: &str,
        producer: &str,
        id: &str,
        payload: &Value,
    ) -> HyveResult<i64> {
        for value in [topic, producer, id] {
            name(value)?;
        }
        let archive = payload.to_binary()?;
        if archive.len() > 1024 * 1024 {
            return Err(HyveError::invalid_request("service event exceeds 1 MiB"));
        }
        let mut bytes = FORMAT_FINGERPRINT.to_le_bytes().to_vec();
        bytes.extend_from_slice(&archive);
        let pos = query::<Position>(
            "INSERT INTO orbic_bus_topics(topic,seq) VALUES($1,0) ON CONFLICT(topic) DO UPDATE SET topic=excluded.topic RETURNING seq",
            vec![bind(topic)]).fetch_one(tx).await?;
        if let Some(prior) = query::<Previous>(
            "SELECT seq,payload FROM orbic_bus_events WHERE topic=$1 AND producer=$2 AND event_id=$3",
            vec![bind(topic),bind(producer),bind(id)]).fetch_optional(tx).await? {
            if prior.payload != bytes { return Err(HyveError::domain("idempotency_key_mismatch", "event ID already has a different payload")); }
            return Ok(prior.seq);
        }
        let seq = pos
            .seq
            .checked_add(1)
            .ok_or_else(|| HyveError::internal("event sequence exhausted"))?;
        let current = TraceContext::current();
        let parent = current.as_ref().map(|c| c.trace_parent().to_string());
        let request = current.map(|c| c.request_id.to_string());
        query::<()>("INSERT INTO orbic_bus_events(topic,seq,producer,event_id,payload,trace_parent,request_id) VALUES($1,$2,$3,$4,$5,$6,$7)",
            vec![bind(topic),bind(&seq),bind(producer),bind(id),bind(&bytes),bind(&parent),bind(&request)]).execute(tx).await?;
        query::<()>(
            "UPDATE orbic_bus_topics SET seq=$1 WHERE topic=$2",
            vec![bind(&seq), bind(topic)],
        )
        .execute(tx)
        .await?;
        Ok(seq)
    }
    /// Applies the next event and advances this group's offset in one
    /// transaction. Use the supplied transaction for durable handler effects.
    /// A handler error or cancellation rolls both writes back. Independent
    /// groups see every event. External effects require their own idempotency key.
    pub async fn consume<F, Fut>(
        &self,
        topic: &str,
        group: &str,
        apply: F,
    ) -> HyveResult<Option<i64>>
    where
        F: FnOnce(Tx, ServiceEvent) -> Fut,
        Fut: Future<Output = HyveResult<()>>,
    {
        name(topic)?;
        name(group)?;
        self.db.tx(|tx| async move {
            let pos = query::<Position>(
                "INSERT INTO orbic_bus_offsets(topic,group_name,seq) VALUES($1,$2,0) ON CONFLICT(topic,group_name) DO UPDATE SET group_name=excluded.group_name RETURNING seq",
                vec![bind(topic),bind(group)]).fetch_one(&tx).await?;
            let row = query::<Stored>(
                "SELECT seq,producer,event_id,payload,trace_parent,request_id FROM orbic_bus_events WHERE topic=$1 AND seq>$2 ORDER BY seq LIMIT 1",
                vec![bind(topic),bind(&pos.seq)]).fetch_optional(&tx).await?;
            let Some(row) = row else { return Ok(None); };
            let archive = row.payload.strip_prefix(&FORMAT_FINGERPRINT.to_le_bytes()[..])
                .ok_or_else(|| HyveError::internal("unsupported service event encoding"))?;
            let payload = Value::from_binary(archive).map_err(|_| HyveError::internal("invalid stored service event"))?;
            let parent = row.trace_parent.as_deref().and_then(TraceParent::parse);
            let request = row.request_id.as_deref().and_then(|v| RequestId::parse(v).ok()).unwrap_or_default();
            let root = Scope::current().unwrap_or_else(Scope::root);
            let span = Span::incoming(&root, request, parent, format!("consume {topic}"));
            let event = ServiceEvent { seq: row.seq, id: row.event_id, producer: row.producer, payload };
            span.scope().scoped(async { apply(tx.clone(), event).await }).await?;
            query::<()>("UPDATE orbic_bus_offsets SET seq=$1 WHERE topic=$2 AND group_name=$3",
                vec![bind(&row.seq),bind(topic),bind(group)]).execute(&tx).await?;
            Ok(Some(row.seq))
        }).await
    }
}
