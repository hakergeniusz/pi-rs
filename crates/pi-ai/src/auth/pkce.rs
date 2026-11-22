//! PKCE and small crypto helpers (`packages/ai/src/auth/oauth/pkce.ts`).
//!
//! Ground truth: `base64urlEncode` (`:9-15`) and `generatePKCE` (`:21-34`)
//! use a 32-byte Web Crypto random verifier and its base64url SHA-256 digest.
//! `openai-codex.ts:62-67` additionally uses 16 random bytes as hex `state`.
//!
//! Deviations: Web Crypto is the `sha2` crate plus `base64`, and randomness
//! is read from `/dev/urandom` (a time/pid SHA-256 stream is the fallback on
//! systems without it) because the port has no `rand` dependency.

use std::fmt::Write;
use std::io::Read;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sha2::{Digest, Sha256};

use super::AuthError;

/// `base64urlEncode` (`pkce.ts:9-15`).
#[must_use]
pub fn base64url_encode(bytes: &[u8]) -> String {
    URL_SAFE_NO_PAD.encode(bytes)
}

/// `base64urlEncode(new Uint8Array(hashBuffer))` over the verifier
/// (`pkce.ts:28-31`). Exposed for the RFC 7636 test vector.
#[must_use]
pub fn challenge(verifier: &str) -> String {
    base64url_encode(&Sha256::digest(verifier.as_bytes()))
}

/// `generatePKCE` (`pkce.ts:21-34`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

/// Generate a verifier and its S256 challenge.
///
/// # Errors
/// Only if no source of randomness can be read at all (the fallback never
/// fails, so this is effectively infallible).
#[allow(clippy::unnecessary_wraps)] // mirrors the fallible `generatePKCE` promise
pub fn generate() -> Result<Pkce, AuthError> {
    let verifier = base64url_encode(&random_bytes(32));
    Ok(Pkce {
        challenge: challenge(&verifier),
        verifier,
    })
}

/// `createState` (`openai-codex.ts:62-67`): 16 random bytes as lowercase hex.
#[must_use]
pub fn state_hex() -> String {
    random_bytes(16)
        .iter()
        .fold(String::with_capacity(32), |mut state, byte| {
            write!(state, "{byte:02x}").expect("writing to a String cannot fail");
            state
        })
}

fn random_bytes(length: usize) -> Vec<u8> {
    if let Ok(mut file) = std::fs::File::open("/dev/urandom") {
        let mut bytes = vec![0_u8; length];
        if file.read_exact(&mut bytes).is_ok() {
            return bytes;
        }
    }
    // No `rand` crate in pi-ai: derive a stream from the clock and pid. This
    // path is a portability fallback, not the normal desktop/browser case.
    let seed = format!(
        "{:?}:{}",
        SystemTime::now().duration_since(UNIX_EPOCH),
        std::process::id()
    );
    let mut bytes = Vec::with_capacity(length);
    let mut counter = 0_u64;
    while bytes.len() < length {
        bytes.extend_from_slice(&Sha256::digest(format!("{seed}:{counter}").as_bytes()));
        counter += 1;
    }
    bytes.truncate(length);
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    /// RFC 7636 appendix B.
    #[test]
    fn pkce_challenge_matches_rfc_7636_vector() {
        assert_eq!(
            challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
    }

    #[test]
    fn generated_verifier_is_32_random_bytes_url_safe() {
        let generated = generate().expect("pkce");
        assert_eq!(generated.verifier.len(), 43);
        assert!(
            generated
                .verifier
                .chars()
                .all(|character| character.is_ascii_alphanumeric()
                    || character == '-'
                    || character == '_')
        );
        assert_eq!(generated.challenge, challenge(&generated.verifier));
        assert_ne!(generate().expect("pkce").verifier, generated.verifier);
    }

    #[test]
    fn state_is_32_hex_characters() {
        let state = state_hex();
        assert_eq!(state.len(), 32);
        assert!(state.chars().all(|character| character.is_ascii_hexdigit()));
    }
}
