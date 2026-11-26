use super::*;
use crate::{
    CacheKey, IDEMPOTENCY_RETENTION, IdempotencyKey, RateDecision, RequestBegin,
    RequestReservation, SessionWrite, authority::Claim,
};
use bytes::Bytes;
use hyve_core::Ulid;

const WRITE_SESSIONS: &str = r#"
local family = KEYS[1]
if ARGV[1] == 'rotate' then
    local old_family = redis.call('HGET',KEYS[2],'family')
    if not old_family or old_family ~= ARGV[2] then return 0 end
    if redis.call('HGET',KEYS[2],'data') ~= ARGV[3] then
        for _,k in ipairs(redis.call('ZRANGE',family,0,-1)) do redis.call('DEL',k) end
        redis.call('DEL',family)
        return 0
    end
end
local time = redis.call('TIME')
local now = time[1] * 1000 + math.floor(time[2] / 1000)
redis.call('ZREMRANGEBYSCORE',family,'-inf',now)
for i=3,#KEYS do
    local ttl = tonumber(ARGV[(i-3)*2+5])
    redis.call('HSET',KEYS[i],'family',ARGV[2],'data',ARGV[(i-3)*2+4])
    redis.call('PEXPIRE',KEYS[i],ttl)
    redis.call('ZADD',family,now+ttl,KEYS[i])
    if redis.call('PTTL',family) < ttl then redis.call('PEXPIRE',family,ttl) end
end
return 1
"#;

impl Redis {
    pub(crate) async fn write_sessions(
        &self,
        family: &str,
        records: &[SessionWrite],
        expected: Option<(&str, &[u8])>,
    ) -> HyveResult<bool> {
        let mut keys = vec![
            self.key("family", family),
            self.key("session", expected.map_or("", |r| r.0)),
        ];
        let mut args = vec![
            Value::from_static_str(if expected.is_some() {
                "rotate"
            } else {
                "issue"
            }),
            family.to_owned().into(),
            Bytes::copy_from_slice(expected.map_or(&[], |r| r.1)).into(),
        ];
        for r in records {
            keys.push(self.key("session", &r.key));
            args.extend([
                Bytes::copy_from_slice(&r.value).into(),
                millis(r.ttl).into(),
            ]);
        }
        self.eval(WRITE_SESSIONS, keys, args).await
    }

    pub(crate) async fn session(&self, key: &str) -> HyveResult<Option<(Vec<u8>, String)>> {
        let record: Option<(Bytes,String)> = self.eval(
            "if redis.call('EXISTS',KEYS[1]) == 0 then return false end return redis.call('HMGET',KEYS[1],'data','family')",
            vec![self.key("session",key)],vec![]).await?;
        Ok(record.map(|(v, f)| (v.to_vec(), f)))
    }

    pub(crate) async fn renew(&self, key: &str, family: &str, ttl: Duration) -> HyveResult<bool> {
        self.eval(
            r#"
if redis.call('HGET',KEYS[1],'family') ~= ARGV[1] then return 0 end
local time = redis.call('TIME')
redis.call('PEXPIRE', KEYS[1], ARGV[2])
redis.call('ZADD', KEYS[2], time[1]*1000+math.floor(time[2]/1000)+tonumber(ARGV[2]), KEYS[1])
if redis.call('PTTL',KEYS[2]) < tonumber(ARGV[2]) then redis.call('PEXPIRE',KEYS[2],ARGV[2]) end
return 1"#,
            vec![self.key("session", key), self.key("family", family)],
            vec![family.to_owned().into(), millis(ttl).into()],
        )
        .await
    }

    pub(crate) async fn revoke(&self, family: &str) -> HyveResult<()> {
        self.eval("for _,k in ipairs(redis.call('ZRANGE',KEYS[1],0,-1)) do redis.call('DEL',k) end return redis.call('DEL',KEYS[1])",
            vec![self.key("family",family)],vec![]).await
    }

