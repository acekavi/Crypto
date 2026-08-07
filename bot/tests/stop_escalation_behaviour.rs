//! Proves `EngineLoop::drive_stop_escalation` is what actually wires
//! `next_escalation` and `Executor::widen_stop` into the live loop.
//!
//! Before this driver existed, `escalation.rs` was complete and unit-tested
//! but nothing ever called it: a stop that triggered but did not fill would
//! rest at its initial offset forever. Every test here would fail against an
//! `EngineLoop` with the driver missing or a no-op.

use std::sync::Arc;

use bot::engine_loop::EngineLoop;
use botcore::{Position, Side, Symbol};
use engine::mock::MockExchange;
use exchange::bybit::transport::ExchangeError;
use exchange::bybit::wire::Ticker;
use exchange::bybit::ws_private::AccountEvent;
use persistence::Journal;
use risk::{RiskManager, RiskParams};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use strategy::{PullbackStrategy, pullback::StrategyParams};

fn btc() -> Symbol {
    Symbol::new("BTCUSDT")
}

fn long_position() -> Position {
    Position {
        symbol: btc(),
        side: Side::Buy,
        size: dec!(1),
        entry_price: dec!(42000),
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

const T0: i64 = 1_000_000;
const TIMEOUT: i64 = 30_000; // EscalationLadder::defaults().timeout_ms

// trigger=41000, atr=200 (matches escalation.rs's own fixtures in spirit).
// rung 1 (offset 0.6 ATR): 41000 - 200*0.6 = 40880.
// rung 2 (offset 1.2 ATR): 41000 - 200*1.2 = 40760.
const TRIGGER: Decimal = dec!(41000);
const ATR: Decimal = dec!(200);

#[tokio::test]
async fn a_position_past_its_trigger_starts_the_ladder_without_widening_on_first_observation() {
    // last_price below the trigger means the long's stop fired and did not
    // fill — the case the ladder exists for.
    let mock = Arc::new(
        MockExchange::new()
            .with_positions(vec![long_position()])
            .with_tickers(vec![ticker(dec!(40900))]),
    );
    let (mut el, _j, _d) = loop_with(mock.clone()).await;
    el.protect_for_test(btc(), Side::Buy, TRIGGER, ATR);

    el.drive_stop_escalation(T0).await.expect("driven");

    assert_eq!(
        el.triggered_stop_count_for_test(),
        1,
        "the ladder must start tracking a stop observed past its trigger"
    );
    assert!(
        mock.amended_stops().is_empty(),
        "rung 0 is already resting and must not widen on the same tick it is first observed"
    );
}

#[tokio::test]
async fn a_position_that_has_not_reached_its_trigger_does_nothing() {
    // last_price above the trigger: the stop has not fired.
    let mock = Arc::new(
        MockExchange::new()
            .with_positions(vec![long_position()])
            .with_tickers(vec![ticker(dec!(41500))]),
    );
    let (mut el, _j, _d) = loop_with(mock.clone()).await;
    el.protect_for_test(btc(), Side::Buy, TRIGGER, ATR);

    el.drive_stop_escalation(T0).await.expect("driven");

    assert_eq!(el.triggered_stop_count_for_test(), 0);
    assert!(mock.amended_stops().is_empty());
}

#[tokio::test]
async fn after_the_timeout_it_widens_to_the_rung_one_price() {
    let mock = Arc::new(
        MockExchange::new()
            .with_positions(vec![long_position()])
            .with_tickers(vec![ticker(dec!(40900))]),
    );
    let (mut el, _j, _d) = loop_with(mock.clone()).await;
    el.protect_for_test(btc(), Side::Buy, TRIGGER, ATR);

    el.drive_stop_escalation(T0)
        .await
        .expect("starts the ladder");
    assert!(mock.amended_stops().is_empty(), "setup: no widen yet");

    el.drive_stop_escalation(T0 + TIMEOUT)
        .await
        .expect("widens at the timeout");

    let amends = mock.amended_stops();
    assert_eq!(amends.len(), 1, "exactly one widen must reach the exchange");
    assert_eq!(amends[0].0, btc());
    assert_eq!(
        amends[0].1, TRIGGER,
        "the trigger passed to amend must be unchanged"
    );
    assert_eq!(
        amends[0].2,
        dec!(40880),
        "rung 1's limit must be trigger - 0.6*ATR"
    );
}

#[tokio::test]
async fn a_failed_amend_does_not_advance_the_rung() {
    let mock = Arc::new(
        MockExchange::new()
            .with_positions(vec![long_position()])
            .with_tickers(vec![ticker(dec!(40900))])
            .fail_amend_stop_once(ExchangeError::WebSocket("timeout".into())),
    );
    let (mut el, _j, _d) = loop_with(mock.clone()).await;
    el.protect_for_test(btc(), Side::Buy, TRIGGER, ATR);

    el.drive_stop_escalation(T0)
        .await
        .expect("starts the ladder");

    // First widen attempt fails and must not be recorded.
    el.drive_stop_escalation(T0 + TIMEOUT)
        .await
        .expect("a failed amend must not surface as an error");
    assert!(
        mock.amended_stops().is_empty(),
        "a failed amend must not be recorded as a successful widen"
    );

    // The retry, still past the SAME rung-0 timeout, must target rung 1
    // again — not skip ahead to rung 2 — proving the rung never advanced on
    // the failed attempt above.
    el.drive_stop_escalation(T0 + TIMEOUT + 1)
        .await
        .expect("the retry succeeds");
    let amends = mock.amended_stops();
    assert_eq!(amends.len(), 1, "only the retry reaches the exchange");
    assert_eq!(
        amends[0].2,
        dec!(40880),
        "the retry must still target rung 1's price, not rung 2's"
    );
}

#[tokio::test]
async fn exhaustion_halts_new_entries_places_no_market_order_and_leaves_the_position_open() {
    let mock = Arc::new(
        MockExchange::new()
            .with_positions(vec![long_position()])
            .with_tickers(vec![ticker(dec!(40900))]),
    );
    let (mut el, journal, _d) = loop_with(mock.clone()).await;
    el.protect_for_test(btc(), Side::Buy, TRIGGER, ATR);

    el.drive_stop_escalation(T0).await.expect("rung 0 starts");
    el.drive_stop_escalation(T0 + TIMEOUT)
        .await
        .expect("widens to rung 1");
    el.drive_stop_escalation(T0 + 2 * TIMEOUT)
        .await
        .expect("widens to rung 2");

    assert!(
        journal.halt_reason().await.expect("readable").is_none(),
        "setup: no halt until the ladder is actually exhausted"
    );

    el.drive_stop_escalation(T0 + 3 * TIMEOUT)
        .await
        .expect("rung 2's own timeout exhausts the ladder");

    let reason = journal
        .halt_reason()
        .await
        .expect("readable")
        .expect("exhaustion must persist a halt so no new entries are taken");
    assert!(
        reason.contains("BTCUSDT"),
        "the halt reason must name the exhausted symbol, got: {reason}"
    );

    // Nothing beyond the two widens (rung 1, rung 2) may ever reach the
    // exchange: no third widen, and — the owner's hard rule — no order that
    // could be a market order either.
    assert_eq!(mock.amended_stops().len(), 2);
    assert!(
        mock.placed_orders().is_empty(),
        "exhaustion must never place an order; the position stays open for a human"
    );
    assert_eq!(
        el.triggered_stop_count_for_test(),
        1,
        "the position must remain tracked, not dropped, since it is still open"
    );

    // An exhausted ladder stays exhausted: driving it further must not
    // re-widen or re-halt.
    el.drive_stop_escalation(T0 + 10 * TIMEOUT)
        .await
        .expect("still exhausted, still not an error");
    assert_eq!(
        mock.amended_stops().len(),
        2,
        "an exhausted ladder must never widen again"
    );
}

#[tokio::test]
async fn a_closed_position_drops_both_the_protection_and_the_triggered_stop() {
    let mock = Arc::new(
        MockExchange::new()
            .with_positions(vec![long_position()])
            .with_tickers(vec![ticker(dec!(40900))]),
    );
    let (mut el, _j, _d) = loop_with(mock.clone()).await;
    el.protect_for_test(btc(), Side::Buy, TRIGGER, ATR);
    el.drive_stop_escalation(T0)
        .await
        .expect("starts the ladder");

    assert_eq!(el.protection_count_for_test(), 1, "setup");
    assert_eq!(el.triggered_stop_count_for_test(), 1, "setup");

    el.on_account_event(&AccountEvent::PositionClosed { symbol: btc() })
        .await;

    assert_eq!(
        el.protection_count_for_test(),
        0,
        "a closed position must not keep its protection record — unbounded growth otherwise"
    );
    assert_eq!(
        el.triggered_stop_count_for_test(),
        0,
        "a closed position must not keep its triggered-stop record — unbounded growth otherwise"
    );
}
