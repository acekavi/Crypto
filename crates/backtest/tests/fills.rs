use backtest::{ExitOutcome, FillOutcome, exit_was_ambiguous, limit_fill, resolve_exit};
use botcore::{Candle, Side};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

fn candle(high: Decimal, low: Decimal) -> Candle {
    Candle {
        open_time_ms: 0,
        open: dec!(100),
        high,
        low,
        close: dec!(100),
        volume: dec!(1),
        turnover: dec!(1),
    }
}

#[test]
fn a_buy_limit_fills_only_when_price_trades_strictly_below_it() {
    // Traded through: low is under the limit.
    assert_eq!(
        limit_fill(Side::Buy, dec!(100), &candle(dec!(105), dec!(99))),
        FillOutcome::Filled { price: dec!(100) }
    );
}

#[test]
fn a_buy_limit_merely_touched_does_not_fill() {
    // THE central rule. Price reached exactly 100 and reversed. The orders
    // already queued at 100 absorbed the volume; a latecomer did not fill.
    // Treating this as a fill is how backtests invent edge.
    assert_eq!(
        limit_fill(Side::Buy, dec!(100), &candle(dec!(105), dec!(100))),
        FillOutcome::NoFill
    );
}

#[test]
fn a_sell_limit_fills_only_when_price_trades_strictly_above_it() {
    assert_eq!(
        limit_fill(Side::Sell, dec!(100), &candle(dec!(101), dec!(95))),
        FillOutcome::Filled { price: dec!(100) }
    );
    assert_eq!(
        limit_fill(Side::Sell, dec!(100), &candle(dec!(100), dec!(95))),
        FillOutcome::NoFill
    );
}

#[test]
fn a_fill_is_always_at_the_limit_price_never_better() {
    // Price traded far through the limit, but the order was resting AT the
    // limit — it does not fill at the extreme of the candle.
    assert_eq!(
        limit_fill(Side::Buy, dec!(100), &candle(dec!(105), dec!(80))),
        FillOutcome::Filled { price: dec!(100) }
    );
}

#[test]
fn a_long_position_stops_out_when_price_trades_below_the_stop_limit() {
    // Long closes by SELLING; its stop sits below.
    assert_eq!(
        resolve_exit(Side::Buy, dec!(98), dec!(104), &candle(dec!(100), dec!(97))),
        ExitOutcome::Stopped { price: dec!(98) }
    );
}

#[test]
fn a_long_position_takes_profit_when_price_trades_above_the_target() {
    assert_eq!(
        resolve_exit(Side::Buy, dec!(98), dec!(104), &candle(dec!(105), dec!(99))),
        ExitOutcome::TargetHit { price: dec!(104) }
    );
}

#[test]
fn a_candle_reaching_both_stop_and_target_resolves_as_stopped() {
    // THE PESSIMISTIC RULE. Both were reachable; the true order within the
    // candle is unknowable, so assume the worst. Resolving this as a target
    // hit would inflate results most on exactly the volatile candles that
    // dominate returns.
    assert_eq!(
        resolve_exit(Side::Buy, dec!(98), dec!(104), &candle(dec!(105), dec!(97))),
        ExitOutcome::Stopped { price: dec!(98) }
    );
    assert!(exit_was_ambiguous(
        Side::Buy,
        dec!(98),
        dec!(104),
        &candle(dec!(105), dec!(97))
    ));
}

#[test]
fn a_short_position_stops_out_when_price_trades_above_the_stop_limit() {
    // Short closes by BUYING; its stop sits ABOVE and its target BELOW.
    // Getting this backwards silently inverts every short trade.
    //
    // The low is deliberately held ABOVE the stop. With a lower low this
    // assertion also passes under inverted direction logic (the low would sit
    // below the stop and register as a hit for the wrong reason), so it would
    // not actually guard the thing its name claims to guard.
    assert_eq!(
        resolve_exit(
            Side::Sell,
            dec!(102),
            dec!(96),
            &candle(dec!(103), dec!(102.5))
        ),
        ExitOutcome::Stopped { price: dec!(102) }
    );
}

#[test]
fn a_short_position_takes_profit_when_price_trades_below_the_target() {
    assert_eq!(
        resolve_exit(
            Side::Sell,
            dec!(102),
            dec!(96),
            &candle(dec!(101), dec!(95))
        ),
        ExitOutcome::TargetHit { price: dec!(96) }
    );
}

#[test]
fn a_short_reaching_both_also_resolves_as_stopped() {
    assert_eq!(
        resolve_exit(
            Side::Sell,
            dec!(102),
            dec!(96),
            &candle(dec!(103), dec!(95))
        ),
        ExitOutcome::Stopped { price: dec!(102) }
    );
}

#[test]
fn a_candle_touching_neither_leaves_the_position_open() {
    assert_eq!(
        resolve_exit(Side::Buy, dec!(98), dec!(104), &candle(dec!(103), dec!(99))),
        ExitOutcome::StillOpen
    );
    assert!(!exit_was_ambiguous(
        Side::Buy,
        dec!(98),
        dec!(104),
        &candle(dec!(103), dec!(99))
    ));
}

#[test]
fn exits_also_require_trade_through_not_touch() {
    // A candle whose low is exactly the stop-limit did not trade through it.
    assert_eq!(
        resolve_exit(Side::Buy, dec!(98), dec!(104), &candle(dec!(103), dec!(98))),
        ExitOutcome::StillOpen
    );
}
