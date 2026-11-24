//! HTTP settings, provided at the app's root (`.provide(HttpConfig::default()
//! .cors_origin("https://app.example"))`) so servers and `TestClient` read
//! the same ones.

use std::net::IpAddr;
use std::time::Duration;

/// The session cookie name unless [`HttpConfig::session_cookie`] sets one.
pub const DEFAULT_SESSION_COOKIE: &str = "hyperish_session";

/// Request bodies larger than this are `413` unless
/// [`HttpConfig::max_body`] sets another limit: 1 MiB.
pub const DEFAULT_MAX_BODY: usize = 1 << 20;

/// HTTP adapter settings.
#[derive(Debug, Clone)]
pub struct HttpConfig {
    pub(crate) max_body: usize,
    pub(crate) cors_origins: Vec<String>,
    pub(crate) trusted_proxies: Vec<String>,
    pub(crate) metrics_from: Vec<String>,
    pub(crate) session_cookie: String,
    pub(crate) shutdown_grace: Duration,
}

impl Default for HttpConfig {
    fn default() -> Self {
        HttpConfig {
            max_body: DEFAULT_MAX_BODY,
            cors_origins: Vec::new(),
            trusted_proxies: Vec::new(),
            metrics_from: vec!["127.0.0.0/8".into(), "::1/128".into()],
            session_cookie: DEFAULT_SESSION_COOKIE.into(),
            shutdown_grace: Duration::from_secs(30),
        }
    }
}

impl HttpConfig {
    /// The largest request body accepted (`413 payload_too_large` above).
    pub fn max_body(mut self, bytes: usize) -> Self {
        self.max_body = bytes;
        self
    }

    /// Allows cross-origin requests, with credentials, from exactly this
    /// origin (`https://app.example`); `*` is refused.
    pub fn cors_origin(mut self, origin: &str) -> Self {
        self.cors_origins.push(origin.to_owned());
        self
    }

    /// Trusts `X-Forwarded-For` and `X-Forwarded-Proto` from peers in this
    /// address range (`10.0.0.0/8`, or one address).
    pub fn trusted_proxy(mut self, cidr: &str) -> Self {
        self.trusted_proxies.push(cidr.to_owned());
        self
    }

    /// Who may read `/metrics` (replaces the default, loopback only).
    pub fn metrics_from(mut self, cidrs: &[&str]) -> Self {
        self.metrics_from = cidrs.iter().map(|c| (*c).to_owned()).collect();
        self
    }

    /// The session cookie's name.
    pub fn session_cookie(mut self, name: &str) -> Self {
        self.session_cookie = name.to_owned();
        self
    }

    /// How long a shutting-down server waits for running requests before
    /// cancelling them (30 s).
    pub fn shutdown_grace(mut self, grace: Duration) -> Self {
        self.shutdown_grace = grace;
        self
    }
}

/// An address range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Cidr {
    net: IpAddr,
    bits: u8,
}

impl Cidr {
    pub(crate) fn parse(s: &str) -> Result<Cidr, String> {
        let bad = || format!("`{s}` is not an address range (`10.0.0.0/8`, `::1`)");
        let (ip, bits) = match s.split_once('/') {
            Some((ip, bits)) => (ip, Some(bits.parse::<u8>().map_err(|_| bad())?)),
            None => (s, None),
        };
        let net: IpAddr = ip.trim().parse().map_err(|_| bad())?;
        let max = if net.is_ipv4() { 32 } else { 128 };
        let bits = bits.unwrap_or(max);
        if bits > max {
            return Err(bad());
        }
        Ok(Cidr { net, bits })
    }

    pub(crate) fn contains(&self, ip: IpAddr) -> bool {
        match (self.net, ip.to_canonical()) {
            (IpAddr::V4(net), IpAddr::V4(ip)) => {
                let mask = u32::MAX.checked_shl(32 - u32::from(self.bits)).unwrap_or(0);
                u32::from(net) & mask == u32::from(ip) & mask
            }
            (IpAddr::V6(net), IpAddr::V6(ip)) => {
                let mask = u128::MAX
                    .checked_shl(128 - u32::from(self.bits))
                    .unwrap_or(0);
                u128::from(net) & mask == u128::from(ip) & mask
            }
            _ => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges() {
        let ten = Cidr::parse("10.0.0.0/8").unwrap();
        assert!(ten.contains("10.1.2.3".parse().unwrap()));
        assert!(
            ten.contains("::ffff:10.1.2.3".parse().unwrap()),
            "IPv4-mapped"
        );
        assert!(!ten.contains("11.0.0.1".parse().unwrap()));
        let one = Cidr::parse("192.168.0.7").unwrap();
        assert!(one.contains("192.168.0.7".parse().unwrap()));
        assert!(!one.contains("192.168.0.8".parse().unwrap()));
        assert!(
            Cidr::parse("::1/128")
                .unwrap()
                .contains("::1".parse().unwrap())
        );
        assert!(
            Cidr::parse("0.0.0.0/0")
                .unwrap()
                .contains("8.8.8.8".parse().unwrap())
        );
        assert!(Cidr::parse("10.0.0.0/33").is_err());
        assert!(Cidr::parse("*").is_err());
    }
}
