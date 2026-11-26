use futures_util::{StreamExt, stream};
use hyve::{
    CancellationToken, HyveError, HyveResult,
    api::{
        BoxFuture, Call, Rail, Reply, Transport, errors,
        http::{HttpAnswer, HttpRequest, decode, encode, wants_standard_rail},
    },
    trace::TraceContext,
};
use reqwest::{
    Client, Method, Url,
    header::{AUTHORIZATION, CONTENT_TYPE, HeaderValue, RETRY_AFTER},
};
use std::{
    collections::HashSet,
    net::IpAddr,
    sync::{
        Arc, RwLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};
use tokio::sync::Mutex;

const MAX_RESPONSE: usize = 8 * 1024 * 1024;
struct Replica {
    url: Url,
    healthy: AtomicBool,
}
struct Directory {
    replicas: RwLock<Vec<Arc<Replica>>>,
    next: AtomicUsize,
    probe: Mutex<()>,
}

/// A transport for one named service. Replica URLs include the component
/// mount prefix. Plain HTTP is restricted to loopback.
#[derive(Clone)]
pub struct Service {
    client: Client,
    directory: Arc<Directory>,
    bearer: Option<HeaderValue>,
}
impl Service {
    /// Configures replicas and checks their standard health routes.
    pub async fn discover(endpoints: &[&str]) -> HyveResult<Self> {
        let client = Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_secs(3))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| HyveError::internal(format!("initializing service client: {e}")))?;
        let service = Self {
            client,
            directory: Arc::new(Directory {
                replicas: RwLock::new(Vec::new()),
                next: AtomicUsize::new(0),
                probe: Mutex::new(()),
            }),
            bearer: None,
        };
        service.replace(endpoints)?;
        service.probe().await;
        Ok(service)
    }
    /// Configures the service credential on this transport. Health probes
    /// carry no credential.
    pub fn bearer(mut self, token: &str) -> HyveResult<Self> {
        if token.is_empty() || !token.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(HyveError::invalid_request("invalid service bearer"));
        }
        let mut value = HeaderValue::from_str(&format!("Bearer {token}"))
            .map_err(|_| HyveError::invalid_request("invalid service bearer"))?;
        value.set_sensitive(true);
        self.bearer = Some(value);
        Ok(self)
    }
    /// Atomically changes membership. Existing replicas keep their health;
    /// new replicas receive calls after a successful probe.
    pub fn replace(&self, endpoints: &[&str]) -> HyveResult<()> {
        let mut urls = Vec::new();
        let mut seen = HashSet::new();
        for endpoint in endpoints {
            let url = Url::parse(endpoint)
                .map_err(|_| HyveError::invalid_request("invalid service URL"))?;
            let loopback = url.host_str().is_some_and(|h| {
                h == "localhost"
                    || h.trim_matches(['[', ']'])
                        .parse::<IpAddr>()
                        .is_ok_and(|ip| ip.is_loopback())
            });
            if url.host().is_none()
                || !(url.scheme() == "https" || url.scheme() == "http" && loopback)
                || !url.username().is_empty()
                || url.password().is_some()
                || url.query().is_some()
                || url.fragment().is_some()
            {
                return Err(HyveError::invalid_request(
                    "service endpoints require HTTPS or loopback HTTP, with no embedded credentials, query or fragment",
                ));
            }
            if seen.insert(url.clone()) {
                urls.push(url);
            }
        }
        let mut replicas = self.directory.replicas.write().expect("service directory");
        *replicas = urls
            .into_iter()
            .map(|url| {
                replicas
                    .iter()
                    .find(|r| r.url == url)
                    .cloned()
                    .unwrap_or_else(|| {
                        Arc::new(Replica {
                            url,
                            healthy: AtomicBool::new(false),
                        })
                    })
            })
            .collect();
        Ok(())
    }
    /// Checks configured endpoints concurrently and returns the healthy count.
    pub async fn probe(&self) -> usize {
        let _probe = self.directory.probe.lock().await;
        let replicas = self
            .directory
            .replicas
            .read()
            .expect("service directory")
            .clone();
        stream::iter(replicas)
            .for_each_concurrent(8, |r| async move {
                let mut url = r.url.clone();
                url.set_path("/health");
                let healthy = self
                    .client
                    .get(url)
                    .timeout(Duration::from_secs(2))
                    .send()
                    .await
                    .is_ok_and(|response| response.status() == reqwest::StatusCode::OK);
                r.healthy.store(healthy, Ordering::Release);
            })
            .await;
        self.directory
            .replicas
            .read()
            .expect("service directory")
            .iter()
            .filter(|r| r.healthy.load(Ordering::Acquire))
            .count()
    }
    /// Runs periodic health probes until application shutdown.
    pub async fn monitor(&self, period: Duration, shutdown: CancellationToken) -> HyveResult<()> {
        if period.is_zero() {
            return Err(HyveError::invalid_request("health period must be positive"));
        }
        let mut timer = tokio::time::interval(period);
        timer.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                () = shutdown.cancelled() => return Ok(()),
                _ = async { timer.tick().await; self.probe().await; } => {}
            }
        }
    }
    fn select(&self) -> HyveResult<Arc<Replica>> {
        let replicas = self.directory.replicas.read().expect("service directory");
        if !replicas.is_empty() {
            let start = self.directory.next.fetch_add(1, Ordering::Relaxed) % replicas.len();
            if let Some(r) = replicas[start..]
                .iter()
                .chain(&replicas[..start])
                .find(|r| r.healthy.load(Ordering::Acquire))
            {
                return Ok(r.clone());
            }
        }
        Err(
            HyveError::infra("service_unavailable", "no healthy service replica")
                .retryable(true)
                .with_public_code(),
        )
    }
    async fn send(
        &self,
        replica: &Replica,
        req: HttpRequest,
        trace: Option<&TraceContext>,
    ) -> HyveResult<reqwest::Response> {
        let url = format!("{}{}", replica.url.as_str().trim_end_matches('/'), req.path);
        let method = Method::from_bytes(req.method.as_str().as_bytes()).expect("known method");
        let mut builder = self.client.request(method, url);
        for (key, value) in req.headers {
            builder = builder.header(key, value);
        }
        if let Some(auth) = &self.bearer {
            builder = builder.header(AUTHORIZATION, auth.clone());
        }
        if let Some(t) = trace {
            builder = builder
                .header("x-request-id", t.request_id.as_str())
                .header("traceparent", t.trace_parent().to_string());
        }
        builder.body(req.body).send().await.map_err(|e| {
            if e.is_connect() {
                replica.healthy.store(false, Ordering::Release);
            }
            HyveError::infra(
                "service_transport",
                "service request failed before the response was received",
            )
        })
    }
    async fn run(&self, call: Call) -> HyveResult<Reply> {
        let replica = self.select()?;
        let path = call.descriptor().path;
        let trace = TraceContext::current();
        let mut response = self
            .send(
                &replica,
                encode(&call, Rail::Fast, path, None)?,
                trace.as_ref(),
            )
            .await?;
        if wants_standard_rail(response.status().as_u16()) {
            response = self
                .send(
                    &replica,
                    encode(&call, Rail::Standard, path, None)?,
                    trace.as_ref(),
                )
                .await?;
        }
        if response
            .content_length()
            .is_some_and(|n| n > MAX_RESPONSE as u64)
        {
            return Err(errors::decode_failed("service response is too large"));
        }
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .map(str::to_owned);
        let retry_after = response.headers().contains_key(RETRY_AFTER);
        let mut body = Vec::new();
        while let Some(chunk) = response
            .chunk()
            .await
            .map_err(|_| HyveError::infra("service_transport", "service response interrupted"))?
        {
            if chunk.len() > MAX_RESPONSE - body.len() {
                return Err(errors::decode_failed("service response is too large"));
            }
            body.extend_from_slice(&chunk);
        }
        decode(HttpAnswer {
            status,
            content_type: content_type.as_deref(),
            retry_after,
            body: &body,
        })
    }
}
impl Transport for Service {
    fn call(&self, call: Call) -> BoxFuture<'_, HyveResult<Reply>> {
        let cancel = call.cancellation().clone();
        Box::pin(async move {
            tokio::select! {
                biased;
                () = cancel.cancelled() => Err(errors::cancelled("service call cancelled")),
                result = self.run(call) => result,
            }
        })
    }
}
