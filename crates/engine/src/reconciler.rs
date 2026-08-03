use botcore::Symbol;
use exchange::ExchangeClient;
use exchange::bybit::transport::ExchangeError;
use tracing::{info, warn};

use crate::order_tracker::{OrderTracker, RestingOrder};

/// What reconciliation found and did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// Positions the exchange holds. After a restart the process knows of
    /// none, so all of them are adopted.
    pub adopted_positions: Vec<Symbol>,
    /// Resting orders still inside their window, taken back under management.
    pub adopted_orders: Vec<String>,
    /// Resting orders past their window, cancelled rather than adopted.
    pub cancelled_stale: Vec<String>,
    /// Positions the caller must verify carry a stop and target.
    pub unprotected: Vec<Symbol>,
}

/// Rebuild in-process state from the exchange.
///
/// The exchange is the source of truth in every disagreement — a restart, a
/// crash, or a manual intervention can all leave the process believing
/// something untrue. Running this before any strategy evaluation is what makes
/// automatic restarts safe.
///
/// Idempotent: running it twice produces the same state, so a crash partway
/// through is safe to retry.
pub async fn reconcile(
    client: &dyn ExchangeClient,
    tracker: &mut OrderTracker,
    now_ms: i64,
    expiry_window_ms: i64,
) -> Result<ReconcileReport, ExchangeError> {
    let mut report = ReconcileReport::default();

    for position in client.positions().await? {
        info!(symbol = %position.symbol, size = %position.size, "adopting position from exchange");
        report.adopted_positions.push(position.symbol.clone());
        // The caller verifies protection; the reconciler only reports.
        report.unprotected.push(position.symbol);
    }

    for order in client.open_orders().await? {
        let age = now_ms - order.created_time_ms;
        if age >= expiry_window_ms {
            // A resting order older than its window is a stale setup —
            // adopting it would let it fill on a signal that no longer applies.
            warn!(link_id = %order.order_link_id, age_ms = age, "cancelling stale resting order");
            match client
                .cancel_order(&order.symbol, &order.order_link_id)
                .await
            {
                Ok(()) => report.cancelled_stale.push(order.order_link_id),
                Err(e) => {
                    // Leave it unadopted. It will be seen again on the next
                    // reconcile rather than silently managed as if fresh.
                    warn!(link_id = %order.order_link_id, error = %e, "cancelling a stale order failed");
                }
            }
            continue;
        }

        tracker.track(RestingOrder {
            link_id: order.order_link_id.clone(),
            symbol: order.symbol.clone(),
            side: order.side,
            qty: order.qty,
            cum_exec_qty: order.cum_exec_qty,
            placed_at_candle_ms: order.created_time_ms,
        });
        report.adopted_orders.push(order.order_link_id);
    }

    Ok(report)
}
