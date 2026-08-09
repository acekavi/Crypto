//! Proves `EngineLoop::drive_breakeven_stops` moves a winning trade's stop to
//! its entry price, that it reads the closed candle rather than the ticker,
//! and that the live candle arm actually calls it.
//!
//! Without this pass the bot trades the backtested 1:5 target with no
//! breakeven stop: PF 1.312 and 21.3% max drawdown, which breaches the 20%
//! total-drawdown halt — the bot would halt itself. The simulator has
//! implemented this rule since `fa85ff4`; these tests are what keep the live
//! engine honest about it.
//!
//! The threshold is measured against the CLOSED CANDLE's high/low, on the
//! strategy's finest declared timeframe — exactly what
//! `SimulatedExchange::advance` does, and it is called for exactly the ticks
//! `run_backtest` settles against (`Some(tick.tf) == finest`). Sampling the
//! ticker on a timer instead, as this pass used to, missed every wick that
//! crossed the threshold and retraced between two polls.

use std::sync::Arc;

use bot::engine_loop::EngineLoop;
use botcore::{Candle, Position, Side, Symbol, Timeframe};
use engine::mock::MockExchange;
use exchange::bybit::transport::ExchangeError;
use exchange::bybit::wire::Ticker;
use persistence::Journal;
use risk::{RiskManager, RiskParams};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use strategy::{PullbackStrategy, pullback::StrategyParams};

fn btc() -> Symbol {
    Symbol::new("BTCUSDT")
}

fn position(side: Side) -> Position {
    Position {
        symbol: btc(),
        side,
        size: dec!(1),
        entry_price: ENTRY,
        liq_price: None,
        unrealized_pnl: Decimal::ZERO,
    }
}

fn ticker(last_price: Decimal) -> Ticker {
    Ticker {
        symbol: btc(),
        turnover_24h: Decimal::ZERO,
        last_price,
    }
}

/// A candle that traded between `low` and `high` and closed back at `close` —
/// the shape the ticker-sampled pass could not see.
fn candle(high: Decimal, low: Decimal, close: Decimal) -> Candle {
    Candle {
        open_time_ms: T0,
        open: close,
        high,
        low,
        close,
        volume: Decimal::ZERO,
        turnover: Decimal::ZERO,
    }
}

/// Entry 100 with the stop-limit at 90 makes 1R exactly 10, so 2R is a candle
/// high of 120 for a long and a low of 80 for a short. The trigger sits one
/// unit inside the stop-limit, so the offset the breakeven amend must reuse
/// is 1.
const ENTRY: Decimal = dec!(100);
const LONG_TRIGGER: Decimal = dec!(91);
const LONG_STOP_LIMIT: Decimal = dec!(90);
const SHORT_TRIGGER: Decimal = dec!(109);
const SHORT_STOP_LIMIT: Decimal = dec!(110);
const ATR: Decimal = dec!(5);
const T0: i64 = 1_000_000;

/// `PullbackStrategy` declares H1 and H4, so H1 is its finest — the timeframe
/// it signals on, and therefore the one the breakeven check rides. Named here
/// rather than written literally at each call site so the tests say *why* H1.
const EXECUTION_TF: Timeframe = Timeframe::H1;
const STRUCTURE_TF: Timeframe = Timeframe::H4;

async fn loop_with(mock: Arc<MockExchange>) -> (EngineLoop, Arc<Journal>, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let j = Arc::new(
        Journal::open_local(dir.path().join("l.db").to_str().unwrap())
            .await
            .expect("journal"),
    );
    let el = EngineLoop::new(
        Box::new(PullbackStrategy::new(StrategyParams::defaults())),
        RiskManager::new(RiskParams::defaults(), dec!(0.3)),
        mock,
        j.clone(),
        vec![],
        "cfg".into(),
        3,
        250,
    );
    (el, j, dir)
}

