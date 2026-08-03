use std::sync::Arc;

use bot::engine_loop::{CandleOutcome, EngineLoop, SkipReason};
use botcore::{Candle, Instrument, Symbol, Timeframe};
use engine::mock::MockExchange;
use persistence::Journal;
use risk::{RiskManager, RiskParams};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use strategy::{PullbackStrategy, pullback::StrategyParams};

const H1: i64 = 3_600_000;

fn instrument() -> Instrument {
    Instrument {
        symbol: Symbol::new("BTCUSDT"),
        tick_size: dec!(0.1),
        qty_step: dec!(0.001),
        min_order_qty: dec!(0.001),
        launch_time_ms: 0,
    }
}

fn candle(open_time_ms: i64) -> Candle {
    Candle {
        open_time_ms,
        open: Decimal::from(100),
        high: Decimal::from(101),
        low: Decimal::from(99),
        close: Decimal::from(100),
        volume: Decimal::ZERO,
        turnover: Decimal::ZERO,
    }
}

async fn loop_with(mock: Arc<MockExchange>) -> (EngineLoop, tempfile::TempDir) {
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
        j,
        vec![instrument()],
        "cfg".into(),
        3,
        250,
    );
    (el, dir)
}

#[tokio::test]
async fn a_cold_stream_never_places_an_order() {
    // Acting on a half-warm indicator is how a restart produces a bad trade.
    let mock = Arc::new(MockExchange::new());
    let (mut el, _d) = loop_with(mock.clone()).await;

    let out = el
        .on_candle_closed(&Symbol::new("BTCUSDT"), Timeframe::H1, &candle(0))
        .await
        .expect("handled");

    assert!(matches!(out, CandleOutcome::Skipped(SkipReason::NotWarm)));
    assert!(mock.placed_orders().is_empty());
}

#[tokio::test]
async fn a_duplicate_candle_is_skipped_without_reaching_the_strategy() {
    let mock = Arc::new(MockExchange::new());
    let (mut el, _d) = loop_with(mock.clone()).await;

    el.on_candle_closed(&Symbol::new("BTCUSDT"), Timeframe::H1, &candle(H1))
        .await
        .expect("first");
    let out = el
        .on_candle_closed(&Symbol::new("BTCUSDT"), Timeframe::H1, &candle(H1))
        .await
        .expect("duplicate");

    assert!(matches!(
        out,
        CandleOutcome::Skipped(SkipReason::NotAccepted)
    ));
}

#[tokio::test]
async fn a_symbol_with_no_instrument_metadata_is_skipped() {
    // Without tick size and qty step no valid order could be formed.
    let mock = Arc::new(MockExchange::new());
    let (mut el, _d) = loop_with(mock.clone()).await;

    let out = el
        .on_candle_closed(&Symbol::new("GHOSTUSDT"), Timeframe::H1, &candle(0))
        .await
        .expect("handled");

    assert!(matches!(
        out,
        CandleOutcome::Skipped(SkipReason::UnknownInstrument)
    ));
    assert!(mock.placed_orders().is_empty());
}

#[tokio::test]
async fn an_expired_resting_order_is_cancelled_on_a_later_candle() {
    use botcore::Side;
    use engine::RestingOrder;

    let mock = Arc::new(MockExchange::new());
    let (mut el, _d) = loop_with(mock.clone()).await;

    el.track_for_test(RestingOrder {
        link_id: "old".into(),
        symbol: Symbol::new("BTCUSDT"),
        side: Side::Buy,
        qty: dec!(1),
        cum_exec_qty: Decimal::ZERO,
        placed_at_candle_ms: 0,
    });

    el.on_candle_closed(&Symbol::new("BTCUSDT"), Timeframe::H1, &candle(3 * H1))
        .await
        .expect("handled");

    assert_eq!(
        mock.cancelled(),
        vec!["old".to_string()],
        "an expired entry must be cancelled"
    );
}

#[tokio::test]
async fn a_zero_equity_account_refuses_rather_than_placing() {
    // An unfunded testnet account must produce a named refusal, not an order.
    use botcore::Balance;
    let mock = Arc::new(MockExchange::new().with_balance(Balance {
        equity: Decimal::ZERO,
        available: Decimal::ZERO,
    }));
    let (mut el, _d) = loop_with(mock.clone()).await;

    for i in 0..300i64 {
        let _ = el
            .on_candle_closed(&Symbol::new("BTCUSDT"), Timeframe::H1, &candle(i * H1))
            .await;
    }
    assert!(
        mock.placed_orders().is_empty(),
        "a zero-equity account must never place an order"
    );
}
