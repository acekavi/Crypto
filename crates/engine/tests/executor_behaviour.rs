use std::sync::Arc;

use botcore::{Side, Symbol};
use engine::executor::Executor;
use engine::mock::MockExchange;
use exchange::bybit::transport::ExchangeError;
use persistence::Journal;
use risk::OrderIntent;
use rust_decimal_macros::dec;

async fn journal() -> (Arc<Journal>, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("exec.db");
    let j = Journal::open_local(path.to_str().unwrap())
        .await
        .expect("journal opens");
    (Arc::new(j), dir)
}

fn intent() -> OrderIntent {
    OrderIntent {
        symbol: Symbol::new("BTCUSDT"),
        side: Side::Buy,
        qty: dec!(0.01),
        entry_price: dec!(42000),
        stop_price: dec!(41000),
        stop_limit_price: dec!(40900),
        target_price: dec!(44000),
        atr: dec!(200),
        signal_candle_open_ms: 1_700_000_000_000,
        breakeven_at_r: None,
    }
}

#[tokio::test]
async fn a_placed_entry_carries_stop_and_target_in_the_same_request() {
    // No window may exist where a position is open without protection.
    let (j, _d) = journal().await;
    let mock = Arc::new(MockExchange::new());
    let ex = Executor::new(mock.clone(), j, "hash".into());

    ex.place_entry(&intent()).await.expect("placed");

    let placed = mock.placed_orders();
    assert_eq!(placed.len(), 1);
    assert_eq!(placed[0].stop_loss, dec!(41000));
    assert_eq!(placed[0].stop_limit_price, dec!(40900));
    assert_eq!(placed[0].take_profit, dec!(44000));
    assert_eq!(placed[0].qty, dec!(0.01));
    assert_eq!(placed[0].price, dec!(42000));
}

#[tokio::test]
async fn the_same_intent_always_produces_the_same_link_id() {
    // A retry must reuse the id so the exchange deduplicates it.
    let (j, _d) = journal().await;
    let mock = Arc::new(MockExchange::new());
    let ex = Executor::new(mock.clone(), j, "hash".into());

    let a = ex.place_entry(&intent()).await.expect("placed");
    let b = ex.place_entry(&intent()).await.expect("placed again");
    assert_eq!(a.link_id, b.link_id);
}

#[tokio::test]
async fn a_placed_entry_is_journalled_with_the_config_hash() {
    // Every order must be attributable to an exact ruleset.
    let (j, _d) = journal().await;
    let mock = Arc::new(MockExchange::new());
    let ex = Executor::new(mock.clone(), j.clone(), "cfg-abc".into());

    let resting = ex.place_entry(&intent()).await.expect("placed");
    let row = j
        .order_by_link_id(&resting.link_id)
        .await
        .expect("query ok")
        .expect("row exists");
    assert_eq!(row.config_hash, "cfg-abc");
    assert_eq!(row.symbol.as_str(), "BTCUSDT");
    assert_eq!(row.qty, dec!(0.01));
}

#[tokio::test]
async fn a_rejected_placement_returns_the_error_and_records_no_resting_order() {
    let (j, _d) = journal().await;
    let mock = Arc::new(
        MockExchange::new().fail_place_entry_always(ExchangeError::Decode("rejected".into())),
    );
    let ex = Executor::new(mock.clone(), j, "hash".into());

    let err = ex.place_entry(&intent()).await;
    assert!(err.is_err(), "a rejected placement must surface the error");
    assert!(mock.placed_orders().is_empty());
}

#[tokio::test]
async fn a_retry_after_a_transient_failure_reuses_the_id_and_places_once() {
    // This is the property that stops a timed-out request becoming two
    // positions. The mock fails once, the caller retries, and exactly one
    // order reaches the exchange — under the same id.
    let (j, _d) = journal().await;
    let mock = Arc::new(
        MockExchange::new().fail_place_entry_once(ExchangeError::WebSocket("timeout".into())),
    );
    let ex = Executor::new(mock.clone(), j, "hash".into());

    let first = ex.place_entry(&intent()).await;
    assert!(first.is_err());

    let second = ex.place_entry(&intent()).await.expect("retry succeeds");

    assert_eq!(
        mock.place_entry_call_count(),
        2,
        "both attempts should reach the client"
    );
    assert_eq!(mock.placed_orders().len(), 1, "only one order was accepted");
    assert_eq!(mock.placed_orders()[0].order_link_id, second.link_id);
}

#[tokio::test]
async fn cancel_and_widen_reach_the_exchange() {
    let (j, _d) = journal().await;
    let mock = Arc::new(MockExchange::new());
    let ex = Executor::new(mock.clone(), j, "hash".into());

    ex.cancel(&Symbol::new("BTCUSDT"), "abc")
        .await
        .expect("cancelled");
    ex.widen_stop(&Symbol::new("BTCUSDT"), dec!(41000), dec!(40500))
        .await
        .expect("widened");

    assert_eq!(mock.cancelled(), vec!["abc".to_string()]);
    let amends = mock.amended_stops();
    assert_eq!(amends.len(), 1);
    assert_eq!(amends[0].2, dec!(40500));
}
