//! PKCE S256 (RFC 7636), random values, and the input rules of protocol 01.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

use crate::error::Error;

/// `bytes` random bytes, base64url without padding.
pub(crate) fn random_base64url(bytes: usize) -> Result<Zeroizing<String>, Error> {
    let mut buffer = Zeroizing::new(vec![0u8; bytes]);
    getrandom::fill(&mut buffer[..]).map_err(|_| Error::InvalidInput {
        field: "random source",
        reason: "the operating system's random generator failed",
    })?;
    Ok(Zeroizing::new(URL_SAFE_NO_PAD.encode(&*buffer)))
}

/// A verifier (64 random bytes, 86 characters) and its S256 challenge.
pub(crate) struct Pkce {
    pub verifier: Zeroizing<String>,
    pub challenge: String,
}

impl Pkce {
    pub fn new() -> Result<Self, Error> {
        let verifier = random_base64url(64)?;
        let challenge = challenge(&verifier);
        Ok(Self {
            verifier,
            challenge,
        })
    }
}

/// `BASE64URL(SHA256(verifier))`.
pub(crate) fn challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// 1–24 scopes of `[A-Za-z0-9.\-_:]`, ≤ 96 characters each (the server's
/// limits), trimmed, deduplicated and sorted.
pub(crate) fn normalize_scopes(scopes: &[String]) -> Result<Vec<String>, Error> {
    let mut result: Vec<String> = scopes
        .iter()
        .map(|scope| scope.trim().to_owned())
        .filter(|scope| !scope.is_empty())
        .collect();
    result.sort();
    result.dedup();
    let valid = !result.is_empty()
        && result.len() <= 24
        && result.iter().all(|scope| {
            scope.len() <= 96
                && scope
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'-' | b'_' | b':'))
        });
    if valid {
        Ok(result)
    } else {
        Err(Error::InvalidInput {
            field: "scopes",
            reason: "1-24 scopes of A-Z a-z 0-9 . - _ :, at most 96 characters each",
        })
    }
}

/// A game server's binding challenge: 32–256 of `[A-Za-z0-9._~-]`.
pub(crate) fn valid_challenge(value: &str) -> bool {
    (32..=256).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'~' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc7636_appendix_b() {
        assert_eq!(
            challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn verifier_shape() {
        let pkce = Pkce::new().unwrap();
        assert_eq!(pkce.verifier.len(), 86);
        assert_eq!(pkce.challenge.len(), 43);
        assert_ne!(*Pkce::new().unwrap().verifier, *pkce.verifier);
    }

    #[test]
    fn scopes() {
        let normalized = normalize_scopes(&[
            "voice.join".into(),
            " identity.basic ".into(),
            "voice.join".into(),
        ])
        .unwrap();
        assert_eq!(normalized, ["identity.basic", "voice.join"]);
        assert!(normalize_scopes(&[]).is_err());
        assert!(normalize_scopes(&["  ".into()]).is_err());
        assert!(normalize_scopes(&["bad scope".into()]).is_err());
        assert!(normalize_scopes(&["a".repeat(97)]).is_err());
        let many: Vec<String> = (0..25).map(|i| format!("s{i}")).collect();
        assert!(normalize_scopes(&many).is_err());
        assert!(normalize_scopes(&many[..24]).is_ok());
    }

    #[test]
    fn challenges() {
        assert!(valid_challenge(&"a".repeat(32)));
        assert!(!valid_challenge(&"a".repeat(31)));
        assert!(!valid_challenge(&"a".repeat(257)));
        assert!(!valid_challenge(&format!("{}+", "a".repeat(40))));
    }
}
