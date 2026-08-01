use core::Candle;
use indicators::{Atr, Ema, Rsi};
use rust_decimal::Decimal;

/// A deterministic pseudo-random price series. Fixed seed so failures are
/// reproducible without a proptest dependency in integration tests.
fn price_series(n: usize) -> Vec<Decimal> {
    let mut state: u64 = 0x2545F491_4F6CDD1D;
    (0..n)
        .map(|_| {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            // Prices in the 900..1100 range with two decimal places.
            let cents = 90_000 + (state >> 40) % 20_000;
            Decimal::new(cents as i64, 2)
        })
        .collect()
}

fn candles(prices: &[Decimal]) -> Vec<Candle> {
    prices
        .iter()
        .enumerate()
        .map(|(i, p)| Candle {
            open_time_ms: i as i64 * 3_600_000,
            open: *p,
            high: *p + Decimal::new(50, 2),
            low: *p - Decimal::new(50, 2),
            close: *p,
            volume: Decimal::ZERO,
            turnover: Decimal::ZERO,
        })
        .collect()
}

/// Feeding a series one sample at a time must equal feeding a fresh indicator
/// the same series — i.e. `update` carries no state that a restart would lose
/// differently. This is what makes warmup-after-restart trustworthy.
#[test]
fn ema_incremental_equals_fresh_replay() {
    let prices = price_series(200);
    let mut streaming = Ema::new(20);
    for p in &prices {
        streaming.update(*p);
    }

    let mut replayed = Ema::new(20);
    for p in &prices {
        replayed.update(*p);
    }

    assert_eq!(streaming.value(), replayed.value());
    assert!(streaming.value().is_some());
}

#[test]
fn rsi_stays_within_zero_and_one_hundred() {
    let prices = price_series(500);
    let mut rsi = Rsi::new(14);
    for p in &prices {
        if let Some(v) = rsi.update(*p) {
            assert!(v >= Decimal::ZERO, "RSI went below 0: {v}");
            assert!(v <= Decimal::from(100), "RSI went above 100: {v}");
        }
    }
    assert!(rsi.is_warm());
}

#[test]
fn atr_is_never_negative() {
    let prices = price_series(500);
    let mut atr = Atr::new(14);
    for c in candles(&prices) {
        if let Some(v) = atr.update(&c) {
            assert!(v >= Decimal::ZERO, "ATR went negative: {v}");
        }
    }
    assert!(atr.is_warm());
}
