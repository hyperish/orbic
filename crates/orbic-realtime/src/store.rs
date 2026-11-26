use super::*;
use hyve::Timestamp;
use hyve::serde::{from_json, to_json};
use orbic_auth::{credential_hash, random_credential};
use orbic_db::{Bind, Sql, bind, projection};

#[cfg(test)]
mod tests;

pub(super) const DDL: &str = "
CREATE TABLE IF NOT EXISTS orbic_room_tickets (
 token_hash TEXT PRIMARY KEY, topic TEXT NOT NULL, room TEXT NOT NULL, principal TEXT NOT NULL,
 session_key TEXT NOT NULL, family TEXT NOT NULL, expires BIGINT NOT NULL);
CREATE INDEX IF NOT EXISTS orbic_room_tickets_expiry ON orbic_room_tickets(expires);
CREATE TABLE IF NOT EXISTS orbic_rooms (room TEXT PRIMARY KEY, seq BIGINT NOT NULL, state TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS orbic_room_events (room TEXT NOT NULL, seq BIGINT NOT NULL, event TEXT NOT NULL, PRIMARY KEY(room,seq));
CREATE TABLE IF NOT EXISTS orbic_room_requests (room TEXT NOT NULL, principal TEXT NOT NULL, request_id TEXT NOT NULL,
 input TEXT NOT NULL, ack TEXT NOT NULL, PRIMARY KEY(room,principal,request_id));";

projection!(Grant, "orbic_room_tickets", topic: String, room: String, principal: String, session_key: String, family: String);
projection!(State, "orbic_rooms", seq: i64, state: String);
projection!(Stored, "orbic_room_events", event: String);
projection!(Prior, "orbic_room_requests", input: String, ack: String);

fn query<T>(sql: &'static str, args: Vec<Bind>) -> Sql<T> {
    Sql::new(sql, &["sqlite", "postgres"], &[], args)
}
fn decode(text: &str) -> HyveResult<Value> {
    from_json(text).map_err(|_| HyveError::internal("invalid persisted room state"))
}
fn event(text: &str) -> HyveResult<Event> {
    Event::parse(&decode(text)?)
}
pub(super) fn denied() -> HyveError {
    HyveError::domain("unauthenticated", "a current room session is required")
}

impl Rooms {
    pub(super) async fn ticket(
        &self,
        topic: &str,
        path: &str,
        principal: &str,
        key: &str,
        family: &str,
    ) -> HyveResult<String> {
        let token = random_credential()?;
        let now = Timestamp::now().as_unix_micros();
        let room = format!("{topic}|{path}");
        query::<()>(
            "DELETE FROM orbic_room_tickets WHERE expires <= $1",
            vec![bind(&now)],
        )
        .execute(&self.db)
        .await?;
        query::<()>("INSERT INTO orbic_room_tickets(token_hash,topic,room,principal,session_key,family,expires) VALUES ($1,$2,$3,$4,$5,$6,$7)",
            vec![bind(&credential_hash("room",&token)), bind(topic),bind(&room),bind(principal),bind(key),bind(family),bind(&(now+30_000_000))]).execute(&self.db).await?;
        Ok(token)
    }
    pub(super) async fn consume(&self, token: &str) -> HyveResult<Grant> {
        if token.len() != 43
            || !token
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
        {
            return Err(denied());
        }
        let grant = query::<Grant>("DELETE FROM orbic_room_tickets WHERE token_hash=$1 AND expires>$2 RETURNING topic,room,principal,session_key,family",
            vec![bind(&credential_hash("room",token)), bind(&Timestamp::now().as_unix_micros())]).fetch_optional(&self.db).await?.ok_or_else(denied)?;
        self.authorize(&grant).await?;
        Ok(grant)
    }
    pub(super) async fn authorize(&self, grant: &Grant) -> HyveResult<()> {
        if !self
            .cache
            .session_record(&grant.session_key)
            .await?
            .is_some_and(|(_, family)| family == grant.family)
        {
            return Err(denied());
        }
        Ok(())
    }
    pub(super) fn hint(&self, grant: &Grant, payload: &Value) -> HyveResult<Value> {
        let handler = self
            .definitions
            .read()
            .expect("room definitions")
            .get(&grant.topic)
            .and_then(|definition| definition.hint.clone())
            .ok_or_else(|| HyveError::invalid_request("this room does not accept hints"))?;
        let (_, path) = grant
            .room
            .split_once('|')
            .ok_or_else(|| HyveError::internal("invalid room key"))?;
        handler(&grant.principal, path, payload)
    }
    pub(super) async fn apply(&self, grant: &Grant, cmd: Command) -> HyveResult<Event> {
        let reducer = self
            .definitions
            .read()
            .expect("room definitions")
            .get(&grant.topic)
            .map(|definition| definition.reduce.clone())
            .ok_or_else(|| HyveError::internal("unregistered room definition"))?;
        let room = grant.room.clone();
        let principal = grant.principal.clone();
        let retention = i64::from(self.retention);
        self.db.tx(move |tx| async move {
            // Acquire the writer lock before reading a snapshot or request ID.
            let old = query::<State>("INSERT INTO orbic_rooms(room,seq,state) VALUES($1,0,'null')
                ON CONFLICT(room) DO UPDATE SET room=excluded.room RETURNING seq,state", vec![bind(&room)]).fetch_one(&tx).await?;
            let input = to_json(&cmd.envelope());
            if let Some(prior) = query::<Prior>("SELECT input,ack FROM orbic_room_requests WHERE room=$1 AND principal=$2 AND request_id=$3",
                vec![bind(&room),bind(&principal),bind(&cmd.id)]).fetch_optional(&tx).await? {
                if prior.input != input { return Err(HyveError::domain("idempotency_mismatch","request ID has different room input")); }
                return event(&prior.ack);
            }
            let change = reducer(&decode(&old.state)?, &principal, &cmd)?;
            if change.kind.starts_with("room.") || change.kind.split_once('.').is_none() {
                return Err(HyveError::internal("invalid application room event type"));
            }
            let seq = old.seq.checked_add(1).ok_or_else(|| HyveError::internal("room sequence exhausted"))?;
            let notification = Event::new(&change.kind,seq,change.payload,None);
            let snapshot = Event::new("room.snapshot", seq, change.state.clone(), Some(cmd.id.clone()));
            for version in [Codec::Json, Codec::Binary] {
                version.encode(&snapshot.envelope())?;
                version.encode(&notification.envelope())?;
            }
            let ack = Event::new("room.ack", seq, Value::String(notification.id.clone()), Some(cmd.id.clone()));
            query::<()>("UPDATE orbic_rooms SET seq=$1,state=$2 WHERE room=$3", vec![bind(&seq),bind(&to_json(&change.state)),bind(&room)]).execute(&tx).await?;
            query::<()>("INSERT INTO orbic_room_events(room,seq,event) VALUES($1,$2,$3)",
                vec![bind(&room),bind(&seq),bind(&to_json(&notification.envelope()))]).execute(&tx).await?;
            query::<()>("INSERT INTO orbic_room_requests(room,principal,request_id,input,ack) VALUES($1,$2,$3,$4,$5)",
                vec![bind(&room),bind(&principal),bind(&cmd.id),bind(&input),bind(&to_json(&ack.envelope()))]).execute(&tx).await?;
            query::<()>("DELETE FROM orbic_room_events WHERE room=$1 AND seq<=$2",vec![bind(&room),bind(&(seq-retention))]).execute(&tx).await?;
            Ok(ack)
        }).await
    }
    pub(super) async fn replay(&self, grant: &Grant, from: i64) -> HyveResult<(Vec<Event>, i64)> {
        let Some(state) = query::<State>(
            "SELECT seq,state FROM orbic_rooms WHERE room=$1",
            vec![bind(&grant.room)],
        )
        .fetch_optional(&self.db)
        .await?
        else {
            let events = if from == 0 {
                vec![]
            } else {
                vec![Event::new("room.snapshot", 0, Value::Null, None)]
            };
            return Ok((events, 0));
        };
        if from == state.seq {
            return Ok((Vec::new(), state.seq));
        }
        if (state.seq - i64::from(self.retention)..state.seq).contains(&from) {
            let rows = query::<Stored>("SELECT event FROM orbic_room_events WHERE room=$1 AND seq>$2 AND seq<=$3 ORDER BY seq LIMIT 128",
                vec![bind(&grant.room),bind(&from),bind(&state.seq)]).fetch_all(&self.db).await?;
            let events = rows
                .into_iter()
                .map(|row| event(&row.event))
                .collect::<HyveResult<Vec<_>>>()?;
            if events.first().is_some_and(|e| e.seq == from + 1) {
                return Ok((events, state.seq));
            }
        }
        // Pruning can race the SELECT above; this snapshot and its sequence
        // came from one row, so the next poll resumes consistently.
        Ok((
            vec![Event::new(
                "room.snapshot",
                state.seq,
                decode(&state.state)?,
                None,
            )],
            state.seq,
        ))
    }
}
