use botcore::{Balance, LimitEntry, Side, Symbol, Timeframe};
use engine::mock::MockExchange;
use exchange::ExchangeClient;
use exchange::bybit::transport::ExchangeError;
use rust_decimal_macros::dec;

fn entry(link_id: &str) -> LimitEntry {
    LimitEntry {
        symbol: Symbol::new("BTCUSDT"),
        side: Side::Buy,
        qty: dec!(0.01),
        price: dec!(42000),
        order_link_id: link_id.into(),
        stop_loss: dec!(41000),
        stop_limit_price: dec!(40900),
        take_profit: dec!(44000),
    }
}

#[tokio::test]
async fn a_placed_order_is_recorded_and_acknowledged() {
    let m = MockExchange::new();
    let ack = m.place_limit_entry(entry("abc")).await.expect("placed");
    assert_eq!(ack.order_link_id, "abc");
    assert_eq!(m.placed_orders().len(), 1);
    assert_eq!(m.placed_orders()[0].order_link_id, "abc");
}

#[tokio::test]
async fn balance_and_positions_are_returned_as_configured() {
    let m = MockExchange::new().with_balance(Balance {
        equity: dec!(5000),
        available: dec!(4500),
    });
    let b = m.balance().await.expect("balance");
    assert_eq!(b.equity, dec!(5000));
    assert!(m.positions().await.expect("positions").is_empty());
}

#[tokio::test]
async fn klines_are_returned_for_a_configured_symbol_and_empty_otherwise() {
    let m = MockExchange::new().with_klines(
        &Symbol::new("BTCUSDT"),
        Timeframe::H1,
        vec![
            engine::mock::test_candle(0),
            engine::mock::test_candle(3_600_000),
        ],
    );
    let got = m
        .klines(&Symbol::new("BTCUSDT"), Timeframe::H1, 10)
        .await
        .expect("klines");
    assert_eq!(got.len(), 2);

    let none = m
        .klines(&Symbol::new("ETHUSDT"), Timeframe::H1, 10)
        .await
        .expect("klines");
    assert!(none.is_empty());
}

#[tokio::test]
async fn a_one_shot_failure_fires_once_then_succeeds() {
    // This is what lets the execution layer be tested against a timeout that
    // is retried — the retry must reuse the same orderLinkId and must not
    // create a second order.
    let m = MockExchange::new().fail_place_entry_once(ExchangeError::Decode("injected".into()));

    let first = m.place_limit_entry(entry("abc")).await;
    assert!(first.is_err(), "the first call should have failed");

    let second = m.place_limit_entry(entry("abc")).await;
    assert!(second.is_ok(), "the second call should have succeeded");

    assert_eq!(m.place_entry_call_count(), 2);
    assert_eq!(
        m.placed_orders().len(),
        1,
        "a failed placement must not be recorded as placed"
    );
}

#[tokio::test]
async fn an_always_failure_keeps_failing() {
    let m = MockExchange::new().fail_place_entry_always(ExchangeError::Decode("injected".into()));
    assert!(m.place_limit_entry(entry("a")).await.is_err());
    assert!(m.place_limit_entry(entry("b")).await.is_err());
    assert!(m.placed_orders().is_empty());
}

#[tokio::test]
async fn cancellations_and_stop_amendments_are_recorded() {
    let m = MockExchange::new();
    m.cancel_order(&Symbol::new("BTCUSDT"), "abc")
        .await
        .expect("cancelled");
    m.amend_stop(&Symbol::new("BTCUSDT"), dec!(41000), dec!(40900))
        .await
        .expect("amended");

    assert_eq!(m.cancelled(), vec!["abc".to_string()]);
    let amends = m.amended_stops();
    assert_eq!(amends.len(), 1);
    assert_eq!(amends[0].1, dec!(41000));
    assert_eq!(amends[0].2, dec!(40900));
}

#[tokio::test]
async fn the_mock_is_shareable_across_tasks() {
    // The executor will hold this behind an Arc; interior mutability must let
    // a shared reference still record calls.
    use std::sync::Arc;
    let m = Arc::new(MockExchange::new());
    let m2 = Arc::clone(&m);
    tokio::spawn(async move {
        m2.place_limit_entry(entry("spawned"))
            .await
            .expect("placed");
    })
    .await
    .expect("task joined");
    assert_eq!(m.placed_orders().len(), 1);
}
