use std::collections::{HashMap, HashSet};

use botcore::{OpenOrder, OrderState, Side, Symbol, Timeframe};
use rust_decimal::Decimal;

/// An entry order resting on the book, waiting to fill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestingOrder {
    pub link_id: String,
    pub symbol: Symbol,
    pub side: Side,
    pub qty: Decimal,
    /// How much has filled so far. Non-zero means a partial fill.
    pub cum_exec_qty: Decimal,
    /// Open time of the candle on which the order was placed.
    pub placed_at_candle_ms: i64,
}

/// Something the caller must do about a resting order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackerAction {
    /// Cancel the remainder. `filled` is what already executed and must be
    /// KEPT as a position — cancelling the whole order would abandon a real
    /// open trade.
    Expire {
        link_id: String,
        symbol: Symbol,
        filled: Decimal,
    },
}

/// Tracks entry orders from placement to fill, cancellation or expiry.
///
/// Exists because limit orders do not fill immediately. An entry resting past
/// its window is a setup that no longer applies — the price never came back —
/// so it is withdrawn rather than left to fill on a stale signal.
#[derive(Debug)]
pub struct OrderTracker {
    expiry_candles: u32,
    resting: HashMap<String, RestingOrder>,
}

impl OrderTracker {
    pub fn new(expiry_candles: u32) -> Self {
        OrderTracker {
            expiry_candles,
            resting: HashMap::new(),
        }
    }

    pub fn track(&mut self, order: RestingOrder) {
        self.resting.insert(order.link_id.clone(), order);
    }

    pub fn is_resting(&self, link_id: &str) -> bool {
        self.resting.contains_key(link_id)
    }

    /// Symbols with a resting order.
    ///
    /// Feeds the universe's protection set: dropping a symbol with a live
    /// order would stop its candles arriving and leave the order unmanaged.
    pub fn resting_symbols(&self) -> HashSet<Symbol> {
        self.resting.values().map(|o| o.symbol.clone()).collect()
    }

    /// Apply an order update from the private feed or from reconciliation.
    ///
    /// Updates for orders this process never placed are ignored rather than
    /// adopted — the reconciler owns that decision, not the tracker.
    pub fn on_order_update(&mut self, update: &OpenOrder) {
        let Some(order) = self.resting.get_mut(&update.order_link_id) else {
            return;
        };
        order.cum_exec_qty = update.cum_exec_qty;
        match update.state {
            // Terminal: the order is no longer on the book.
            OrderState::Filled | OrderState::Cancelled | OrderState::Rejected => {
                self.resting.remove(&update.order_link_id);
            }
            OrderState::New | OrderState::PartiallyFilled => {}
        }
    }

