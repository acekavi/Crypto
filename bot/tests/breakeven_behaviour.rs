//! Proves `EngineLoop::drive_breakeven_stops` moves a winning trade's stop to
//! its entry price, and that the live tick actually calls it.
//!
//! Without this pass the bot trades the backtested 1:5 target with no
//! breakeven stop: PF 1.312 and 21.3% max drawdown, which breaches the 20%
//! total-drawdown halt — the bot would halt itself. The simulator has
//! implemented this rule since `fa85ff4`; these tests are what keep the live
//! engine honest about it.

use std::sync::Arc;

use bot::engine_loop::EngineLoop;
use botcore::{Position, Side, Symbol};
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

/// Entry 100 with the stop-limit at 90 makes 1R exactly 10, so 2R is a last
/// price of 120 for a long and 80 for a short. The trigger sits one unit
/// inside the stop-limit, so the offset the breakeven amend must reuse is 1.
const ENTRY: Decimal = dec!(100);
const LONG_TRIGGER: Decimal = dec!(91);
const LONG_STOP_LIMIT: Decimal = dec!(90);
const SHORT_TRIGGER: Decimal = dec!(109);
const SHORT_STOP_LIMIT: Decimal = dec!(110);
const ATR: Decimal = dec!(5);
const T0: i64 = 1_000_000;

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
async fn stop_moves_to_entry_once_price_reaches_the_breakeven_multiple() {
    let (mut el, mock, _d) = engine_with_recorded_stop(Side::Buy, dec!(120), Some(dec!(2))).await;

    el.drive_breakeven_stops().await.expect("breakeven pass");

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
    // One unit short of 2R.
    let (mut el, mock, _d) = engine_with_recorded_stop(Side::Buy, dec!(119), Some(dec!(2))).await;

    el.drive_breakeven_stops().await.expect("breakeven pass");

    assert!(
        mock.amended_stops().is_empty(),
        "a trade short of its breakeven multiple must keep its original stop"
    );
}

#[tokio::test]
async fn a_short_moves_its_stop_down_to_entry() {
    let (mut el, mock, _d) = engine_with_recorded_stop(Side::Sell, dec!(80), Some(dec!(2))).await;

    el.drive_breakeven_stops().await.expect("breakeven pass");

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
async fn the_stop_is_amended_only_once() {
    let (mut el, mock, _d) = engine_with_recorded_stop(Side::Buy, dec!(130), Some(dec!(2))).await;

    el.drive_breakeven_stops().await.expect("first pass");
    el.drive_breakeven_stops().await.expect("second pass");

    assert_eq!(
        mock.amended_stops().len(),
        1,
        "a stop already at entry must not be re-amended on every subsequent tick"
    );
}

#[tokio::test]
async fn a_failed_amend_is_retried_on_the_next_tick() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mock = Arc::new(
        MockExchange::new()
            .with_positions(vec![position(Side::Buy)])
            .with_tickers(vec![ticker(dec!(130))])
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

    el.drive_breakeven_stops()
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

    el.drive_breakeven_stops().await.expect("second pass");

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
    let (mut el, mock, _d) = engine_with_recorded_stop(Side::Buy, dec!(500), None).await;

    el.drive_breakeven_stops().await.expect("breakeven pass");

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
    let (mut el, _mock, _d) = engine_with_recorded_stop(Side::Buy, dec!(130), Some(dec!(2))).await;

    el.drive_breakeven_stops().await.expect("breakeven pass");

    assert_eq!(
        el.recorded_trigger_for_test(&btc()),
        Some(ENTRY),
        "a successful breakeven amend must rewrite the recorded trigger to entry"
    );
}

#[tokio::test]
async fn one_symbols_failed_amend_does_not_stop_the_others_being_moved() {
    // Positions are iterated in order, so failing the FIRST amend and then
    // asserting the second symbol was still moved is what proves the loop
    // does not bail out on the first error.
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
            .with_tickers(vec![
                ticker(dec!(130)),
                Ticker {
                    symbol: eth.clone(),
                    turnover_24h: Decimal::ZERO,
                    last_price: dec!(130),
                },
            ])
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

    el.drive_breakeven_stops().await.expect("breakeven pass");

    let amends = mock.amended_stops();
    assert_eq!(
        amends.len(),
        1,
        "the second symbol must still have been evaluated after the first failed"
    );
    assert_eq!(amends[0].0, eth);
}

#[tokio::test]
async fn the_engine_tick_runs_the_breakeven_pass() {
    // Guards against the class of bug where a pass is implemented, tested in
    // isolation, and never actually called — as happened with the escalation
    // ladder (299bf40) and the drawdown halt (3a725d5).
    // `drive_position_management` is the single seam `main.rs`'s timer arm
    // calls; if the breakeven call is removed from it, this fails.
    let (mut el, mock, _d) = engine_with_recorded_stop(Side::Buy, dec!(130), Some(dec!(2))).await;

    el.drive_position_management(T0)
        .await
        .expect("position management tick");

    let amends = mock.amended_stops();
    assert_eq!(
        amends.len(),
        1,
        "the live tick must drive the breakeven pass, not just the ladder"
    );
    assert_eq!(amends[0].1, ENTRY);
}

#[tokio::test]
async fn the_engine_tick_still_runs_the_escalation_ladder() {
    // The other half of the seam: adding the breakeven pass must not have
    // displaced the ladder the tick already drove.
    let mock = Arc::new(
        MockExchange::new()
            .with_positions(vec![position(Side::Buy)])
            .with_tickers(vec![ticker(dec!(85))]),
    );
    let (mut el, _j, _d) = loop_with(mock.clone()).await;
    el.protect_for_test(btc(), Side::Buy, LONG_TRIGGER, ATR);

    el.drive_position_management(T0)
        .await
        .expect("position management tick");

    assert_eq!(
        el.triggered_stop_count_for_test(),
        1,
        "a stop observed past its trigger must still start the escalation ladder"
    );
}
