use botcore::{Side, Symbol};
use sha2::{Digest, Sha256};

/// Prefix so an id is recognisably ours in Bybit's UI and logs.
const PREFIX: &str = "cb";

/// Hex characters kept from the digest. Combined with the prefix this yields a
/// 34-character id, inside Bybit's 36-character limit with room to spare.
const DIGEST_CHARS: usize = 32;

/// A deterministic, idempotent order identifier.
///
/// Derived from the signal rather than randomly generated, so a retry after a
/// network timeout reuses the same id and Bybit deduplicates it. Without this,
/// a request that timed out *after* the exchange accepted it would be retried
/// and open a second position — the single most expensive failure mode
/// available to an order-placing bot.
///
/// A hex digest also guarantees the id needs no escaping in a query string or
/// JSON body, which matters because the same bytes are both transmitted and
/// signed.
pub fn order_link_id(symbol: &Symbol, signal_candle_open_ms: i64, side: Side) -> String {
    let mut hasher = Sha256::new();
    hasher.update(symbol.as_str().as_bytes());
    hasher.update(b"|");
    hasher.update(signal_candle_open_ms.to_be_bytes());
    hasher.update(b"|");
    hasher.update(side.as_bybit().as_bytes());
    let digest = hex::encode(hasher.finalize());
    format!("{PREFIX}{}", &digest[..DIGEST_CHARS])
}

#[cfg(test)]
mod tests {
    use super::*;
    use botcore::{Side, Symbol};

    #[test]
    fn the_same_signal_always_produces_the_same_id() {
        // This is the whole point: a retry after a network timeout must reuse
        // the same id so Bybit deduplicates it instead of opening a second
        // position.
        let a = order_link_id(&Symbol::new("BTCUSDT"), 1_700_000_000_000, Side::Buy);
        let b = order_link_id(&Symbol::new("BTCUSDT"), 1_700_000_000_000, Side::Buy);
        assert_eq!(a, b);
    }

    #[test]
    fn every_input_changes_the_id() {
        let base = order_link_id(&Symbol::new("BTCUSDT"), 1_700_000_000_000, Side::Buy);
        assert_ne!(
            base,
            order_link_id(&Symbol::new("ETHUSDT"), 1_700_000_000_000, Side::Buy)
        );
        assert_ne!(
            base,
            order_link_id(&Symbol::new("BTCUSDT"), 1_700_000_003_600, Side::Buy)
        );
        assert_ne!(
            base,
            order_link_id(&Symbol::new("BTCUSDT"), 1_700_000_000_000, Side::Sell)
        );
    }

    #[test]
    fn the_id_fits_bybits_thirty_six_character_limit() {
        let id = order_link_id(
            &Symbol::new("SOMEVERYLONGSYMBOLNAMEUSDT"),
            i64::MAX,
            Side::Sell,
        );
        assert!(id.len() <= 36, "id was {} chars: {id}", id.len());
        assert!(!id.is_empty());
    }

    #[test]
    fn the_id_is_safe_for_a_url_and_a_json_body() {
        // A raw symbol could in principle carry characters that need escaping;
        // a hex digest cannot.
        let id = order_link_id(&Symbol::new("BTCUSDT"), 1_700_000_000_000, Side::Buy);
        assert!(
            id.chars().all(|c| c.is_ascii_alphanumeric()),
            "id contained a character needing escaping: {id}"
        );
    }

    #[test]
    fn adjacent_candles_do_not_collide() {
        // Consecutive 1h candles are one hour apart; their ids must differ.
        let a = order_link_id(&Symbol::new("BTCUSDT"), 1_700_000_000_000, Side::Buy);
        let b = order_link_id(
            &Symbol::new("BTCUSDT"),
            1_700_000_000_000 + 3_600_000,
            Side::Buy,
        );
        assert_ne!(a, b);
    }
}
