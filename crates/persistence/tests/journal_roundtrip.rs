use botcore::{OrderState, Side, Symbol};
use persistence::{Journal, OrderRecord};
use rust_decimal_macros::dec;

async fn temp_journal() -> (Journal, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("test.db");
    let j = Journal::open_local(path.to_str().unwrap())
        .await
        .expect("journal opens");
    (j, dir)
}

fn order(link_id: &str, day_ms: i64) -> OrderRecord {
    OrderRecord {
        order_link_id: link_id.into(),
        order_id: None,
        symbol: Symbol::new("BTCUSDT"),
        side: Side::Buy,
        price: dec!(42000.5),
        qty: dec!(0.01),
        stop_loss: dec!(41000),
        take_profit: dec!(44001),
        state: OrderState::New,
        cum_exec_qty: dec!(0),
        config_hash: "deadbeef".into(),
        created_at_ms: day_ms,
    }
}

#[tokio::test]
async fn order_round_trips_through_the_journal() {
    let (j, _dir) = temp_journal().await;
    j.record_order(&order("link-1", 1_700_000_000_000))
        .await
        .expect("recorded");

    let fetched = j
        .order_by_link_id("link-1")
        .await
        .expect("query ok")
        .expect("row exists");
    assert_eq!(fetched.symbol.as_str(), "BTCUSDT");
    assert_eq!(fetched.price, dec!(42000.5));
    assert_eq!(fetched.state, OrderState::New);
    assert_eq!(fetched.config_hash, "deadbeef");
}

#[tokio::test]
async fn recording_the_same_link_id_twice_does_not_duplicate() {
    // orderLinkId is the idempotency key; a retry must not create a second row.
    let (j, _dir) = temp_journal().await;
    j.record_order(&order("link-1", 1))
        .await
        .expect("first insert");
    j.record_order(&order("link-1", 1))
        .await
        .expect("second insert is a no-op");
    assert_eq!(j.order_count().await.expect("count"), 1);
}

#[tokio::test]
async fn order_state_updates_in_place() {
    let (j, _dir) = temp_journal().await;
    j.record_order(&order("link-1", 1)).await.expect("recorded");
    j.update_order_state("link-1", OrderState::Filled, dec!(0.01), Some(2))
        .await
        .expect("updated");

    let fetched = j
        .order_by_link_id("link-1")
        .await
        .expect("query ok")
        .expect("row exists");
    assert_eq!(fetched.state, OrderState::Filled);
    assert_eq!(fetched.cum_exec_qty, dec!(0.01));
}

#[tokio::test]
async fn daily_fill_count_only_counts_filled_orders_in_that_utc_day() {
    let (j, _dir) = temp_journal().await;
    let day = 1_700_000_000_000i64 / 86_400_000 * 86_400_000;

    // Two filled, one still resting, one on the following day. Placement time
    // (created_at_ms) and fill time (filled_at_ms) are the same here on
    // purpose, to isolate this test from the placement-vs-fill distinction
    // covered separately below.
    for (id, state, ts) in [
        ("a", OrderState::Filled, day + 1_000),
        ("b", OrderState::Filled, day + 2_000),
        ("c", OrderState::New, day + 3_000),
        ("d", OrderState::Filled, day + 86_400_000),
    ] {
        j.record_order(&order(id, ts)).await.expect("recorded");
        if state == OrderState::Filled {
            j.update_order_state(id, state, dec!(0.01), Some(ts))
                .await
                .expect("filled");
        }
    }

    assert_eq!(j.daily_fill_count(day).await.expect("count"), 2);
}

#[tokio::test]
async fn daily_fill_count_uses_fill_time_not_placement_time() {
    // A PostOnly limit entry can rest across the UTC day boundary before it
    // fills. The daily budget must charge the day the fill actually
    // happened, not the day the order was placed — otherwise an order
    // placed late on day N and filled early on day N+1 would wrongly eat
    // into day N's budget (or wrongly free up day N+1's).
    let (j, _dir) = temp_journal().await;
    let day_n = 1_700_000_000_000i64 / 86_400_000 * 86_400_000;
    let day_n_plus_1 = day_n + 86_400_000;

    let placed_at = day_n_plus_1 - 1_000; // 23:59:59.000 on day N
    let filled_at = day_n_plus_1 + 1_000; // 00:00:01.000 on day N+1

    j.record_order(&order("cross-midnight", placed_at))
        .await
        .expect("recorded");
    j.update_order_state(
        "cross-midnight",
        OrderState::Filled,
        dec!(0.01),
        Some(filled_at),
    )
    .await
    .expect("filled");

    assert_eq!(
        j.daily_fill_count(day_n)
            .await
            .expect("count for placement day"),
        0
    );
    assert_eq!(
        j.daily_fill_count(day_n_plus_1)
            .await
            .expect("count for fill day"),
        1
    );
}

#[tokio::test]
async fn halt_state_survives_reopening_the_database() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("halt.db");
    let p = path.to_str().unwrap();

    {
        let j = Journal::open_local(p).await.expect("opens");
        j.set_halt("daily drawdown -5%", 1_700_000_000_000)
            .await
            .expect("halt set");
    }

    // A restart must not clear the halt — that is the whole point of persisting it.
    let j = Journal::open_local(p).await.expect("reopens");
    assert_eq!(
        j.halt_reason().await.expect("query"),
        Some("daily drawdown -5%".to_string())
    );
}

#[tokio::test]
async fn clearing_the_halt_requires_an_explicit_call() {
    let (j, _dir) = temp_journal().await;
    j.set_halt("test", 1).await.expect("set");
    j.clear_halt().await.expect("cleared");
    assert_eq!(j.halt_reason().await.expect("query"), None);
}