/// An engine holding one open position with its stop recorded, exactly as
/// `on_candle_closed` records it after a successful `place_entry`.
///
/// `last_price` seeds the ticker feed only, and every test here leaves it
/// *short* of the threshold: nothing the breakeven pass decides may depend on
/// it any more.
async fn engine_with_recorded_stop(
    side: Side,
    last_price: Decimal,
    breakeven_at_r: Option<Decimal>,
) -> (EngineLoop, Arc<MockExchange>, tempfile::TempDir) {
    let mock = Arc::new(
        MockExchange::new()
            .with_positions(vec![position(side)])
            .with_tickers(vec![ticker(last_price)]),
    );
    let (mut el, _j, dir) = loop_with(mock.clone()).await;
    let (trigger, stop_limit) = match side {
        Side::Buy => (LONG_TRIGGER, LONG_STOP_LIMIT),
        Side::Sell => (SHORT_TRIGGER, SHORT_STOP_LIMIT),
    };
    el.protect_with_breakeven_for_test(
        btc(),
        side,
        ENTRY,
        trigger,
        stop_limit,
        ATR,
        breakeven_at_r,
    );
    (el, mock, dir)
}

#[tokio::test]
async fn stop_moves_to_entry_once_the_candle_reaches_the_breakeven_multiple() {
    // The wick reaches 2R and the candle closes back at 105, with the ticker
    // still reporting 105. Under the old ticker-sampled pass this trade never
    // moved its stop; in the backtest that produced the validated numbers it
    // always did.
    let (mut el, mock, _d) = engine_with_recorded_stop(Side::Buy, dec!(105), Some(dec!(2))).await;

    el.drive_breakeven_stops(
        &btc(),
        EXECUTION_TF,
        &candle(dec!(120), dec!(104), dec!(105)),
    )
    .await
    .expect("breakeven pass");

    let amends = mock.amended_stops();
    assert_eq!(amends.len(), 1, "exactly one amend must reach the exchange");
    assert_eq!(amends[0].0, btc());
    assert_eq!(
        amends[0].1, ENTRY,
        "the trigger must be the entry price exactly, not entry plus a tick"
    );
    assert!(
        amends[0].2 < ENTRY,
        "a long's stop-limit sits BELOW its trigger, since that is where a stopped trade fills; got {}",
        amends[0].2
    );
    assert_eq!(
        amends[0].2,
        dec!(99),
        "the breakeven limit must reuse the offset the original stop was placed with"
    );
    assert!(
        mock.placed_orders().is_empty(),
        "the breakeven pass must amend an existing stop, never place an order"
    );
}

#[tokio::test]
async fn stop_does_not_move_before_the_breakeven_multiple() {
    // One unit short of 2R at the candle's extreme.
    let (mut el, mock, _d) = engine_with_recorded_stop(Side::Buy, dec!(105), Some(dec!(2))).await;

    el.drive_breakeven_stops(
        &btc(),
        EXECUTION_TF,
        &candle(dec!(119), dec!(104), dec!(105)),
    )
    .await
    .expect("breakeven pass");

    assert!(
        mock.amended_stops().is_empty(),
        "a trade short of its breakeven multiple must keep its original stop"
    );
}

#[tokio::test]
async fn a_short_moves_its_stop_down_to_entry() {
    let (mut el, mock, _d) = engine_with_recorded_stop(Side::Sell, dec!(95), Some(dec!(2))).await;

    el.drive_breakeven_stops(&btc(), EXECUTION_TF, &candle(dec!(96), dec!(80), dec!(95)))
        .await
        .expect("breakeven pass");

    let amends = mock.amended_stops();
    assert_eq!(amends.len(), 1);
    assert_eq!(amends[0].1, ENTRY);
    assert!(
        amends[0].2 > ENTRY,
        "a short's stop-limit sits ABOVE its trigger; got {}",
        amends[0].2
    );
    assert_eq!(amends[0].2, dec!(101));
}

#[tokio::test]
async fn a_shorts_high_is_not_what_arms_it() {
    // The mirror of the long case: a short is measured against the candle's
    // LOW. A candle whose high is far above entry has moved against the trade,
    // and reading the wrong extreme would pull the stop up on a loser.
    let (mut el, mock, _d) = engine_with_recorded_stop(Side::Sell, dec!(105), Some(dec!(2))).await;

    el.drive_breakeven_stops(
        &btc(),
        EXECUTION_TF,
        &candle(dec!(130), dec!(99), dec!(105)),
    )
    .await
    .expect("breakeven pass");

    assert!(
        mock.amended_stops().is_empty(),
        "a short that never traded down to 2R must keep its original stop"
    );
}

