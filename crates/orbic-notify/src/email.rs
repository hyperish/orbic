use hyve::{HyveError, HyveResult, Value};
use lettre::{
    AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor, message::Mailbox,
    transport::smtp::authentication::Credentials,
};
use std::{net::SocketAddr, time::Duration};

/// A UTF-8 plain-text message with one envelope recipient.
pub struct Email {
    from: Mailbox,
    to: Mailbox,
    subject: String,
    text: String,
}
impl Email {
    /// Parses both addresses and validates the subject before selecting a provider.
    pub fn new(from: &str, to: &str, subject: &str, text: &str) -> HyveResult<Self> {
        if subject.chars().any(char::is_control) {
            return Err(HyveError::invalid_request("invalid email subject"));
        }
        let address = |s: &str| {
            s.parse::<Mailbox>()
                .map_err(|_| HyveError::invalid_request("invalid email address"))
        };
        Ok(Self {
            from: address(from)?,
            to: address(to)?,
            subject: subject.into(),
            text: text.into(),
        })
    }
    pub(super) fn json(&self) -> Value {
        let mut value = Value::map();
        value.insert("from", self.from.to_string());
        value.insert("to", vec![self.to.to_string()]);
        value.insert("subject", self.subject.clone());
        value.insert("text", self.text.clone());
        value
    }
}

/// SMTP with TLS and optional authentication, or a local development inbox.
pub struct Smtp(AsyncSmtpTransport<Tokio1Executor>);
impl Smtp {
    /// Connects to a TLS relay. STARTTLS requires a successful upgrade.
    pub fn relay(
        host: &str,
        port: u16,
        starttls: bool,
        login: Option<(&str, &str)>,
    ) -> HyveResult<Self> {
        let transport = if starttls {
            AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(host)
        } else {
            AsyncSmtpTransport::<Tokio1Executor>::relay(host)
        };
        let mut builder = transport
            .map_err(|_| HyveError::invalid_request("invalid SMTP TLS relay"))?
            .port(port)
            .timeout(Some(Duration::from_secs(30)));
        if let Some((user, password)) = login {
            builder = builder.credentials(Credentials::new(user.into(), password.into()));
        }
        Ok(Self(builder.build()))
    }
    /// Sends plaintext SMTP to an inbox bound to a loopback address.
    pub fn local(addr: SocketAddr) -> HyveResult<Self> {
        if !addr.ip().is_loopback() {
            return Err(HyveError::invalid_request("local SMTP requires loopback"));
        }
        Ok(Self(
            AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous(addr.ip().to_string())
                .port(addr.port())
                .timeout(Some(Duration::from_secs(15)))
                .build(),
        ))
    }
    /// Delivers the MIME message and waits for the relay's final reply.
    pub async fn send(&self, email: &Email) -> HyveResult<()> {
        let message = Message::builder()
            .from(email.from.clone())
            .to(email.to.clone())
            .subject(&email.subject)
            .body(email.text.clone())
            .map_err(|_| HyveError::invalid_request("invalid email body or envelope"))?;
        self.0.send(message).await.map(|_| ()).map_err(|error| {
            HyveError::infra("smtp_delivery", "SMTP relay did not confirm delivery")
                .retryable(error.is_transient())
        })
    }
}
