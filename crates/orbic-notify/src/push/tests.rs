use super::*;
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Request, Response, body::Incoming, service::service_fn};
use hyper_util::rt::{TokioExecutor, TokioIo};
use std::{
    convert::Infallible,
    sync::{Arc, Mutex},
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
};

#[tokio::test]
async fn real_h2_apns_and_http_fcm_email() {
    let client = HttpNotifications::new().unwrap();
    for h2 in [true, false] {
        let socket = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = destination(&format!("http://{}", socket.local_addr().unwrap())).unwrap();
        let captured = Arc::new(Mutex::new(Vec::new()));
        let records = captured.clone();
        let server = tokio::spawn(async move {
            let (io, _) = socket.accept().await.unwrap();
            let svc = service_fn(move |req: Request<Incoming>| {
                let records = records.clone();
                async move {
                    let (parts, body) = req.into_parts();
                    let body = body.collect().await.unwrap().to_bytes();
                    let value =
                        hyve::serde::from_json::<Value>(std::str::from_utf8(&body).unwrap())
                            .unwrap();
                    let is_mail = parts.uri.path() == "/mail";
                    records.lock().unwrap().push((parts, value));
                    Ok::<_, Infallible>(
                        Response::builder()
                            .status(if is_mail { 429 } else { 200 })
                            .body(Full::new(Bytes::new()))
                            .unwrap(),
                    )
                }
            });
            if h2 {
                hyper::server::conn::http2::Builder::new(TokioExecutor::new())
                    .serve_connection(TokioIo::new(io), svc)
                    .await
                    .unwrap();
            } else {
                hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(io), svc)
                    .await
                    .unwrap();
            }
        });
        if h2 {
            let payload = hyve::serde::from_json::<Value>(
                r#"{"aps":{"timestamp":7,"event":"update","content-state":{"percent":60}}}"#,
            )
            .unwrap();
            client
                .apns_at(
                    origin,
                    "test-apple",
                    "dev.notes",
                    "aabbccdd",
                    PushType::LiveActivity,
                    &payload,
                )
                .await
                .unwrap();
            let seen = captured.lock().unwrap();
            let (meta, data) = &seen[0];
            assert_eq!(meta.version, hyper::Version::HTTP_2);
            assert_eq!(meta.uri.path(), "/3/device/aabbccdd");
            assert_eq!(
                meta.headers["apns-topic"],
                "dev.notes.push-type.liveactivity"
            );
            assert_eq!(meta.headers["apns-push-type"], "liveactivity");
            assert_eq!(meta.headers["authorization"], "Bearer test-apple");
            assert_eq!(data, &payload);
        } else {
            let mut msg = Value::map();
            msg.insert("topic", "reports");
            client
                .fcm_at(origin.clone(), "notes-test", "test-fcm", &msg)
                .await
                .unwrap();
            let email = Email::new(
                "sender@example.test",
                "user@example.test",
                "notes",
                "new note",
            )
            .unwrap();
            assert!(
                client
                    .email(origin.join("mail").unwrap().as_str(), "mail-key", &email)
                    .await
                    .unwrap_err()
                    .is_retryable()
            );
            let seen = captured.lock().unwrap();
            assert_eq!(
                seen[0].0.uri.path(),
                "/v1/projects/notes-test/messages:send"
            );
            assert_eq!(seen[0].0.headers["authorization"], "Bearer test-fcm");
            assert_eq!(seen[0].1.get("message"), Some(&msg));
            assert_eq!(seen[1].1.get("text"), Some(&Value::from("new note")));
        }
        server.abort();
    }
}

#[tokio::test]
async fn smtp_delivers_the_envelope_and_mime_to_a_local_inbox() {
    let socket = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let smtp = crate::Smtp::local(socket.local_addr().unwrap()).unwrap();
    let inbox = tokio::spawn(async move {
        let (io, _) = socket.accept().await.unwrap();
        let (read, mut write) = io.into_split();
        let mut lines = BufReader::new(read).lines();
        write.write_all(b"220 local ESMTP\r\n").await.unwrap();
        let mut transcript = String::new();
        let mut data = false;
        while let Some(line) = lines.next_line().await.unwrap() {
            transcript.push_str(&line);
            transcript.push('\n');
            if data && line != "." {
                continue;
            }
            let reply = if line == "DATA" {
                data = true;
                "354 end with dot\r\n"
            } else if line == "QUIT" {
                break;
            } else {
                data = false;
                "250 OK\r\n"
            };
            write.write_all(reply.as_bytes()).await.unwrap();
            if line == "." {
                break;
            }
        }
        transcript
    });
    smtp.send(
        &Email::new(
            "from@example.test",
            "to@example.test",
            "build ready",
            "Android build complete",
        )
        .unwrap(),
    )
    .await
    .unwrap();
    let wire = inbox.await.unwrap();
    for text in [
        "MAIL FROM:<from@example.test>",
        "RCPT TO:<to@example.test>",
        "Subject: build ready",
        "Android build complete",
    ] {
        assert!(wire.contains(text));
    }
}
