//! The production host answers real HTTP requests against a versioned bundle.
#![cfg(feature = "serge")]

use bytes::Bytes;
use futures_util::{StreamExt, stream};
use hyve::{HyveError, HyveResult, Scope, Ulid, api::BoxFuture};
use orbic::{OrbicApp, site::*};
use orbic_core::{Guard, GuardCtx, Principal};
use serde_json::json;
use std::{
    fs,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};

const DIGEST: &str = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
fn bundle(root: &Path, build: &str, filename: &str) {
    fs::write(root.join(filename), "abc").unwrap();
    fs::write(root.join("page.html"), format!("page {build}")).unwrap();
    fs::write(root.join("serge-bundle.json"),serde_json::to_vec(&json!({
        "version":1,"buildId":build,
        "assets":[{"url":format!("/assets/{DIGEST}/{filename}"),"file":filename,"bytes":3,"mediaType":"text/javascript","sha256":DIGEST}],
        "routes":[{"mode":"ssg","pattern":"/","file":"page.html"},{"mode":"csr","pattern":"/private","file":"page.html"},
                  {"mode":"ssr","pattern":"/live/{id}"},{"mode":"ssr","pattern":"/redirect"}]
    })).unwrap()).unwrap();
}
struct Signed;
impl Guard for Signed {
    fn check<'a>(&'a self, cx: &'a mut GuardCtx<'_>) -> BoxFuture<'a, HyveResult<()>> {
        Box::pin(async move {
            cx.response.rate_limit = Some(orbic_core::RateLimitInfo {
                limit: 10,
                remaining: 8,
                reset_secs: 60,
            });
            if !matches!(cx.meta.credential,Some(orbic_core::Credential::Bearer(ref token)) if token=="local-session")
            {
                return Err(HyveError::domain("unauthenticated", "sign in"));
            }
            cx.scope.provide(Principal::new("reader"));
            Ok(())
        })
    }
}
struct Render {
    id: String,
    last: Arc<Mutex<Option<Scope>>>,
}
impl SergeSsrHandler for Render {
    fn build_id(&self) -> &str {
        &self.id
    }
    fn render(&self, request: SsrRequest) -> SsrFuture {
        *self.last.lock().unwrap() = Some(request.scope.clone());
        Box::pin(async move {
            if request.path == "/redirect" {
                return Ok(SsrResponse::Redirect {
                    location: if request.query == "bad" {
                        "//another.test"
                    } else {
                        "/private"
                    }
                    .into(),
                    permanent: false,
                });
            }
            let p = request.scope.require::<Principal>()?;
            let chunk = Bytes::from(format!(
                "{} {} {} {}",
                p.as_str(),
                request.path,
                request.query,
                request.request_id.as_str()
            ));
            Ok(SsrResponse::Html {
                status: 200,
                body: Box::pin(stream::iter([Ok(chunk)]).chain(stream::pending())),
            })
        })
    }
}

async fn open(addr: std::net::SocketAddr, method: &str, path: &str, headers: &str) -> TcpStream {
    let mut conn = TcpStream::connect(addr).await.unwrap();
    conn.write_all(
        format!(
            "{method} {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n{headers}\r\n"
        )
        .as_bytes(),
    )
    .await
    .unwrap();
    conn
}
async fn request(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    headers: &str,
) -> (u16, String, String) {
    let mut conn = open(addr, method, path, headers).await;
    let mut data = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), conn.read_to_end(&mut data))
        .await
        .unwrap()
        .unwrap();
    let text = String::from_utf8(data).unwrap();
    let (head, body) = text.split_once("\r\n\r\n").unwrap();
    (
        head.split_whitespace().nth(1).unwrap().parse().unwrap(),
        head.to_ascii_lowercase(),
        body.into(),
    )
}