#[tokio::test]
async fn only_the_execution_timeframe_arms_the_breakeven_stop() {
    // `run_backtest` settles — and so applies breakeven — only on ticks whose
    // timeframe is the strategy's finest. An H4 candle spans price action four
    // H1 candles already covered, so acting on it too would apply the rule
    // twice for the same move.
    let (mut el, mock, _d) = engine_with_recorded_stop(Side::Buy, dec!(105), Some(dec!(2))).await;

    el.drive_breakeven_stops(
        &btc(),
        STRUCTURE_TF,
        &candle(dec!(120), dec!(104), dec!(105)),
    )
    .await
    .expect("breakeven pass");

    assert!(
        mock.amended_stops().is_empty(),
        "only the finest declared timeframe drives this check"
    );
}

#[tokio::test]
async fn a_symbol_with_no_open_position_is_never_amended() {
    // A protection outlives its position until the escalation ladder prunes
    // it. Amending a stop for a position that has already closed is an error
    // at the real exchange.
    let mock = Arc::new(MockExchange::new().with_positions(vec![]));
    let (mut el, _j, _d) = loop_with(mock.clone()).await;
    el.protect_with_breakeven_for_test(
        btc(),
        Side::Buy,
        ENTRY,
        LONG_TRIGGER,
        LONG_STOP_LIMIT,
        ATR,
        Some(dec!(2)),
    );

    el.drive_breakeven_stops(
        &btc(),
        EXECUTION_TF,
        &candle(dec!(130), dec!(104), dec!(105)),
    )
    .await
    .expect("breakeven pass");

    assert!(mock.amended_stops().is_empty());
}

#[tokio::test]
async fn the_stop_is_amended_only_once() {
    let (mut el, mock, _d) = engine_with_recorded_stop(Side::Buy, dec!(105), Some(dec!(2))).await;
    let hot = candle(dec!(130), dec!(104), dec!(105));

    el.drive_breakeven_stops(&btc(), EXECUTION_TF, &hot)
        .await
        .expect("first candle");
    el.drive_breakeven_stops(&btc(), EXECUTION_TF, &hot)
        .await
        .expect("second candle");

    assert_eq!(
        mock.amended_stops().len(),
        1,
        "a stop already at entry must not be re-amended on every subsequent candle"
    );
}

#[tokio::test]
async fn a_failed_amend_is_retried_on_the_next_candle() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mock = Arc::new(
        MockExchange::new()
            .with_positions(vec![position(Side::Buy)])
            .with_tickers(vec![ticker(dec!(105))])
            .fail_amend_stop_once(ExchangeError::WebSocket("timeout".into())),
    );
    let j = Arc::new(
        Journal::open_local(dir.path().join("l.db").to_str().unwrap())
            .await
            .expect("journal"),
    );
    let mut el = EngineLoop::new(
        Box::new(PullbackStrategy::new(StrategyParams::defaults())),
        RiskManager::new(RiskParams::defaults(), dec!(0.3)),
        mock.clone(),
        j,
        vec![],
        "cfg".into(),
        3,
        250,
    );
    el.protect_with_breakeven_for_test(
        btc(),
        Side::Buy,
        ENTRY,
        LONG_TRIGGER,
        LONG_STOP_LIMIT,
        ATR,
        Some(dec!(2)),
    );
    let hot = candle(dec!(130), dec!(104), dec!(105));

    el.drive_breakeven_stops(&btc(), EXECUTION_TF, &hot)
        .await
        .expect("a failed amend must not abort the pass");
    assert!(
        mock.amended_stops().is_empty(),
        "a failed amend must not be recorded as a successful move"
    );
    assert_eq!(
        el.recorded_trigger_for_test(&btc()),
        Some(LONG_TRIGGER),
        "a failed amend must leave the recorded trigger where it was"
    );

    el.drive_breakeven_stops(&btc(), EXECUTION_TF, &hot)
        .await
        .expect("second candle");

    let amends = mock.amended_stops();
    assert_eq!(
        amends.len(),
        1,
        "the failure must be retried, not silently skipped"
    );
    assert_eq!(amends[0].1, ENTRY);
}

#[tokio::test]
async fn a_position_with_no_breakeven_threshold_is_left_alone() {
    // Far past any plausible multiple: only the missing threshold can be what
    // holds the stop in place.
    let (mut el, mock, _d) = engine_with_recorded_stop(Side::Buy, dec!(105), None).await;

    el.drive_breakeven_stops(
        &btc(),
        EXECUTION_TF,
        &candle(dec!(500), dec!(104), dec!(105)),
    )
    .await
    .expect("breakeven pass");

    assert!(
        mock.amended_stops().is_empty(),
        "the strategy, not this pass, decides whether a stop moves"
    );
}

