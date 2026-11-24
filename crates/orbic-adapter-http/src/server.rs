//! The socket: HTTP/1.1 connections over `hyper`, graceful shutdown.

use std::convert::Infallible;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, LengthLimitError, Limited};
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use hyve::HyveError;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinSet;

use crate::{HttpApp, too_large};

/// After the grace period, cancelled requests get this long to unwind
/// before their connections are dropped.
const CANCEL_GRACE: Duration = Duration::from_secs(1);

impl HttpApp {
    /// Serves HTTP/1.1 on `listener` until `shutdown` resolves, then drains:
    /// new requests get a retryable `503` (and `/health` turns `503`),
    /// running ones get the configured grace period, then they are
    /// cancelled. Plain HTTP is allowed when the listener is on loopback
    /// (development and tests); otherwise TLS ends at a trusted proxy.
    ///
    /// Each request runs inside its connection's task, so a client that
    /// disconnects cancels its request.
    pub async fn serve(
        self,
        listener: TcpListener,
        shutdown: impl Future<Output = ()>,
    ) -> io::Result<()> {
        let loopback = listener.local_addr()?.ip().is_loopback();
        let app = Arc::new(if loopback {
            self.allow_plaintext(true)
        } else {
            self
        });
        let (stop, stopped) = watch::channel(false);
        let mut connections = JoinSet::new();
        tokio::pin!(shutdown);
        loop {
            tokio::select! {
                () = &mut shutdown => break,
                Some(_) = connections.join_next(), if !connections.is_empty() => {}
                accepted = listener.accept() => {
                    let Ok((stream, peer)) = accepted else {
                        // Out of descriptors and the like: back off, keep serving.
                        tokio::time::sleep(Duration::from_millis(50)).await;
                        continue;
                    };
                    let app = app.clone();
                    let mut stopped = stopped.clone();
                    connections.spawn(async move {
                        let service = service_fn(move |req| {
                            let app = app.clone();
                            async move { Ok::<_, Infallible>(app.serve_one(req, peer).await) }
                        });
                        let conn = http1::Builder::new()
                            .serve_connection(TokioIo::new(stream), service);
                        tokio::pin!(conn);
                        let stop_requested = async move {
                            // The guard `wait_for` returns is not `Send`; drop it here.
                            let _ = stopped.wait_for(|s| *s).await.is_ok();
                        };
                        tokio::select! {
                            _ = conn.as_mut() => {}
                            () = stop_requested => {
                                conn.as_mut().graceful_shutdown();
                                let _ = conn.await;
                            }
                        }
                    });
                }
            }
        }
        drop(listener);
        app.pipeline.begin_shutdown();
        let _ = stop.send(true);
        let drain = async { while connections.join_next().await.is_some() {} };
        if tokio::time::timeout(app.settings.shutdown_grace, drain)
            .await
            .is_err()
        {
            app.pipeline.cancel_in_flight();
            let drain = async { while connections.join_next().await.is_some() {} };
            if tokio::time::timeout(CANCEL_GRACE, drain).await.is_err() {
                connections.shutdown().await;
            }
        }
        Ok(())
    }

    async fn serve_one(
        &self,
        req: hyper::Request<Incoming>,
        peer: SocketAddr,
    ) -> hyper::Response<Full<Bytes>> {
        let (parts, body) = req.into_parts();
        let max = self.settings.max_body;
        let declared_too_large = parts
            .headers
            .get(http::header::CONTENT_LENGTH)
            .and_then(|v| v.to_str().ok()?.parse::<u64>().ok())
            .is_some_and(|n| n > max as u64);
        let body = if declared_too_large {
            Err(too_large(max))
        } else {
            match Limited::new(body, max).collect().await {
                Ok(collected) => Ok(collected.to_bytes()),
                Err(e) if e.downcast_ref::<LengthLimitError>().is_some() => Err(too_large(max)),
                Err(_) => Err(HyveError::invalid_request(
                    "the request body could not be read",
                )),
            }
        };
        self.answer(&parts, body, peer).await.map(Full::new)
    }
}
