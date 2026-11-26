//! The shared tier uses one hash slot per application namespace. Lua scripts
//! compare ownership and update related records in a single server operation.

use fred::prelude::*;
use hyve_core::{HyveError, HyveResult};
use std::sync::Arc;
use std::time::Duration;

mod data;
mod reply;
mod stores;

pub(crate) use data::Loaded;
pub(crate) use stores::RemoteReservation;

pub(crate) struct Redis {
    client: Client,
    prefix: String,
    driver: tokio::task::AbortHandle,
}

impl Drop for Redis {
    fn drop(&mut self) {
        self.driver.abort();
    }
}

fn unavailable(_: fred::error::Error) -> HyveError {
    // Client errors can contain the endpoint or arguments. Keep credentials
    // and private cache data out of the public request error.
    HyveError::infra("cache_unavailable", "the shared cache is unavailable").retryable(true)
}

fn millis(ttl: Duration) -> i64 {
    ttl.as_millis().clamp(1, i64::MAX as u128 / 2) as i64
}

impl Redis {
    pub(crate) async fn connect(url: &str, namespace: &str) -> HyveResult<Arc<Self>> {
        if namespace.is_empty()
            || namespace.len() > 128
            || !namespace
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        {
            return Err(HyveError::internal(
                "cache namespace must be 1–128 ASCII letters, digits, dots, underscores or hyphens",
            ));
        }
        let mut config =
            Config::from_url(url).map_err(|_| HyveError::internal("invalid shared cache URL"))?;
        config.version = fred::types::RespVersion::RESP3;
        let client = Builder::from_config(config)
            .set_policy(ReconnectPolicy::new_exponential(0, 100, 5_000, 2))
            .with_connection_config(|c| {
                c.connection_timeout = Duration::from_secs(3);
                // A lost reply can follow a committed session rotation or rate increment.
                c.max_command_attempts = 1;
            })
            .with_performance_config(|c| {
                c.default_command_timeout = Duration::from_secs(3);
            })
            .build()
            .map_err(unavailable)?;
        let driver = client.connect();
        // Aborting construction also releases a driver that never connected.
        let redis = Arc::new(Self {
            client,
            prefix: format!("orbic:{{{namespace}}}:"),
            driver: driver.abort_handle(),
        });
        tokio::time::timeout(Duration::from_secs(5), redis.client.wait_for_connect())
            .await
            .map_err(|_| {
                HyveError::infra("cache_unavailable", "shared cache connection timed out")
            })?
            .map_err(unavailable)?;
        Ok(redis)
    }

    fn key(&self, kind: &str, id: &str) -> String {
        format!("{}{kind}:{id}", self.prefix)
    }

    pub(crate) async fn subscribe(
        &self,
        invalidate: impl Fn(Option<&str>) + Send + 'static,
    ) -> HyveResult<tokio::task::AbortHandle> {
        use tokio::sync::broadcast::error::RecvError;
        let client = self.client.clone();
        let channel = self.key("invalidate", "v1");
        let mut messages = client.message_rx();
        let mut errors = client.error_rx();
        let mut reconnect = client.reconnect_rx();
        client
            .subscribe(channel.clone())
            .await
            .map_err(unavailable)?;
        let task = tokio::spawn(async move {
            loop {
                tokio::select! {
                    message = messages.recv() => match message {
                        Ok(m) if &*m.channel == channel.as_str() => invalidate(m.value.as_str().as_deref()),
                        Ok(_) => (),
                        Err(RecvError::Lagged(_)) => invalidate(None),
                        Err(RecvError::Closed) => break,
                    },
                    event = errors.recv() => {
                        if matches!(event,Err(RecvError::Closed)){break}
                        invalidate(None);
                    }
                    event = reconnect.recv() => {
                        if matches!(event,Err(RecvError::Closed)){break}
                        invalidate(None);
                        // A per-entry revision check also covers missed invalidations.
                        let _ = client.subscribe(channel.clone()).await;
                    }
                }
            }
        });
        Ok(task.abort_handle())
    }

    async fn eval<T: FromValue>(
        &self,
        script: &'static str,
        keys: Vec<String>,
        args: Vec<Value>,
    ) -> HyveResult<T> {
        self.client
            .eval(script, keys, args)
            .await
            .map_err(unavailable)
    }
}
