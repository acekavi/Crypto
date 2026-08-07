use botcore::Timeframe;
use history::{Gap, find_gaps};

use botcore::Candle;
use rust_decimal_macros::dec;

const H1: i64 = 3_600_000;

/// Only `open_time_ms` matters for gap detection; the price fields are
/// filler so the test reads as one line per candle.
fn c(open_time_ms: i64) -> Candle {
    Candle {
        open_time_ms,
        open: dec!(100),
        high: dec!(101),
        low: dec!(99),
        close: dec!(100),
        volume: dec!(10),
        turnover: dec!(1000),
    }
}

#[test]
fn consecutive_candles_have_no_gaps() {
    let candles: Vec<_> = (0..5).map(|i| c(i * H1)).collect();
    assert!(find_gaps(&candles, Timeframe::H1).is_empty());
}

#[test]
fn a_single_missing_candle_is_reported() {
    let candles = vec![c(0), c(H1), c(3 * H1)];
    assert_eq!(
        find_gaps(&candles, Timeframe::H1),
        vec![Gap {
            from_ms: H1,
            to_ms: 3 * H1
        }]
    );
}

#[test]
fn several_separate_gaps_are_all_reported() {
    let candles = vec![c(0), c(2 * H1), c(5 * H1)];
    assert_eq!(
        find_gaps(&candles, Timeframe::H1),
        vec![
            Gap {
                from_ms: 0,
                to_ms: 2 * H1
            },
            Gap {
                from_ms: 2 * H1,
                to_ms: 5 * H1
            },
        ]
    );
}

#[test]
fn an_empty_or_single_candle_series_has_no_gaps() {
    assert!(find_gaps(&[], Timeframe::H1).is_empty());
    assert!(find_gaps(&[c(0)], Timeframe::H1).is_empty());
}

#[test]
fn the_h4_timeframe_uses_its_own_spacing() {
    // Candles one hour apart are a GAP on H4, not consecutive — using the
    // wrong duration here would hide every hole on the higher timeframe.
    let candles = vec![c(0), c(H1)];
    assert_eq!(
        find_gaps(&candles, Timeframe::H4),
        vec![Gap {
            from_ms: 0,
            to_ms: H1
        }]
    );
}
