//! Standard Webhooks delivery through the running Orbic HTTP server.
#![cfg(feature = "notify")]
use base64::{Engine, engine::general_purpose::STANDARD};
use hyve::{Ctx, HyveError, HyveResult, Timestamp};
use orbic::{
    Db, OrbicApp, component,
    notify::{Delivery, DeliveryOutcome, Webhooks},
};
use orbic_db::{Sql, bind, projection, testing::TempPostgres};
use ring::hmac;
use std::sync::atomic::{AtomicBool, Ordering};

projection!(Count,"mail_receipts", total:i64);
async fn receive(
    hooks: Ctx<Webhooks>,
    fail_first: Ctx<AtomicBool>,
    delivery: Delivery,
) -> HyveResult<bool> {
    let received = hooks
        .accept(delivery, |tx, bytes| async move {
            let body = String::from_utf8(bytes).unwrap();
            Sql::<()>::new(
                "INSERT INTO mail_receipts(body) VALUES($1)",
                &["sqlite", "postgres"],
                &[],
                vec![bind(&body)],
            )
            .execute(&tx)
            .await?;
            if fail_first.swap(false, Ordering::Relaxed) {
                return Err(HyveError::internal("retry consumer"));
            }
            Ok(())
        })
        .await?;
    Ok(received == DeliveryOutcome::Applied)
}

async fn deliveries(db: Db) {
    db.execute_script("CREATE TABLE mail_receipts(body TEXT NOT NULL)")
        .await
        .unwrap();
    let key = [8u8; 32];
    let secret = format!("whsec_{}", STANDARD.encode(key));
    let hooks = Webhooks::new(db.clone(), "billing", &[&secret])
        .await
        .unwrap();
    let app = OrbicApp::new()
        .provide(orbic::Cache::in_memory())
        .provide(hooks)
        .provide(AtomicBool::new(true))
        .mount("/v1", component().post("/receive", receive));
    let socket = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/v1/receive", socket.local_addr().unwrap());
    let (stop, done) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(orbic::serve(app, socket, async {
        let _ = done.await;
    }));
    let http = reqwest::Client::new();
    let now = Timestamp::now().as_unix_micros() / 1_000_000;
    let body = "{ \"sent\" : true }";
    let request = |id: &str, timestamp: i64, signed: &str, actual: &str| {
        let data = format!("{id}.{timestamp}.{signed}");
        let tag = hmac::sign(&hmac::Key::new(hmac::HMAC_SHA256, &key), data.as_bytes());
        http.post(&url)
            .header("content-type", "application/json")
            .header("webhook-id", id)
            .header("webhook-timestamp", timestamp.to_string())
            .header(
                "webhook-signature",
                format!("v2,unused v1,{}", STANDARD.encode(tag.as_ref())),
            )
            .body(actual.to_owned())
            .send()
    };
    assert_eq!(
        request("msg-1", now - 600, body, body)
            .await
            .unwrap()
            .status(),
        401
    );
    assert_eq!(
        request("msg-1", now, body, "{}").await.unwrap().status(),
        401
    );
    assert_eq!(
        request("msg-1", now, body, body).await.unwrap().status(),
        500
    );
    let (x, y) = tokio::join!(
        request("msg-1", now, body, body),
        request("msg-1", now, body, body)
    );
    let mut answers = vec![];
    for result in [x, y] {
        let response = result.unwrap();
        assert_eq!(response.status(), 200);
        answers.push(response.text().await.unwrap());
    }
    assert!(answers.iter().any(|s| s.contains("true")));
    assert!(answers.iter().any(|s| s.contains("false")));
    let conflict = request("msg-1", now, "{}", "{}").await.unwrap();
    assert_eq!(conflict.status(), 409);
    let count = Sql::<Count>::new(
        "SELECT COUNT(*) AS total FROM mail_receipts",
        &["sqlite", "postgres"],
        &[],
        vec![],
    )
    .fetch_one(&db)
    .await
    .unwrap();
    assert_eq!(count.total, 1);
    stop.send(()).unwrap();
    server.await.unwrap().unwrap();
}
#[tokio::test]
async fn sqlite_signed_webhook_delivery() {
    deliveries(Db::empty_in_memory().await).await;
}
#[tokio::test]
async fn postgres_signed_webhook_delivery() {
    let pg = TempPostgres::new().await;
    deliveries(pg.db().clone()).await;
    pg.close().await;
}
