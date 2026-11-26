//! Credential values: 256 bits from the OS's CSPRNG, base64url (43
//! characters). Stores keep only their SHA-256, so a leaked store holds no
//! usable credential (`hyperish/docs` `02-core-contracts.md` §6).

use hyve::HyveError;
use sha2::{Digest, Sha256};

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";

/// A new credential value.
pub fn new() -> Result<String, HyveError> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| {
        HyveError::infra(
            "service_unavailable",
            format!("the OS random source failed: {e}"),
        )
        .retryable(true)
    })?;
    Ok(base64url(&bytes))
}

fn base64url(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
        for i in 0..=chunk.len() {
            out.push(char::from(ALPHABET[(n >> (18 - 6 * i) & 63) as usize]));
        }
    }
    out
}

/// The store key of a credential: a namespace and the hex SHA-256.
pub fn key(namespace: &str, token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    let mut out = String::with_capacity(namespace.len() + 65);
    out.push_str(namespace);
    out.push(':');
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// Equality that takes the same time wherever the inputs differ.
pub(crate) fn same(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_long_random_and_hashed() {
        let (a, b) = (new().unwrap(), new().unwrap());
        assert_eq!(a.len(), 43);
        assert_ne!(a, b);
        assert!(a.bytes().all(|c| ALPHABET.contains(&c)));
        assert_eq!(base64url(&[0xff, 0xff, 0xff]), "____");
        assert_eq!(base64url(&[0xfb, 0xff]), "-_8");
        assert_eq!(key("s", "x").len(), 2 + 64);
        assert_ne!(key("s", "x"), key("r", "x"));
        assert!(same("abc", "abc") && !same("abc", "abd") && !same("abc", "ab"));
    }
}