    pub(crate) async fn count(
        &self,
        guard: &str,
        subject: &str,
        limit: u32,
        window: Duration,
    ) -> HyveResult<RateDecision> {
        let key = CacheKey::new(guard).push(subject);
        let (allowed, remaining, reset): (bool, u32, u64) = self
            .eval(
                r#"
local n = tonumber(redis.call('GET',KEYS[1]) or '0')
local limit = tonumber(ARGV[1])
if n >= limit then return {0,0,redis.call('PTTL',KEYS[1])} end
n = redis.call('INCR',KEYS[1])
if n == 1 then redis.call('PEXPIRE',KEYS[1],ARGV[2]) end
return {1,limit-n,redis.call('PTTL',KEYS[1])}"#,
                vec![self.key("rate", key.as_str())],
                vec![i64::from(limit).into(), millis(window).into()],
            )
            .await?;
        Ok(RateDecision {
            allowed,
            limit,
            remaining,
            reset: Duration::from_millis(reset),
        })
    }

    pub(crate) async fn begin(
        self: &Arc<Self>,
        key: IdempotencyKey,
        digest: u128,
    ) -> HyveResult<RequestBegin> {
        let id = CacheKey::new(&key.principal)
            .push(key.method)
            .push(&key.path)
            .push(&key.key);
        let key = self.key("request", id.as_str());
        let owner = Ulid::new().to_string();
        let (state, data): (i64, Bytes) = self
            .eval(
                r#"
local old = redis.call('HGET',KEYS[1],'digest')
if not old then
    redis.call('HSET',KEYS[1],'digest',ARGV[1],'owner',ARGV[2])
    redis.call('PEXPIRE',KEYS[1],ARGV[3])
    return {0,''}
end
if old ~= ARGV[1] then return {3,''} end
local data = redis.call('HGET',KEYS[1],'reply')
if data then return {1,data} end
return {2,''}"#,
                vec![key.clone()],
                vec![
                    format!("{digest:032x}").into(),
                    owner.clone().into(),
                    millis(IDEMPOTENCY_RETENTION).into(),
                ],
            )
            .await?;
        Ok(match state {
            0 => RequestBegin::Started(RequestReservation(Claim::Remote(RemoteReservation {
                redis: self.clone(),
                key,
                owner: Some(owner),
            }))),
            1 => RequestBegin::Replay(data.to_vec()),
            2 => RequestBegin::InProgress,
            3 => RequestBegin::Mismatch,
            _ => return Err(HyveError::internal("invalid idempotency record")),
        })
    }
}

pub(crate) struct RemoteReservation {
    redis: Arc<Redis>,
    key: String,
    owner: Option<String>,
}

impl Drop for RemoteReservation {
    fn drop(&mut self) {
        if let (Some(owner), Ok(rt)) = (self.owner.take(), tokio::runtime::Handle::try_current()) {
            let redis = self.redis.clone();
            let key = self.key.clone();
            rt.spawn(async move {
                let _=redis.eval::<i64>("if redis.call('HGET',KEYS[1],'owner') == ARGV[1] then return redis.call('DEL',KEYS[1]) end return 0",
                    vec![key],vec![owner.into()]).await;
            });
        }
    }
}

impl RemoteReservation {
    pub(crate) async fn finish(mut self, outcome: Option<Vec<u8>>) -> HyveResult<()> {
        // If a process or connection dies during a write, retain the in-flight
        // reservation. Re-running an unknown external effect could duplicate it.
        let (op, bytes) = match outcome {
            Some(b) => ("save", b),
            None => ("release", Vec::new()),
        };
        let saved: bool = self
            .redis
            .eval(
                r#"
if redis.call('HGET',KEYS[1],'owner') ~= ARGV[1] then return 0 end
if ARGV[2] == 'release' then redis.call('DEL',KEYS[1]) return 1 end
redis.call('HSET',KEYS[1],'reply',ARGV[3])
redis.call('HDEL',KEYS[1],'owner')
redis.call('PEXPIRE',KEYS[1],ARGV[4])
return 1"#,
                vec![self.key.clone()],
                vec![
                    self.owner.take().expect("unfinished reservation").into(),
                    op.into(),
                    Bytes::from(bytes).into(),
                    millis(IDEMPOTENCY_RETENTION).into(),
                ],
            )
            .await?;
        if !saved {
            return Err(HyveError::infra(
                "reservation_expired",
                "request reservation expired before completion",
            ));
        }
        Ok(())
    }
}