    /// Advance the expiry clock for one symbol's stream.
    ///
    /// Returns an action for every order whose window has elapsed. Expired
    /// orders stop being tracked immediately, so the same expiry cannot fire
    /// twice.
    pub fn on_candle_close(
        &mut self,
        symbol: &Symbol,
        candle_open_ms: i64,
        tf: Timeframe,
    ) -> Vec<TrackerAction> {
        let window = tf.duration_ms() * i64::from(self.expiry_candles);

        let expired: Vec<String> = self
            .resting
            .values()
            .filter(|o| {
                o.symbol.as_str() == symbol.as_str()
                    && candle_open_ms - o.placed_at_candle_ms >= window
            })
            .map(|o| o.link_id.clone())
            .collect();

        expired
            .into_iter()
            .filter_map(|link_id| {
                self.resting
                    .remove(&link_id)
                    .map(|o| TrackerAction::Expire {
                        link_id,
                        symbol: o.symbol,
                        filled: o.cum_exec_qty,
                    })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use botcore::{OpenOrder, OrderState, Side, Symbol, Timeframe};
    use rust_decimal::Decimal;
    use rust_decimal_macros::dec;

    const H1: i64 = 3_600_000;

    fn btc() -> Symbol {
        Symbol::new("BTCUSDT")
    }

    fn resting(link_id: &str, placed_at: i64) -> RestingOrder {
        RestingOrder {
            link_id: link_id.into(),
            symbol: btc(),
            side: Side::Buy,
            qty: dec!(1),
            cum_exec_qty: dec!(0),
            placed_at_candle_ms: placed_at,
        }
    }

    fn update(link_id: &str, state: OrderState, cum: Decimal) -> OpenOrder {
        OpenOrder {
            symbol: btc(),
            order_id: format!("oid-{link_id}"),
            order_link_id: link_id.into(),
            side: Side::Buy,
            price: dec!(100),
            qty: dec!(1),
            cum_exec_qty: cum,
            state,
            created_time_ms: 0,
        }
    }

    #[test]
    fn an_order_is_resting_once_tracked() {
        let mut t = OrderTracker::new(3);
        t.track(resting("a", 0));
        assert!(t.is_resting("a"));
        assert!(!t.is_resting("b"));
    }

    #[test]
    fn an_order_does_not_expire_before_its_window_elapses() {
        // Placed on the candle opening at 0; with a 3-candle window it must
        // survive candles 1 and 2 and expire on candle 3.
        let mut t = OrderTracker::new(3);
        t.track(resting("a", 0));
        assert!(t.on_candle_close(&btc(), H1, Timeframe::H1).is_empty());
        assert!(t.on_candle_close(&btc(), 2 * H1, Timeframe::H1).is_empty());
        assert!(t.is_resting("a"));
    }

    #[test]
    fn an_order_expires_once_its_window_elapses() {
        let mut t = OrderTracker::new(3);
        t.track(resting("a", 0));
        let actions = t.on_candle_close(&btc(), 3 * H1, Timeframe::H1);
        assert_eq!(
            actions,
            vec![TrackerAction::Expire {
                link_id: "a".into(),
                symbol: btc(),
                filled: dec!(0),
            }]
        );
        assert!(
            !t.is_resting("a"),
            "an expired order must stop being tracked"
        );
    }

    #[test]
    fn expiry_reports_a_partial_fill_so_the_caller_keeps_the_filled_portion() {
        // A partially-filled entry that expires leaves a real position. The
        // caller must cancel only the remainder, never the whole thing.
        let mut t = OrderTracker::new(3);
        t.track(resting("a", 0));
        t.on_order_update(&update("a", OrderState::PartiallyFilled, dec!(0.4)));

        let actions = t.on_candle_close(&btc(), 3 * H1, Timeframe::H1);
        assert_eq!(
            actions,
            vec![TrackerAction::Expire {
                link_id: "a".into(),
                symbol: btc(),
                filled: dec!(0.4),
            }]
        );
    }

    #[test]
    fn a_fully_filled_order_stops_resting_and_never_expires() {
        let mut t = OrderTracker::new(3);
        t.track(resting("a", 0));
        t.on_order_update(&update("a", OrderState::Filled, dec!(1)));
        assert!(!t.is_resting("a"));
        assert!(t.on_candle_close(&btc(), 99 * H1, Timeframe::H1).is_empty());
    }

    #[test]
    fn a_cancelled_or_rejected_order_stops_resting() {
        let mut t = OrderTracker::new(3);
        t.track(resting("a", 0));
        t.track(resting("b", 0));
        t.on_order_update(&update("a", OrderState::Cancelled, dec!(0)));
        t.on_order_update(&update("b", OrderState::Rejected, dec!(0)));
        assert!(!t.is_resting("a"));
        assert!(!t.is_resting("b"));
    }

    #[test]
    fn a_candle_for_another_symbol_does_not_expire_this_one() {
        let mut t = OrderTracker::new(3);
        t.track(resting("a", 0));
        let actions = t.on_candle_close(&Symbol::new("ETHUSDT"), 99 * H1, Timeframe::H1);
        assert!(actions.is_empty());
        assert!(t.is_resting("a"));
    }

    #[test]
    fn resting_symbols_feeds_the_universe_protection_set() {
        // A symbol with a resting order must never be dropped from the
        // universe, or its candles stop arriving mid-order.
        let mut t = OrderTracker::new(3);
        t.track(resting("a", 0));
        let syms = t.resting_symbols();
        assert!(syms.contains(&btc()));
        assert_eq!(syms.len(), 1);
    }

    #[test]
    fn an_update_for_an_untracked_order_is_ignored() {
        // Reconciliation may surface orders this process never placed.
        let mut t = OrderTracker::new(3);
        t.on_order_update(&update("ghost", OrderState::Filled, dec!(1)));
        assert!(!t.is_resting("ghost"));
    }
}
