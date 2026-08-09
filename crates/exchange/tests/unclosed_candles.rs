// The kline REST endpoint returns the in-progress candle with no flag marking
// it. Warm-up fed that partial bar to the indicators and recorded its open
// time, so the real close was later rejected as a duplicate — every symbol
// logged Skipped(NotAccepted) on its first live candle.
use botcore::{Candle, Timeframe};
use exchange::bybit::rest::drop_unclosed;
use rust_decimal_macros::dec;

fn candle(open_time_ms: i64) -> Candle {
    Candle {
        open_time_ms,
        open: dec!(1),
        high: dec!(1),
        low: dec!(1),
        close: dec!(1),
        volume: dec!(1),
        turnover: dec!(1),
    }
}

#[test]
fn the_candle_still_forming_is_dropped() {
    let open = 1_786_246_200_000; // an M15 boundary
    let now = open + 400_000; // 6m40s into it
    let kept = drop_unclosed(Timeframe::M15, now, vec![candle(open)]);
    assert!(kept.is_empty(), "an in-progress candle must never be kept");
}

#[test]
fn a_candle_whose_span_has_just_elapsed_is_kept() {
    // Closed exactly on the boundary: the whole span is in the past.
    let open = 1_786_246_200_000;
    let now = open + Timeframe::M15.duration_ms();
    let kept = drop_unclosed(Timeframe::M15, now, vec![candle(open)]);
    assert_eq!(kept.len(), 1, "a candle closing exactly now is complete");
}

#[test]
fn one_tick_short_of_the_close_is_still_forming() {
    let open = 1_786_246_200_000;
    let now = open + Timeframe::M15.duration_ms() - 1;
    assert!(drop_unclosed(Timeframe::M15, now, vec![candle(open)]).is_empty());
}

#[test]
fn only_the_newest_candle_is_dropped_and_the_history_survives() {
    let step = Timeframe::M15.duration_ms();
    let newest = 1_786_246_200_000;
    let batch: Vec<Candle> = (0..5).map(|i| candle(newest - i * step)).collect();
    let kept = drop_unclosed(Timeframe::M15, newest + 60_000, batch);
    assert_eq!(kept.len(), 4, "the four closed candles must all survive");
    assert!(kept.iter().all(|c| c.open_time_ms < newest));
}

#[test]
fn every_timeframe_uses_its_own_span() {
    // A D1 candle six minutes old is still forming; an M5 one is not.
    let open = 1_786_204_800_000;
    let now = open + 360_000;
    assert!(drop_unclosed(Timeframe::D1, now, vec![candle(open)]).is_empty());
    assert_eq!(
        drop_unclosed(Timeframe::M5, now, vec![candle(open)]).len(),
        1
    );
}
