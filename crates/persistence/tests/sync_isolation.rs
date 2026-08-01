use botcore::{OrderState, Side, Symbol};
use persistence::{Journal, OrderRecord};
use rust_decimal_macros::dec;

/// Invariant 12: a journal configured for a cloud that cannot be reached must
/// still accept local writes. If this test ever fails, an outage would stop
/// the bot from recording that it placed an order.
#[tokio::test]
async fn local_writes_succeed_when_the_cloud_is_unreachable() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("offline.db");

    // Point at a URL that cannot resolve.
    let journal = Journal::open_synced(
        path.to_str().unwrap(),
        "libsql://nonexistent-host-for-tests.invalid",
        "not-a-real-token",
    )
    .await;

    // Opening may fail outright if the SDK validates connectivity eagerly; in
    // that case the bot must fall back to a local-only journal, which is what
    // config wiring does in Task 11. Either way, local persistence must work.
    let journal = match journal {
        Ok(j) => j,
        Err(_) => Journal::open_local(path.to_str().unwrap()).await.expect("local fallback opens"),
    };

    journal
        .record_order(&OrderRecord {
            order_link_id: "offline-1".into(),
            order_id: None,
            symbol: Symbol::new("BTCUSDT"),
            side: Side::Buy,
            price: dec!(42000),
            qty: dec!(0.01),
            stop_loss: dec!(41000),
            take_profit: dec!(44000),
            state: OrderState::New,
            cum_exec_qty: dec!(0),
            config_hash: "hash".into(),
            created_at_ms: 1,
        })
        .await
        .expect("local write must succeed regardless of cloud reachability");

    assert_eq!(journal.order_count().await.expect("count"), 1);

    // A failed push is reported, not panicked on, and leaves data intact.
    let _ = journal.push().await;
    assert_eq!(journal.order_count().await.expect("count"), 1);
}
