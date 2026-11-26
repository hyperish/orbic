use crate::Email;
use hyve::{HyveError, HyveResult, Value, serde::to_json};
use reqwest::{Client, RequestBuilder, Url, header::CONTENT_TYPE};
use std::{net::IpAddr, time::Duration};

/// Apple Push Notification service environment.
#[derive(Copy, Clone)]
pub enum Apns {
    /// Development-signed apps.
    Sandbox,
    /// Distribution-signed apps.
    Production,
}
/// The APNs delivery mode and topic rules.
#[derive(Copy, Clone)]
pub enum PushType {
    /// Visible user notification.
    Alert,
    /// Silent content update at background priority.
    Background,
    /// ActivityKit update, start or end.
    LiveActivity,
}

/// Provider HTTP clients with bounded timeouts and redirects disabled. APNs
/// always uses HTTP/2. Supply a fresh app-managed provider token for each call.
#[derive(Clone)]
pub struct HttpNotifications {
    regular: Client,
    h2: Client,
}
impl HttpNotifications {
    /// Creates the HTTP clients without sending any requests.
    pub fn new() -> HyveResult<Self> {
        let builder = || {
            Client::builder()
                .timeout(Duration::from_secs(30))
                .connect_timeout(Duration::from_secs(4))
                .redirect(reqwest::redirect::Policy::none())
                .retry(reqwest::retry::never())
        };
        let err = |_| HyveError::internal("could not initialize notification TLS client");
        Ok(Self {
            regular: builder().build().map_err(err)?,
            h2: builder().http2_prior_knowledge().build().map_err(err)?,
        })
    }
    /// POSTs a UTF-8 JSON email envelope to the configured mail API.
    pub async fn email(&self, endpoint: &str, api_key: &str, email: &Email) -> HyveResult<()> {
        send(
            self.regular.post(destination(endpoint)?),
            api_key,
            &email.json(),
            8 * 1024 * 1024,
        )
        .await
    }
    /// Sends a notification through the selected APNs environment. For Live
    /// Activities, pass the host app's bundle ID; the topic suffix is appended.
    pub async fn apns(
        &self,
        env: Apns,
        provider_jwt: &str,
        bundle: &str,
        device: &str,
        kind: PushType,
        payload: &Value,
    ) -> HyveResult<()> {
        let origin = match env {
            Apns::Sandbox => "https://api.sandbox.push.apple.com",
            Apns::Production => "https://api.push.apple.com",
        };
        self.apns_at(
            destination(origin)?,
            provider_jwt,
            bundle,
            device,
            kind,
            payload,
        )
        .await
    }
    async fn apns_at(
        &self,
        mut origin: Url,
        jwt: &str,
        bundle: &str,
        device: &str,
        kind: PushType,
        payload: &Value,
    ) -> HyveResult<()> {
        if device.is_empty()
            || device.len() > 512
            || !device.bytes().all(|b| b.is_ascii_hexdigit())
            || bundle.is_empty()
            || !bundle
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-'))
            || !matches!(payload.get("aps"), Some(Value::Map(_)))
        {
            return Err(HyveError::invalid_request(
                "invalid APNs destination or aps payload",
            ));
        }
        let (mode, topic, priority) = match kind {
            PushType::Alert => ("alert", bundle.to_owned(), "10"),
            PushType::Background => ("background", bundle.to_owned(), "5"),
            PushType::LiveActivity => (
                "liveactivity",
                format!("{bundle}.push-type.liveactivity"),
                "10",
            ),
        };
        origin.set_path(&format!("/3/device/{device}"));
        let request = self
            .h2
            .post(origin)
            .header("apns-topic", topic)
            .header("apns-push-type", mode)
            .header("apns-priority", priority);
        send(request, jwt, payload, 4096).await
    }
    /// Sends an FCM v1 message to its token, topic or condition. The bearer is
    /// the app's short-lived OAuth access token.
    pub async fn fcm(&self, project: &str, access_token: &str, message: &Value) -> HyveResult<()> {
        self.fcm_at(
            destination("https://fcm.googleapis.com")?,
            project,
            access_token,
            message,
        )
        .await
    }
    async fn fcm_at(
        &self,
        mut origin: Url,
        project: &str,
        access: &str,
        message: &Value,
    ) -> HyveResult<()> {
        if project.is_empty()
            || !project
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            || ["token", "topic", "condition"]
                .iter()
                .filter(|k| matches!(message.get(k),Some(Value::String(s)) if !s.is_empty()))
                .count()
                != 1
        {
            return Err(HyveError::invalid_request(
                "invalid FCM project or destination",
            ));
        }
        origin.set_path(&format!("/v1/projects/{project}/messages:send"));
        let mut body = Value::map();
        body.insert("message", message.clone());
        send(self.regular.post(origin), access, &body, 64 * 1024).await
    }
}
fn destination(s: &str) -> HyveResult<Url> {
    let url =
        Url::parse(s).map_err(|_| HyveError::invalid_request("invalid notification endpoint"))?;
    let local = url.host_str().is_some_and(|h| {
        h == "localhost"
            || h.trim_matches(['[', ']'])
                .parse::<IpAddr>()
                .is_ok_and(|ip| ip.is_loopback())
    });
    if url.host().is_none()
        || !(url.scheme() == "https" || url.scheme() == "http" && local)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(HyveError::invalid_request(
            "notification endpoint requires HTTPS or loopback HTTP",
        ));
    }
    Ok(url)
}
async fn send(
    request: RequestBuilder,
    credential: &str,
    json: &Value,
    limit: usize,
) -> HyveResult<()> {
    if credential.is_empty() || !credential.bytes().all(|b| b.is_ascii_graphic()) {
        return Err(HyveError::invalid_request("invalid provider credential"));
    }
    let body = to_json(json);
    if body.len() > limit {
        return Err(HyveError::invalid_request("notification payload too large"));
    }
    let response = request
        .bearer_auth(credential)
        .header(CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await
        .map_err(|_| {
            HyveError::infra(
                "notification_transport",
                "provider did not confirm delivery",
            )
        })?;
    let status = response.status();
    if status.is_success() {
        return Ok(());
    }
    Err(HyveError::infra(
        "notification_rejected",
        format!("provider returned HTTP {}", status.as_u16()),
    )
    .retryable(status.as_u16() == 429 || status.is_server_error()))
}
#[cfg(test)]
mod tests;