#[tokio::test]
async fn guarded_files_atomic_reloads_and_disconnect_during_ssr() {
    let last = Arc::new(Mutex::new(None));
    let renderer = |id: &str| {
        Some(Arc::new(Render {
            id: id.into(),
            last: last.clone(),
        }) as Arc<dyn SergeSsrHandler>)
    };
    let one = tempfile::tempdir().unwrap();
    let first = Ulid::new().to_string();
    bundle(one.path(), &first, "first.js");
    let policy = Policies::new()
        .public("/")
        .guard("/private", Signed)
        .guard("/live", Signed);
    assert!(SergeHost::open(one.path(), Policies::new(), renderer(&first)).is_err());
    assert!(SergeHost::open(one.path(), policy.clone(), None).is_err());
    assert!(
        SergeHost::open(
            one.path(),
            policy.clone(),
            renderer(&Ulid::new().to_string())
        )
        .is_err()
    );
    let site = SergeHost::open(one.path(), policy, renderer(&first)).unwrap();
    let http = orbic::http_app(OrbicApp::new().provide(site)).unwrap();
    let host = http.pipeline().provided::<SergeHost>().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (stop, halt) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(http.serve(listener, async {
        let _ = halt.await;
    }));
    let (code, head, html) = request(addr, "GET", "/", "").await;
    assert_eq!(code, 200);
    assert_eq!(html, format!("page {first}"));
    assert!(head.contains("cache-control: no-cache"));
    let etag = head.lines().find_map(|l| l.strip_prefix("etag: ")).unwrap();
    let conditional = format!("If-None-Match: W/{etag}\r\n");
    assert_eq!(request(addr, "GET", "/", &conditional).await.0, 304);
    let auth = "Authorization: Bearer local-session\r\n";
    let (denied, headers, _) = request(addr, "GET", "/private", &conditional).await;
    assert_eq!(denied, 401);
    assert!(headers.contains("ratelimit-remaining: 8"));
    assert_eq!(request(addr, "HEAD", "/private", &conditional).await.0, 401);
    let (code, h, b) = request(addr, "HEAD", "/private", auth).await;
    assert_eq!(code, 200);
    assert!(b.is_empty() && h.contains("private, no-store"));
    assert!(h.contains("ratelimit-limit: 10"));
    assert_eq!(
        request(addr, "GET", "/private", &format!("{auth}{conditional}"))
            .await
            .0,
        304
    );
    assert_eq!(request(addr, "POST", "/", "").await.0, 405);
    for path in ["/missing", "/v1/missing", "/assets/missing", "/page.html"] {
        assert_eq!(request(addr, "GET", path, "").await.0, 404);
    }
    assert_eq!(request(addr, "GET", "/redirect", "").await.0, 307);
    assert_eq!(request(addr, "GET", "/redirect?bad", "").await.0, 500);
    assert_eq!(request(addr, "GET", "/live/3", "").await.0, 401);
    let mut stream = open(
        addr,
        "GET",
        "/live/3?n=7",
        &format!("{auth}X-Request-Id: reader-123\r\n"),
    )
    .await;
    let mut received = Vec::new();
    tokio::time::timeout(Duration::from_secs(3), async {
        let mut buf = [0; 256];
        while !String::from_utf8_lossy(&received).contains("reader /live/3 n=7 reader-123") {
            let n = stream.read(&mut buf).await.unwrap();
            assert_ne!(n, 0);
            received.extend_from_slice(&buf[..n]);
        }
    })
    .await
    .unwrap();
    let scope = last.lock().unwrap().clone().unwrap();
    assert!(!scope.is_disposed());
    let two = tempfile::tempdir().unwrap();
    let next = Ulid::new().to_string();
    bundle(two.path(), &next, "second.js");
    fs::remove_file(two.path().join("second.js")).unwrap();
    assert!(host.reload(two.path(), renderer(&next)).is_err());
    assert_eq!(
        request(addr, "GET", "/", "").await.2,
        format!("page {first}")
    );
    fs::write(two.path().join("second.js"), "abc").unwrap();
    host.reload(two.path(), renderer(&next)).unwrap();
    assert_eq!(
        request(addr, "GET", "/", "").await.2,
        format!("page {next}")
    );
    for f in ["first.js", "second.js"] {
        let (code, head, data) = request(addr, "GET", &format!("/assets/{DIGEST}/{f}"), "").await;
        assert_eq!((code, data.as_str()), (200, "abc"));
        assert!(head.contains("immutable"));
    }
    drop(stream);
    tokio::time::timeout(Duration::from_secs(3), scope.cancellation().cancelled())
        .await
        .unwrap();
    stop.send(()).unwrap();
    server.await.unwrap().unwrap();
}
