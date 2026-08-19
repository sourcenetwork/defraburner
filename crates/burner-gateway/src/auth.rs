//! Tenant API token issuance and verification. A token is 32 random
//! bytes, shown once (as hex) at issue time; only its sha256 hex digest
//! is ever persisted, in `burner_cell::TenantSpec::token_sha256`.

use anyhow::{Context, Result};
use rand::RngCore;
use rand::rngs::OsRng;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

const TOKEN_BYTES: usize = 32;

/// A freshly issued token: the raw hex string to hand to the caller
/// (shown once, never persisted) and the sha256 hex digest that IS
/// persisted (`TenantSpec::token_sha256`).
#[derive(Debug)]
pub struct IssuedToken {
    pub token_hex: String,
    pub digest_hex: String,
}

/// Generates a fresh 32-byte token and its sha256 digest.
pub fn issue() -> Result<IssuedToken> {
    let mut bytes = [0u8; TOKEN_BYTES];
    OsRng
        .try_fill_bytes(&mut bytes)
        .context("generating tenant token")?;
    let token_hex = hex_encode(&bytes);
    let digest_hex = digest_hex(&token_hex);
    Ok(IssuedToken {
        token_hex,
        digest_hex,
    })
}

/// Sha256 hex digest of `token` (a presented bearer token, or a freshly
/// issued one), for storage in or comparison against
/// `TenantSpec::token_sha256`.
pub fn digest_hex(token: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(token.as_bytes());
    hex_encode(&hasher.finalize())
}

/// Constant-time comparison of two hex digests: guards token lookup
/// against a timing side-channel. Length is checked first (not
/// secret-dependent: both are always sha256 hex, 64 bytes) before the
/// vetted constant-time byte comparison.
pub fn digests_match(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    a.len() == b.len() && bool::from(a.ct_eq(b))
}

fn hex_encode(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn issue_produces_a_64_char_hex_token_and_matching_digest() {
        let issued = issue().unwrap();
        assert_eq!(issued.token_hex.len(), TOKEN_BYTES * 2);
        assert!(issued.token_hex.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(issued.digest_hex, digest_hex(&issued.token_hex));
        assert_eq!(issued.digest_hex.len(), 64, "sha256 hex is 64 chars");
    }

    #[test]
    fn issue_is_random_across_calls() {
        let a = issue().unwrap();
        let b = issue().unwrap();
        assert_ne!(a.token_hex, b.token_hex);
        assert_ne!(a.digest_hex, b.digest_hex);
    }

    #[test]
    fn digest_hex_is_deterministic() {
        assert_eq!(digest_hex("hello"), digest_hex("hello"));
        assert_ne!(digest_hex("hello"), digest_hex("world"));
    }

    #[test]
    fn digests_match_accepts_equal_and_rejects_different() {
        let d = digest_hex("some-token");
        assert!(digests_match(&d, &d));
        assert!(!digests_match(&d, &digest_hex("other-token")));
    }

    #[test]
    fn digests_match_rejects_mismatched_length_without_panicking() {
        assert!(!digests_match("ab", "abcd"));
        assert!(!digests_match("", "ab"));
    }
}
