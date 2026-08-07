use botcore::{ErrorClass, OpenOrder, OrderState, Position, Side, Symbol};
use engine::OrderTracker;
use engine::mock::MockExchange;
use engine::reconciler::reconcile;
use rust_decimal_macros::dec;

const H1: i64 = 3_600_000;
const NOW: i64 = 1_700_000_000_000;

fn position(sym: &str, liq: Option<rust_decimal::Decimal>) -> Position {
    Position {
        symbol: Symbol::new(sym),
        side: Side::Buy,
        size: dec!(1),
        entry_price: dec!(100),
        liq_price: liq,
        unrealized_pnl: dec!(0),
    }
}

fn open_order(link_id: &str, created_ms: i64) -> OpenOrder {
    OpenOrder {
        symbol: Symbol::new("BTCUSDT"),
        order_id: format!("oid-{link_id}"),
        order_link_id: link_id.into(),
        side: Side::Buy,
        price: dec!(100),
        qty: dec!(1),
        cum_exec_qty: dec!(0),
        state: OrderState::New,
        created_time_ms: created_ms,
        updated_time_ms: created_ms,
    }
}

#[tokio::test]
async fn a_position_the_tracker_never_knew_about_is_adopted() {
    // After a restart the tracker is empty but the exchange still holds
    // positions. The exchange is the source of truth.
    let mock = MockExchange::new().with_positions(vec![position("BTCUSDT", Some(dec!(80)))]);
    let mut tracker = OrderTracker::new(3);

    let report = reconcile(&mock, &mut tracker, NOW, 3 * H1)
        .await
        .expect("reconciled");

    assert_eq!(report.adopted_positions.len(), 1);
    assert_eq!(report.adopted_positions[0].as_str(), "BTCUSDT");
}

#[tokio::test]
async fn a_still_valid_resting_order_is_adopted_into_the_tracker() {
    let mock = MockExchange::new().with_open_orders(vec![open_order("live", NOW - H1)]);
    let mut tracker = OrderTracker::new(3);

    let report = reconcile(&mock, &mut tracker, NOW, 3 * H1)
        .await
        .expect("reconciled");

    assert_eq!(report.adopted_orders, vec!["live".to_string()]);
    assert!(
        tracker.is_resting("live"),
        "an adopted order must be tracked"
    );
    assert!(report.cancelled_stale.is_empty());
}

#[tokio::test]
async fn an_order_past_its_expiry_window_is_cancelled_not_adopted() {
    // A resting order older than its window is a stale setup. Adopting it
    // would let it fill on a signal that no longer applies.
    let mock = MockExchange::new().with_open_orders(vec![open_order("stale", NOW - 10 * H1)]);
    let mut tracker = OrderTracker::new(3);

    let report = reconcile(&mock, &mut tracker, NOW, 3 * H1)
        .await
        .expect("reconciled");

    assert_eq!(report.cancelled_stale, vec!["stale".to_string()]);
    assert!(!tracker.is_resting("stale"));
    assert_eq!(mock.cancelled(), vec!["stale".to_string()]);
}

#[tokio::test]
async fn an_order_exactly_at_the_expiry_boundary_is_cancelled() {
    // The boundary is inclusive: at exactly one window old the setup has
    // elapsed, so the order is withdrawn rather than adopted.
    let mock = MockExchange::new().with_open_orders(vec![open_order("edge", NOW - 3 * H1)]);
    let mut tracker = OrderTracker::new(3);

    let report = reconcile(&mock, &mut tracker, NOW, 3 * H1)
        .await
        .expect("reconciled");

    assert_eq!(report.cancelled_stale, vec!["edge".to_string()]);
    assert!(!tracker.is_resting("edge"));
}

#[tokio::test]
async fn a_position_with_no_liquidation_price_is_still_adopted() {
    // liq_price is absent when the exchange sees no liquidation risk; that is
    // not the same as the position being absent.
    let mock = MockExchange::new().with_positions(vec![position("BTCUSDT", None)]);
    let mut tracker = OrderTracker::new(3);

    let report = reconcile(&mock, &mut tracker, NOW, 3 * H1)
        .await
        .expect("reconciled");

    assert_eq!(report.adopted_positions.len(), 1);
    assert_eq!(
        report.unprotected.len(),
        1,
        "the caller must verify protection"
    );
}

#[tokio::test]
async fn an_empty_exchange_reconciles_to_an_empty_report() {
    let mock = MockExchange::new();
    let mut tracker = OrderTracker::new(3);

    let report = reconcile(&mock, &mut tracker, NOW, 3 * H1)
        .await
        .expect("reconciled");

    assert!(report.adopted_positions.is_empty());
    assert!(report.adopted_orders.is_empty());
    assert!(report.cancelled_stale.is_empty());
    assert!(report.unprotected.is_empty());
}

