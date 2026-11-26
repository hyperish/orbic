//! The socket: HTTP/1.1 connections over `hyper`, graceful shutdown.

use std::convert::Infallible;
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

#[cfg(not(feature = "serge"))]
use bytes::Bytes;
use hyper::body::Incoming;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use tokio::sync::watch;
use tokio::task::JoinSet;

use crate::HttpApp;

/// After the grace period, cancelled requests get this long to unwind
/// before their connections are dropped.
const CANCEL_GRACE: Duration = Duration::from_secs(1);

#[cfg(not(feature = "serge"))]
type SocketBody = http_body_util::Full<Bytes>;
#[cfg(feature = "serge")]
type SocketBody = orbic_adapter_serge::SiteBody;

mod sealed {
    pub trait Sealed {}
    impl Sealed for tokio::net::TcpListener {}
    #[cfg(all(feature = "ipc", any(unix, windows)))]
    impl Sealed for hyve::ipc::Listener {}
}

/// A listener supported by the HTTP adapter and its graceful shutdown loop.
pub trait ServerListener: sealed::Sealed + Send {
    /// An established byte stream.
    type Stream: tokio::io::AsyncRead + tokio::io::AsyncWrite + Send + Unpin + 'static;
    /// Accepts and identifies the next peer.
    fn accept_peer(
        &mut self,
    ) -> impl Future<Output = io::Result<(Self::Stream, SocketAddr)>> + Send;
    /// Whether this listener's transport permits plaintext.
    fn local_plaintext(&self) -> io::Result<bool>;
    /// Local RPC peers cannot delegate a forwarded proxy identity.
    fn is_ipc(&self) -> bool {
        false
    }
}
impl ServerListener for TcpListener {
    type Stream = tokio::net::TcpStream;
    async fn accept_peer(&mut self) -> io::Result<(Self::Stream, SocketAddr)> {
        self.accept().await
    }
    fn local_plaintext(&self) -> io::Result<bool> {
        Ok(self.local_addr()?.ip().is_loopback())
    }
}
#[cfg(all(feature = "ipc", any(unix, windows)))]
impl ServerListener for hyve::ipc::Listener {
    type Stream = hyve::ipc::ServerStream;
    async fn accept_peer(&mut self) -> io::Result<(Self::Stream, SocketAddr)> {
        Ok((self.accept().await?, SocketAddr::from(([127, 0, 0, 1], 0))))
    }
    fn local_plaintext(&self) -> io::Result<bool> {
        Ok(true)
    }
    fn is_ipc(&self) -> bool {
        true
    }
}

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
        mut self,
        mut listener: impl ServerListener,
        shutdown: impl Future<Output = ()>,
    ) -> io::Result<()> {
        let loopback = listener.local_plaintext()?;
        if listener.is_ipc() {
            self.settings.trusted.clear();
        }
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
                accepted = listener.accept_peer() => {
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
    ) -> hyper::Response<SocketBody> {
        let (parts, body) = req.into_parts();
        let body = crate::body::collect(body, self.settings.max_body).await;
        #[cfg(feature = "serge")]
        {
            self.socket_answer(&parts, body, peer).await
        }
        #[cfg(not(feature = "serge"))]
        {
            self.answer(&parts, body, peer)
                .await
                .map(http_body_util::Full::new)
        }
    }
}
