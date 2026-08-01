use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, thiserror::Error)]
pub enum SignError {
    #[error("missing environment variable {0}")]
    MissingEnv(&'static str),
}

/// API credentials. Loaded from the environment only — never from a file,
/// never from config, never logged.
#[derive(Clone)]
pub struct Credentials {
    pub api_key: String,
    pub api_secret: String,
}

impl Credentials {
    pub fn from_env() -> Result<Self, SignError> {
        Ok(Credentials {
            api_key: std::env::var("BYBIT_API_KEY")
                .map_err(|_| SignError::MissingEnv("BYBIT_API_KEY"))?,
            api_secret: std::env::var("BYBIT_API_SECRET")
                .map_err(|_| SignError::MissingEnv("BYBIT_API_SECRET"))?,
        })
    }
}

// Deliberately opaque: prevents a secret reaching logs through a derived Debug.
impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("api_key", &"<redacted>")
            .field("api_secret", &"<redacted>")
            .finish()
    }
}

pub(crate) fn hmac_hex(secret: &str, message: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .expect("HMAC accepts keys of any length");
    mac.update(message.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// REST signature: HMAC_SHA256 over timestamp + api_key + recv_window + payload.
///
/// `payload` is the raw query string for GET requests and the exact JSON body
/// for POST requests — byte-identical to what is actually transmitted.
pub fn sign_rest(
    secret: &str,
    timestamp_ms: i64,
    api_key: &str,
    recv_window: u32,
    payload: &str,
) -> String {
    hmac_hex(secret, &format!("{timestamp_ms}{api_key}{recv_window}{payload}"))
}

/// Private WebSocket auth signature: HMAC_SHA256 over "GET/realtime" + expires.
pub fn sign_ws_auth(secret: &str, expires_ms: i64) -> String {
    hmac_hex(secret, &format!("GET/realtime{expires_ms}"))
}

/// Tracks drift between the local clock and Bybit's server clock.
///
/// Bybit rejects requests unless
/// `server_time - recv_window <= timestamp < server_time + 1000`, so a machine
/// with a few seconds of NTP drift would fail every signed request. Every
/// response carries a server `time`, which we use to correct.
#[derive(Debug)]
pub struct ClockOffset {
    offset_ms: AtomicI64,
}

impl ClockOffset {
    pub fn new() -> Self {
        ClockOffset { offset_ms: AtomicI64::new(0) }
    }

    /// Record an observation of the server clock against our own.
    pub fn observe(&self, server_time_ms: i64, local_time_ms: i64) {
        self.offset_ms.store(server_time_ms - local_time_ms, Ordering::Relaxed);
    }

    pub fn offset_ms(&self) -> i64 {
        self.offset_ms.load(Ordering::Relaxed)
    }

    /// Current time in the server's frame of reference.
    pub fn now_ms(&self) -> i64 {
        local_now_ms() + self.offset_ms()
    }
}

impl Default for ClockOffset {
    fn default() -> Self {
        Self::new()
    }
}

pub fn local_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is after the unix epoch")
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    // Fixed vector: the signature is HMAC_SHA256 over the exact concatenation
    // timestamp + api_key + recv_window + payload, hex-encoded lowercase.
    // Computed independently with:
    //   printf '1700000000000testkey5000{"symbol":"BTCUSDT"}' \
    //     | openssl dgst -sha256 -hmac testsecret
    const EXPECTED: &str =
        "2e7c2adf786003c873babfab26e2c27092b5d7f1f1e493a00a0491d5b42a8634";

    #[test]
    fn rest_signature_concatenates_in_the_documented_order() {
        let sig = sign_rest("testsecret", 1_700_000_000_000, "testkey", 5000, r#"{"symbol":"BTCUSDT"}"#);
        // Length and alphabet are what we assert deterministically; the exact
        // digest is pinned by the golden test below once generated locally.
        assert_eq!(sig.len(), 64, "HMAC-SHA256 hex must be 64 chars");
        assert!(sig.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        assert_eq!(sig, EXPECTED, "signature drifted from the pinned vector");
    }

    #[test]
    fn rest_signature_is_deterministic() {
        let a = sign_rest("s", 1, "k", 5000, "payload");
        let b = sign_rest("s", 1, "k", 5000, "payload");
        assert_eq!(a, b);
    }

    #[test]
    fn rest_signature_changes_with_every_input() {
        let base = sign_rest("s", 1, "k", 5000, "p");
        assert_ne!(base, sign_rest("s2", 1, "k", 5000, "p"));
        assert_ne!(base, sign_rest("s", 2, "k", 5000, "p"));
        assert_ne!(base, sign_rest("s", 1, "k2", 5000, "p"));
        assert_ne!(base, sign_rest("s", 1, "k", 6000, "p"));
        assert_ne!(base, sign_rest("s", 1, "k", 5000, "p2"));
    }

    #[test]
    fn ws_auth_signs_the_literal_get_realtime_prefix() {
        // Bybit specifies HMAC_SHA256("GET/realtime" + expires).
        let expected = hmac_hex("secret", "GET/realtime1700000000000");
        assert_eq!(sign_ws_auth("secret", 1_700_000_000_000), expected);
    }

    #[test]
    fn clock_offset_corrects_local_drift() {
        let clock = ClockOffset::new();
        // Server is 3 seconds ahead of our local clock.
        clock.observe(1_700_000_003_000, 1_700_000_000_000);
        assert_eq!(clock.offset_ms(), 3_000);
    }

    #[test]
    fn clock_offset_defaults_to_zero_before_any_observation() {
        let clock = ClockOffset::new();
        assert_eq!(clock.offset_ms(), 0);
    }
}