#[tokio::test]
async fn reconciliation_is_idempotent() {
    // A crash mid-reconcile must be safe to retry.
    let mock = MockExchange::new().with_open_orders(vec![open_order("live", NOW - H1)]);
    let mut tracker = OrderTracker::new(3);

    reconcile(&mock, &mut tracker, NOW, 3 * H1)
        .await
        .expect("first");
    let second = reconcile(&mock, &mut tracker, NOW, 3 * H1)
        .await
        .expect("second");

    assert_eq!(second.adopted_orders, vec!["live".to_string()]);
    assert!(tracker.is_resting("live"));
}

#[tokio::test]
async fn a_mix_of_fresh_and_stale_orders_is_split_correctly() {
    let mock = MockExchange::new().with_open_orders(vec![
        open_order("fresh", NOW - H1),
        open_order("stale", NOW - 10 * H1),
    ]);
    let mut tracker = OrderTracker::new(3);

    let report = reconcile(&mock, &mut tracker, NOW, 3 * H1)
        .await
        .expect("reconciled");

    assert_eq!(report.adopted_orders, vec!["fresh".to_string()]);
    assert_eq!(report.cancelled_stale, vec!["stale".to_string()]);
    assert!(tracker.is_resting("fresh"));
    assert!(!tracker.is_resting("stale"));
}

#[tokio::test]
async fn an_order_whose_cancel_fails_is_left_unadopted_and_unreported() {
    // The subtlest correctness point in reconciliation. A stale order we
    // failed to cancel is still live on the exchange, but we do NOT know its
    // fate — so it must not be adopted (which would manage it as if fresh,
    // letting it fill on an expired setup) and must not be reported as
    // cancelled (which would claim something the exchange never confirmed).
    // Leaving it in neither bucket means the next reconcile sees it again.
    let mock = MockExchange::new()
        .with_open_orders(vec![open_order("stubborn", NOW - 10 * H1)])
        .fail_cancel_always(exchange::bybit::transport::ExchangeError::WebSocket(
            "injected cancel failure".into(),
        ));
    let mut tracker = OrderTracker::new(3);

    let report = reconcile(&mock, &mut tracker, NOW, 3 * H1)
        .await
        .expect("reconcile itself must not fail because one cancel did");

    assert!(
        !tracker.is_resting("stubborn"),
        "an order we failed to cancel must not be adopted"
    );
    assert!(
        !report.adopted_orders.contains(&"stubborn".to_string()),
        "an order we failed to cancel must not be reported as adopted"
    );
    assert!(
        !report.cancelled_stale.contains(&"stubborn".to_string()),
        "cancelled_stale must only contain cancellations the exchange confirmed"
    );
    assert!(
        mock.cancelled().is_empty(),
        "a failed cancel must not be recorded as cancelled"
    );
}

#[tokio::test]
async fn a_failed_cancel_does_not_stop_other_orders_being_processed() {
    // One stubborn order must not prevent the rest of reconciliation.
    let mock = MockExchange::new()
        .with_open_orders(vec![
            open_order("stubborn", NOW - 10 * H1),
            open_order("fresh", NOW - H1),
        ])
        .fail_cancel_always(exchange::bybit::transport::ExchangeError::WebSocket(
            "injected".into(),
        ));
    let mut tracker = OrderTracker::new(3);

    let report = reconcile(&mock, &mut tracker, NOW, 3 * H1)
        .await
        .expect("reconciled");

    assert_eq!(report.adopted_orders, vec!["fresh".to_string()]);
    assert!(tracker.is_resting("fresh"));
    assert!(!tracker.is_resting("stubborn"));
}

#[tokio::test]
async fn a_fatal_cancel_failure_halts_reconciliation_instead_of_being_swallowed() {
    // This runs at startup, before any strategy evaluation, so a Fatal cancel
    // failure (revoked key, bad signature) must propagate rather than be
    // logged and skipped like an ordinary cancel failure — otherwise a
    // partial-permission API key (read works, trade revoked) would look
    // healthy through the very first reconcile.
    //
    // 10004 is Bybit's "bad sign" retCode. Assert it actually classifies
    // Fatal so this test cannot silently degrade into exercising the
    // Rejected/Retryable path if the classification table ever drifts.
    let fatal = exchange::bybit::transport::ExchangeError::Api {
        code: 10004,
        msg: "bad sign".into(),
    };
    assert_eq!(fatal.class(), ErrorClass::Fatal);

    let mock = MockExchange::new()
        .with_open_orders(vec![open_order("stubborn", NOW - 10 * H1)])
        .fail_cancel_always(fatal);
    let mut tracker = OrderTracker::new(3);

    let err = reconcile(&mock, &mut tracker, NOW, 3 * H1)
        .await
        .expect_err("a Fatal cancel failure must propagate as Err, not be swallowed");
    assert_eq!(err.class(), ErrorClass::Fatal);

    // Left in neither bucket: reconcile bailed out before recording anything
    // about this order, exactly as an unadopted stale order must be.
    assert!(!tracker.is_resting("stubborn"));
}
