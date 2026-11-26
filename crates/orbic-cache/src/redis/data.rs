use super::reply::Claim;
use super::*;
use crate::{CacheKey, Tag};
use bytes::Bytes;
use hyve_api::Wire;
use hyve_core::Ulid;
use std::future::Future;
use std::time::Instant;

const LEASE_MS: i64 = 30_000;

// Tag membership is fenced by revision. An expired entry can be replaced with
// different tags without an old tag later invalidating the new entry.
const CLAIM: &str = r#"
local handoff = KEYS[#KEYS]
if ARGV[5] ~= '' and redis.call('EXISTS',handoff) == 1 then
    local data = redis.call('HMGET',handoff,'type','data')
    if redis.call('HINCRBY',handoff,'readers',-1) == 0 then redis.call('DEL',handoff) end
    return {3,ARGV[5],data[1],data[2],0}
end
local rev = redis.call('HGET', KEYS[1], 'rev')
if rev then
    if redis.call('HEXISTS', KEYS[1], 'data') == 0 then
        if redis.call('HGET',KEYS[1],'instant') == '1' then
            if ARGV[5] ~= rev then redis.call('HINCRBY',KEYS[1],'readers',1) end
            return {0,rev,'','',0}
        end
        return {0,'','','',0}
    end
    return {1, rev, redis.call('HGET', KEYS[1], 'type'), redis.call('HGET', KEYS[1], 'data'), redis.call('PTTL', KEYS[1])}
end
redis.call('HSET', KEYS[1], 'rev', ARGV[1])
if ARGV[4] == '1' then redis.call('HSET',KEYS[1],'instant','1') end
redis.call('PEXPIRE', KEYS[1], ARGV[2])
for i=2,#KEYS-1 do
    redis.call('HSET', KEYS[i], KEYS[1], ARGV[1])
    if redis.call('PTTL', KEYS[i]) < tonumber(ARGV[3]) then redis.call('PEXPIRE', KEYS[i], ARGV[3]) end
end
return {2, '', '', '', 0}
"#;
const RELEASE: &str = "if redis.call('HGET',KEYS[1],'rev') == ARGV[1] and redis.call('HEXISTS',KEYS[1],'data') == 0 then return redis.call('DEL',KEYS[1]) end return 0";
const RENEW: &str = r#"
if redis.call('HGET',KEYS[1],'rev') ~= ARGV[1] then return 0 end
redis.call('PEXPIRE',KEYS[1],ARGV[2])
for i=2,#KEYS do
    if redis.call('PTTL',KEYS[i]) < tonumber(ARGV[2]) then redis.call('PEXPIRE',KEYS[i],ARGV[2]) end
end
return 1
"#;
const PUBLISH: &str = r#"
if redis.call('HGET',KEYS[1],'rev') ~= ARGV[1] then return 0 end
if tonumber(ARGV[4]) == 0 then
    local instant = redis.call('HGET',KEYS[1],'instant') == '1'
    local readers = tonumber(redis.call('HGET',KEYS[1],'readers') or 0)
    redis.call('DEL',KEYS[1])
    if instant and readers > 0 then
        redis.call('HSET',KEYS[2],'type',ARGV[2],'data',ARGV[3],'readers',readers)
        redis.call('PEXPIRE',KEYS[2],ARGV[5])
    end
    if instant then return 2 else return 0 end
end
redis.call('HSET',KEYS[1],'type',ARGV[2],'data',ARGV[3])
redis.call('PEXPIRE',KEYS[1],ARGV[4])
return 1
"#;

struct Lease {
    redis: Arc<Redis>,
    key: String,
    rev: Option<String>,
}

impl Drop for Lease {
    fn drop(&mut self) {
        if let Some(rev) = self.rev.take() {
            let redis = self.redis.clone();
            let key = self.key.clone();
            if let Ok(rt) = tokio::runtime::Handle::try_current() {
                rt.spawn(async move {
                    let _ = redis
                        .eval::<i64>(RELEASE, vec![key], vec![rev.into()])
                        .await;
                });
            }
        }
    }
}

pub(crate) struct Loaded<T> {
    pub(crate) value: T,
    pub(crate) expires: Option<Instant>,
    pub(crate) revision: String,
}

impl Redis {
    pub(crate) async fn current(&self, key: &CacheKey, rev: &str) -> HyveResult<bool> {
        let actual: Option<String> = self
            .client
            .hget(self.key("data", key.as_str()), "rev")
            .await
            .map_err(unavailable)?;
        Ok(actual.as_deref() == Some(rev))
    }

    pub(crate) async fn load<T: Wire, F: Future<Output = HyveResult<T>>>(
        self: &Arc<Self>,
        key: &CacheKey,
        ttl: Duration,
        tags: &[Tag],
        init: impl FnOnce() -> F,
    ) -> HyveResult<Loaded<T>> {
        let key = self.key("data", key.as_str());
        let mut keys = vec![key.clone()];
        keys.extend(tags.iter().map(|t| self.key("tag", t.as_str())));
        let rev = Ulid::new().to_string();
        let ty = format!("{}:{:016x}", std::any::type_name::<T>(), T::FINGERPRINT);
        let args = vec![
            rev.clone().into(),
            LEASE_MS.into(),
            millis(ttl).max(LEASE_MS).into(),
            i64::from(ttl.is_zero()).into(),
        ];
        let mut waiting = String::new();
        loop {
            let started = Instant::now();
            let mut claim_keys = keys.clone();
            claim_keys.push(self.key("handoff", &waiting));
            let mut claim_args = args.clone();
            claim_args.push(waiting.clone().into());
            match self.eval(CLAIM, claim_keys, claim_args).await? {
                Claim::Wait(owner) => {
                    waiting = owner;
                    tokio::time::sleep(Duration::from_millis(10)).await;
                }
                Claim::Read {
                    revision,
                    schema,
                    data,
                    ttl,
                } => {
                    if ty.as_bytes() != schema.as_ref() {
                        return Err(HyveError::internal(
                            "cache key was read with a different type or schema",
                        ));
                    }
                    let value = T::from_binary(&data)
                        .map_err(|_| HyveError::internal("invalid shared cache record"))?;
                    return Ok(Loaded {
                        value,
                        expires: ttl.map(|remaining| started + remaining),
                        revision,
                    });
                }
                Claim::Load => break,
            }
        }
        let started = Instant::now();
        let mut lease = Lease {
            redis: self.clone(),
            key: key.clone(),
            rev: Some(rev.clone()),
        };
        let mut future = std::pin::pin!(init());
        let value = loop {
            tokio::select! {
                outcome = &mut future => break outcome?,
                _ = tokio::time::sleep(Duration::from_millis(LEASE_MS as u64 / 3)) => {
                    // Invalidation can supersede this load while its caller still needs the value.
                    let _: i64 = self.eval(RENEW, keys.clone(), vec![rev.clone().into(), LEASE_MS.into()]).await?;
                }
            }
        };
        let remaining = ttl
            .saturating_sub(started.elapsed())
            .as_millis()
            .min(i64::MAX as u128 / 2) as i64;
        let bytes = value
            .to_binary()
            .map_err(|_| HyveError::internal("could not encode cache value"))?;
        let published: i64 = self
            .eval(
                PUBLISH,
                vec![key, self.key("handoff", &rev)],
                vec![
                    rev.clone().into(),
                    ty.into(),
                    Bytes::from_owner(bytes).into(),
                    remaining.into(),
                    LEASE_MS.into(),
                ],
            )
            .await?;
        lease.rev = None;
        Ok(Loaded {
            value,
            revision: rev,
            expires: match published {
                0 => Some(started),
                1 => Some(started + Duration::from_millis(millis(ttl) as u64)),
                2 => None,
                _ => return Err(HyveError::internal("invalid shared cache publish result")),
            },
        })
    }

    pub(crate) async fn invalidate(&self, key: &CacheKey) -> HyveResult<()> {
        self.eval(
            "redis.call('DEL',KEYS[1]) return redis.call('PUBLISH',KEYS[2],ARGV[1])",
            vec![self.key("data", key.as_str()), self.key("invalidate", "v1")],
            vec![format!("key:{}", key.as_str()).into()],
        )
        .await
    }
    pub(crate) async fn invalidate_tag(&self, tag: &Tag) -> HyveResult<()> {
        self.eval(
            r#"
local members = redis.call('HGETALL',KEYS[1])
for i=1,#members,2 do
    if redis.call('HGET',members[i],'rev') == members[i+1] then redis.call('DEL',members[i]) end
end
redis.call('DEL',KEYS[1])
return redis.call('PUBLISH',KEYS[2],ARGV[1])
"#,
            vec![self.key("tag", tag.as_str()), self.key("invalidate", "v1")],
            vec![format!("tag:{}", tag.as_str()).into()],
        )
        .await
    }
}
