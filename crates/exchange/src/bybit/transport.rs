use std::time::Duration;

use botcore::ErrorClass;

/// Upper bound on a single backoff sleep. A multi-hour outage should keep the
/// bot polling at a sane cadence, not sleeping for days.
pub const MAX_BACKOFF: Duration = Duration::from_secs(60);

#[derive(Debug, thiserror::Error)]
pub enum ExchangeError {
    #[error("http transport error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("bybit returned retCode {code}: {msg}")]
    Api { code: i32, msg: String },

    #[error("failed to decode bybit response: {0}")]
    Decode(String),

    #[error("websocket error: {0}")]
    WebSocket(String),

    #[error("retries exhausted after {attempts} attempts: {last}")]
    RetriesExhausted { attempts: u32, last: Box<ExchangeError> },
}

impl ExchangeError {
    pub fn class(&self) -> ErrorClass {
        match self {
            // A transport failure carries no verdict from the exchange, so it
            // is always safe to retry — orderLinkId makes it idempotent.
            ExchangeError::Http(_) | ExchangeError::WebSocket(_) => ErrorClass::Retryable,
            ExchangeError::Api { code, .. } => classify_ret_code(*code),
            ExchangeError::Decode(_) => ErrorClass::Rejected,
            ExchangeError::RetriesExhausted { last, .. } => last.class(),
        }
    }
}

/// Map a Bybit `retCode` onto an engine reaction.
///
/// Unknown codes default to `Rejected`, never `Retryable`: retrying an
/// unrecognised failure against a live exchange is how a bot spams orders.
pub fn classify_ret_code(ret_code: i32) -> ErrorClass {
    match ret_code {
        10006 | 10016 | 10018 => ErrorClass::Retryable,
        10003 | 10004 | 10005 | 10010 | 33004 => ErrorClass::Fatal,
        _ => ErrorClass::Rejected,
    }
}

/// Exponential backoff with symmetric multiplicative jitter.
///
/// `jitter` is a fraction (0.25 means +/-25%). Jitter matters because every
/// symbol's stream reconnects at once after a network blip; without it they
/// would retry in lockstep and trip the rate limiter.
pub fn backoff_delay(attempt: u32, base_ms: u64, jitter: f64) -> Duration {
    let raw = base_ms.saturating_mul(1u64 << attempt.min(20));
    let capped = Duration::from_millis(raw).min(MAX_BACKOFF);
    if jitter <= 0.0 {
        return capped;
    }
    let factor = 1.0 + (pseudo_unit_random() * 2.0 - 1.0) * jitter;
    Duration::from_secs_f64((capped.as_secs_f64() * factor).max(0.0))
}

/// Small non-cryptographic RNG in [0,1). Avoids pulling in `rand` for jitter.
fn pseudo_unit_random() -> f64 {
    use std::cell::Cell;
    thread_local! {
        static STATE: Cell<u64> = const { Cell::new(0x853C49E6_748FEA9B) };
    }
    STATE.with(|s| {
        let mut x = s.get();
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        s.set(x);
        (x >> 11) as f64 / (1u64 << 53) as f64
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use botcore::ErrorClass;

    #[test]
    fn rate_limit_codes_are_retryable() {
        assert_eq!(classify_ret_code(10006), ErrorClass::Retryable);
        assert_eq!(classify_ret_code(10018), ErrorClass::Retryable);
    }

    #[test]
    fn auth_failures_are_fatal() {
        // 10003 invalid api key, 10004 bad sign, 10005 permission denied.
        assert_eq!(classify_ret_code(10003), ErrorClass::Fatal);
        assert_eq!(classify_ret_code(10004), ErrorClass::Fatal);
        assert_eq!(classify_ret_code(10005), ErrorClass::Fatal);
    }

    #[test]
    fn business_rejections_are_rejected_not_fatal() {
        // 110007 insufficient balance, 110017 price/qty precision.
        assert_eq!(classify_ret_code(110007), ErrorClass::Rejected);
        assert_eq!(classify_ret_code(110017), ErrorClass::Rejected);
    }

    #[test]
    fn unknown_codes_default_to_rejected() {
        // Defaulting to Rejected rather than Retryable is deliberate: an
        // unrecognised failure must not be retried in a loop against a live
        // exchange.
        assert_eq!(classify_ret_code(999_999), ErrorClass::Rejected);
    }

    #[test]
    fn backoff_grows_exponentially_and_is_bounded() {
        let d0 = backoff_delay(0, 200, 0.0);
        let d1 = backoff_delay(1, 200, 0.0);
        let d2 = backoff_delay(2, 200, 0.0);
        assert_eq!(d0.as_millis(), 200);
        assert_eq!(d1.as_millis(), 400);
        assert_eq!(d2.as_millis(), 800);
        // Capped so a long outage never produces an absurd sleep.
        assert!(backoff_delay(30, 200, 0.0) <= MAX_BACKOFF);
    }

    #[test]
    fn jitter_stays_within_the_requested_fraction() {
        for attempt in 0..5 {
            let base = backoff_delay(attempt, 200, 0.0).as_millis() as f64;
            for _ in 0..50 {
                let jittered = backoff_delay(attempt, 200, 0.25).as_millis() as f64;
                assert!(jittered >= base * 0.75, "{jittered} below jitter floor");
                assert!(jittered <= base * 1.25, "{jittered} above jitter ceiling");
            }
        }
    }
}