#[tokio::test]
async fn a_moved_stop_becomes_the_trigger_the_escalation_ladder_measures_from() {
    // Otherwise the ladder keeps watching the original stop, far below the
    // one now resting at the exchange, and a breakeven stop that triggers
    // without filling would never escalate.
    let (mut el, _mock, _d) = engine_with_recorded_stop(Side::Buy, dec!(105), Some(dec!(2))).await;

    el.drive_breakeven_stops(
        &btc(),
        EXECUTION_TF,
        &candle(dec!(130), dec!(104), dec!(105)),
    )
    .await
    .expect("breakeven pass");

    assert_eq!(
        el.recorded_trigger_for_test(&btc()),
        Some(ENTRY),
        "a successful breakeven amend must rewrite the recorded trigger to entry"
    );
}

#[tokio::test]
async fn one_symbols_failed_amend_does_not_stop_the_others_being_moved() {
    // Each symbol's breakeven now rides its own closed candle, so the
    // isolation that matters is across calls: a failure on BTC's candle must
    // leave ETH's candle, arriving next, fully able to move its stop.
    let eth = Symbol::new("ETHUSDT");
    let eth_position = Position {
        symbol: eth.clone(),
        side: Side::Buy,
        size: dec!(1),
        entry_price: ENTRY,
        liq_price: None,
        unrealized_pnl: Decimal::ZERO,
    };
    let mock = Arc::new(
        MockExchange::new()
            .with_positions(vec![position(Side::Buy), eth_position])
            .fail_amend_stop_once(ExchangeError::WebSocket("timeout".into())),
    );
    let (mut el, _j, _d) = loop_with(mock.clone()).await;
    for symbol in [btc(), eth.clone()] {
        el.protect_with_breakeven_for_test(
            symbol,
            Side::Buy,
            ENTRY,
            LONG_TRIGGER,
            LONG_STOP_LIMIT,
            ATR,
            Some(dec!(2)),
        );
    }
    let hot = candle(dec!(130), dec!(104), dec!(105));

    el.drive_breakeven_stops(&btc(), EXECUTION_TF, &hot)
        .await
        .expect("btc candle");
    el.drive_breakeven_stops(&eth, EXECUTION_TF, &hot)
        .await
        .expect("eth candle");

    let amends = mock.amended_stops();
    assert_eq!(
        amends.len(),
        1,
        "the second symbol must still have been evaluated after the first failed"
    );
    assert_eq!(amends[0].0, eth);
}

#[tokio::test]
async fn the_candle_arm_runs_the_breakeven_pass() {
    // Guards against the class of bug where a pass is implemented, tested in
    // isolation, and never actually called — as happened with the escalation
    // ladder (299bf40) and the drawdown halt (3a725d5).
    // `drive_candle_close` is the single seam `main.rs`'s candle arm calls; if
    // the breakeven call is removed from it, this fails.
    let (mut el, mock, _d) = engine_with_recorded_stop(Side::Buy, dec!(105), Some(dec!(2))).await;

    el.drive_candle_close(
        &btc(),
        EXECUTION_TF,
        &candle(dec!(130), dec!(104), dec!(105)),
    )
    .await
    .expect("candle close");

    let amends = mock.amended_stops();
    assert_eq!(
        amends.len(),
        1,
        "the live candle arm must drive the breakeven pass, not just the strategy"
    );
    assert_eq!(amends[0].1, ENTRY);
}

#[tokio::test]
async fn the_timer_still_runs_the_escalation_ladder() {
    // The other half of the split: taking breakeven off the timer must not
    // have taken the ladder off it too. The ladder stays on the timer because
    // it reacts to a stop that has already fired and not filled — a genuinely
    // time-sensitive condition, unlike a threshold that a closed candle
    // records exactly.
    let mock = Arc::new(
        MockExchange::new()
            .with_positions(vec![position(Side::Buy)])
            .with_tickers(vec![ticker(dec!(85))]),
    );
    let (mut el, _j, _d) = loop_with(mock.clone()).await;
    el.protect_for_test(btc(), Side::Buy, LONG_TRIGGER, ATR);

    el.drive_stop_escalation(T0)
        .await
        .expect("position management tick");

    assert_eq!(
        el.triggered_stop_count_for_test(),
        1,
        "a stop observed past its trigger must still start the escalation ladder"
    );
}
