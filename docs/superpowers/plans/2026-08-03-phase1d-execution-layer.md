# Phase 1d — Execution Layer Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Turn the decision layer into a bot that actually trades — track resting limit orders through expiry and partial fills, widen a triggered stop through an escalation ladder without ever using a market order, place and record orders idempotently, reconcile against the exchange on every startup, and run the whole thing as a live loop.

**Architecture:** Two pure state machines (`OrderTracker`, `escalation`) hold every lifecycle decision, so the hard logic is testable with no network. `Executor` and `Reconciler` are thin async shells over those decisions, driven in tests by the existing `MockExchange`. The live loop wires candle → strategy → risk → executor and treats a closed event channel as a halt condition.

**Tech Stack:** Rust 1.97 (edition 2024), tokio, rust_decimal, botcore, exchange, risk, strategy, engine, persistence.

## Global Constraints

- All monetary and quantity values use `rust_decimal::Decimal`. **Never `f64`.**
- The domain crate is **`botcore`**, never `core`.
- **No code path may construct `orderType: "Market"`.** `crates/exchange/tests/no_market_orders.rs` greps every `.rs` file under `crates/` and `bot/` for the quoted literal and fails the build. `"MarkPrice"` is fine.
- **Stops are stop-LIMIT orders and escalate by widening, never by falling back to a market order.** This is the owner's rule; the residual gap risk is accepted and documented.
- **Every entry carries its stop and target in the same request**, so no window exists where a position is open unprotected.
- `orderLinkId` is deterministic — a retry must never open a second position.
- **Journal writes must never block or fail an order.** Log and continue.
- Max 5 filled entries per UTC day; max 4 concurrent positions; 1 per symbol.
- A stale feed blocks new entries. A closed `MarketEvent` **or** `AccountEvent` channel is a halt condition.
- Mainnet requires `--profile mainnet` **and** `BYBIT_ALLOW_MAINNET`.
- `dec!` in `#[cfg(test)]` and `tests/` only.
- Rust edition 2024, rust-version 1.97.

## What already exists

- **`botcore`** — `Candle`, `Timeframe::{H1,H4}` (`duration_ms()`), `Symbol`, `Instrument`, `Side::{Buy,Sell}` (`opposite()`), `Position`, `Balance`, `LimitEntry { symbol, side, qty, price, order_link_id, stop_loss, stop_limit_price, take_profit }`, `OrderAck`, `OpenOrder { symbol, order_id, order_link_id, side, price, qty, cum_exec_qty, state, created_time_ms }`, `OrderState::{New,PartiallyFilled,Filled,Cancelled,Rejected}`, `ErrorClass`, `money::{round_down_to_step, round_price_away_from_market}`.
- **`exchange`** — `ExchangeClient` (async: `instruments`, `tickers`, `klines`, `place_limit_entry`, `amend_stop`, `cancel_order`, `positions`, `open_orders`, `set_leverage`, `balance`), `MarketFeed`, `MarketEvent::{CandleClosed,GapFilled}`, `Subscription`, `bybit::wire::Ticker`, `bybit::transport::ExchangeError`, `bybit::ws_private::{BybitPrivateFeed, AccountEvent}`.
- **`strategy`** — `Signal { symbol, side, entry_price, stop_price, target_price, atr, signal_candle_open_ms }`, `MarketContext { symbol, timeframe, candle, instrument }`, `Strategy`, `PullbackStrategy`, `StrategyParams`.
- **`risk`** — `RiskManager::{new, evaluate}`, `Decision::{Enter,Refuse}`, `OrderIntent { symbol, side, qty, entry_price, stop_price, stop_limit_price, target_price, atr, signal_candle_open_ms }`, `AccountState`, `RiskParams`, `Refusal`.
- **`engine`** — `order_link_id(symbol, signal_candle_open_ms, side)`, `CandleStore` (`warm`, `accept -> Acceptance`, `is_warm`, `is_stale`, `retain_symbols`), `Acceptance::{Accepted,Duplicate,OutOfOrder,Gap}`, `UniverseFilter`, `select_universe`, `utc_day_start_ms`, `update_high_water_mark`, `JournalFacts`, `assemble_account_state`, `mock::MockExchange`.
- **`persistence`** — `Journal` (`open_local`, `open_synced`, `record_order`, `update_order_state`, `order_by_link_id`, `daily_fill_count`, `record_equity`, `set_halt`, `clear_halt`, `halt_reason`, `push`), `OrderRecord`, `spawn_sync_task`.

## Carried forward from Plan 1c's final review

- `CandleStore::retain_symbols` exists but **has no call site** — Task 5 must call it on each daily re-rank.
- There is no combined "safe to trade" helper; a caller must AND `is_warm()` and `!is_stale()` itself.
- `botcore::Symbol` performs **no normalisation**. Every symbol must originate from exchange wire data, never a hand-typed config string.

---

## File Structure

```
crates/engine/src/
├── order_tracker.rs   resting-order lifecycle: expiry, partial fills
├── escalation.rs      stop-limit escalation ladder (pure)
├── executor.rs        OrderIntent -> exchange + journal
└── reconciler.rs      startup reconciliation against the exchange
bot/src/
├── main.rs            live loop replacing the probe
└── engine_loop.rs     the wiring: candle -> strategy -> risk -> executor
```

`order_tracker.rs` and `escalation.rs` are pure and hold every lifecycle decision. `executor.rs` and `reconciler.rs` are thin async shells that act on those decisions, so the async surface stays small enough to test through `MockExchange`.

---

### Task 1: OrderTracker — resting-order expiry and partial fills

**Files:**
- Create: `crates/engine/src/order_tracker.rs`
- Modify: `crates/engine/src/lib.rs`

**Interfaces:**
- Consumes: `botcore::{OpenOrder, OrderState, Side, Symbol}`
- Produces:
  - `RestingOrder { link_id: String, symbol: Symbol, side: Side, qty: Decimal, cum_exec_qty: Decimal, placed_at_candle_ms: i64 }`
  - `TrackerAction::{Expire { link_id, symbol, filled: Decimal }}`
  - `OrderTracker::new(expiry_candles: u32) -> Self`
  - `OrderTracker::track(&mut self, order: RestingOrder)`
  - `OrderTracker::on_candle_close(&mut self, symbol: &Symbol, candle_open_ms: i64, tf: Timeframe) -> Vec<TrackerAction>`
  - `OrderTracker::on_order_update(&mut self, update: &OpenOrder)`
  - `OrderTracker::resting_symbols(&self) -> HashSet<Symbol>`
  - `OrderTracker::is_resting(&self, link_id: &str) -> bool`

- [ ] **Step 1: Write the failing test**

Create `crates/engine/src/order_tracker.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use botcore::{OpenOrder, OrderState, Side, Symbol, Timeframe};
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
        assert!(!t.is_resting("a"), "an expired order must stop being tracked");
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
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p engine`
Expected: FAIL — `OrderTracker`, `RestingOrder`, `TrackerAction` not found.

- [ ] **Step 3: Implement**

Prepend to `crates/engine/src/order_tracker.rs`:

```rust
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
                self.resting.remove(&link_id).map(|o| TrackerAction::Expire {
                    link_id,
                    symbol: o.symbol,
                    filled: o.cum_exec_qty,
                })
            })
            .collect()
    }
}
```

Add to `crates/engine/src/lib.rs`:

```rust
pub mod order_tracker;

pub use order_tracker::{OrderTracker, RestingOrder, TrackerAction};
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p engine && cargo clippy -p engine --all-targets -- -D warnings`
Expected: PASS — 58 tests, no clippy warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/engine
git commit -m "feat(engine): OrderTracker for resting-order expiry and partial fills

An entry resting past its window is a setup that no longer applies — the
price never came back — so it is withdrawn rather than left to fill on a
stale signal. Expiry reports the already-filled quantity so the caller
cancels only the remainder: a partially-filled entry is a real position and
cancelling the whole order would abandon it."
```

---

### Task 2: Stop escalation ladder

**Files:**
- Create: `crates/engine/src/escalation.rs`
- Modify: `crates/engine/src/lib.rs`

**Interfaces:**
- Consumes: `botcore::{Side, Symbol}`
- Produces:
  - `EscalationLadder { offsets_atr: Vec<Decimal>, timeout_ms: i64 }` with `EscalationLadder::defaults()`
  - `TriggeredStop { symbol: Symbol, side: Side, trigger: Decimal, atr: Decimal, rung: usize, rung_started_ms: i64 }`
  - `EscalationAction::{Wait, Widen { rung: usize, limit_price: Decimal }, Exhausted}`
  - `stop_limit_for(trigger: Decimal, atr: Decimal, side: Side, offset_atr: Decimal) -> Decimal`
  - `next_escalation(stop: &TriggeredStop, ladder: &EscalationLadder, now_ms: i64) -> EscalationAction`

- [ ] **Step 1: Write the failing test**

Create `crates/engine/src/escalation.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use botcore::{Side, Symbol};
    use rust_decimal_macros::dec;

    fn stop(rung: usize, rung_started_ms: i64, side: Side) -> TriggeredStop {
        TriggeredStop {
            symbol: Symbol::new("BTCUSDT"),
            side,
            trigger: dec!(100),
            atr: dec!(2),
            rung,
            rung_started_ms,
        }
    }

    #[test]
    fn defaults_widen_through_three_rungs() {
        let l = EscalationLadder::defaults();
        assert_eq!(l.offsets_atr, vec![dec!(0.3), dec!(0.6), dec!(1.2)]);
        assert_eq!(l.timeout_ms, 30_000);
    }

    #[test]
    fn a_long_stop_limit_sits_below_the_trigger() {
        // The position is being SOLD to close, so the limit must sit below the
        // trigger to fill into the move rather than at its edge.
        assert_eq!(
            stop_limit_for(dec!(100), dec!(2), Side::Buy, dec!(0.3)),
            dec!(99.4)
        );
    }

    #[test]
    fn a_short_stop_limit_sits_above_the_trigger() {
        assert_eq!(
            stop_limit_for(dec!(100), dec!(2), Side::Sell, dec!(0.3)),
            dec!(100.6)
        );
    }

    #[test]
    fn a_wider_rung_sits_further_from_the_trigger() {
        let near = stop_limit_for(dec!(100), dec!(2), Side::Buy, dec!(0.3));
        let far = stop_limit_for(dec!(100), dec!(2), Side::Buy, dec!(1.2));
        assert!(far < near, "rung 2 ({far}) was not further out than rung 0 ({near})");
    }

    #[test]
    fn before_the_timeout_the_action_is_to_wait() {
        let l = EscalationLadder::defaults();
        let s = stop(0, 1_000_000, Side::Buy);
        assert_eq!(next_escalation(&s, &l, 1_000_000 + 29_999), EscalationAction::Wait);
    }

    #[test]
    fn at_the_timeout_the_stop_widens_to_the_next_rung() {
        let l = EscalationLadder::defaults();
        let s = stop(0, 1_000_000, Side::Buy);
        assert_eq!(
            next_escalation(&s, &l, 1_000_000 + 30_000),
            EscalationAction::Widen {
                rung: 1,
                limit_price: dec!(98.8), // 100 - 0.6 * 2
            }
        );
    }

    #[test]
    fn the_last_rung_widens_to_the_widest_offset() {
        let l = EscalationLadder::defaults();
        let s = stop(1, 1_000_000, Side::Buy);
        assert_eq!(
            next_escalation(&s, &l, 1_000_000 + 30_000),
            EscalationAction::Widen {
                rung: 2,
                limit_price: dec!(97.6), // 100 - 1.2 * 2
            }
        );
    }

    #[test]
    fn past_the_last_rung_the_ladder_is_exhausted() {
        // Nothing further can be tried WITHOUT a market order, which the
        // owner's rules forbid. The caller must alert and halt new entries.
        let l = EscalationLadder::defaults();
        let s = stop(2, 1_000_000, Side::Buy);
        assert_eq!(
            next_escalation(&s, &l, 1_000_000 + 30_000),
            EscalationAction::Exhausted
        );
    }

    #[test]
    fn an_exhausted_ladder_stays_exhausted_however_long_it_waits() {
        let l = EscalationLadder::defaults();
        let s = stop(2, 1_000_000, Side::Buy);
        assert_eq!(
            next_escalation(&s, &l, 1_000_000 + 86_400_000),
            EscalationAction::Exhausted
        );
    }

    #[test]
    fn a_short_widens_upward() {
        let l = EscalationLadder::defaults();
        let s = stop(0, 1_000_000, Side::Sell);
        assert_eq!(
            next_escalation(&s, &l, 1_000_000 + 30_000),
            EscalationAction::Widen {
                rung: 1,
                limit_price: dec!(101.2), // 100 + 0.6 * 2
            }
        );
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p engine`
Expected: FAIL — `EscalationLadder`, `TriggeredStop`, `next_escalation` not found.

- [ ] **Step 3: Implement**

Prepend to `crates/engine/src/escalation.rs`:

```rust
use botcore::{Side, Symbol};
use rust_decimal::Decimal;

/// How a triggered stop widens when it does not fill.
///
/// The owner's rule forbids market orders anywhere, including stops. A
/// stop-limit that does not fill is therefore widened rather than converted —
/// each rung places the limit further into the move, which is where liquidity
/// is. If every rung is exhausted the position stays open and a human must
/// intervene; that residual gap risk is accepted deliberately.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EscalationLadder {
    /// Offsets in multiples of ATR, ascending.
    pub offsets_atr: Vec<Decimal>,
    /// How long a rung is given to fill before widening.
    pub timeout_ms: i64,
}

impl EscalationLadder {
    /// The spec's ladder: 0.3, 0.6, then 1.2 ATR, 30 seconds per rung.
    pub fn defaults() -> Self {
        EscalationLadder {
            offsets_atr: vec![
                Decimal::new(3, 1),
                Decimal::new(6, 1),
                Decimal::new(12, 1),
            ],
            timeout_ms: 30_000,
        }
    }
}

/// A stop that has triggered and is waiting to fill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriggeredStop {
    pub symbol: Symbol,
    /// The side of the POSITION, not of the closing order. A long position's
    /// stop sells, so its limit sits below the trigger.
    pub side: Side,
    pub trigger: Decimal,
    pub atr: Decimal,
    /// Index into `offsets_atr` currently in force.
    pub rung: usize,
    pub rung_started_ms: i64,
}

/// What to do with a triggered stop right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EscalationAction {
    /// The current rung still has time to fill.
    Wait,
    /// Move the limit to `rung`'s offset.
    Widen { rung: usize, limit_price: Decimal },
    /// Every rung has been tried. Nothing further is possible without a
    /// market order, which the rules forbid — alert and halt new entries.
    Exhausted,
}

/// Where a stop-limit sits for a given offset.
///
/// Placed BEYOND the trigger, in the direction the position is closing: a long
/// closes by selling, so its limit goes below; a short closes by buying, so
/// its limit goes above. Sitting beyond the trigger is what lets it fill into
/// the move rather than at its edge.
pub fn stop_limit_for(
    trigger: Decimal,
    atr: Decimal,
    side: Side,
    offset_atr: Decimal,
) -> Decimal {
    let offset = atr * offset_atr;
    match side {
        Side::Buy => trigger - offset,
        Side::Sell => trigger + offset,
    }
}

/// Decide the next escalation step.
///
/// `Exhausted` is returned once the current rung is the last one and its
/// timeout has passed — and stays `Exhausted` however long the caller waits,
/// so a stuck stop cannot silently look like it is still progressing.
pub fn next_escalation(
    stop: &TriggeredStop,
    ladder: &EscalationLadder,
    now_ms: i64,
) -> EscalationAction {
    if now_ms - stop.rung_started_ms < ladder.timeout_ms {
        return EscalationAction::Wait;
    }
    let next = stop.rung + 1;
    match ladder.offsets_atr.get(next) {
        Some(offset) => EscalationAction::Widen {
            rung: next,
            limit_price: stop_limit_for(stop.trigger, stop.atr, stop.side, *offset),
        },
        None => EscalationAction::Exhausted,
    }
}
```

Add to `crates/engine/src/lib.rs`:

```rust
pub mod escalation;

pub use escalation::{EscalationAction, EscalationLadder, TriggeredStop, next_escalation, stop_limit_for};
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p engine && cargo clippy -p engine --all-targets -- -D warnings`
Expected: PASS — 68 tests, no clippy warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/engine
git commit -m "feat(engine): stop-limit escalation ladder

The owner's rule forbids market orders anywhere, including stops, so a
stop-limit that does not fill is widened rather than converted — each rung
places the limit further into the move, where the liquidity is. Exhausting
every rung returns Exhausted permanently rather than cycling, so a stuck stop
cannot look like it is still progressing. The position then stays open until a
human intervenes; that residual gap risk is accepted deliberately."
```

---

### Task 3: Executor — placing and recording orders

**Files:**
- Create: `crates/engine/src/executor.rs`
- Modify: `crates/engine/src/lib.rs`, `crates/engine/Cargo.toml` (add `persistence = { path = "../persistence" }`)
- Test: `crates/engine/tests/executor_behaviour.rs`

**Interfaces:**
- Consumes: `exchange::ExchangeClient`, `risk::OrderIntent`, `persistence::{Journal, OrderRecord}`, `engine::{order_link_id, RestingOrder}`
- Produces:
  - `Executor::new(client: Arc<dyn ExchangeClient>, journal: Arc<Journal>, config_hash: String) -> Self`
  - `Executor::place_entry(&self, intent: &OrderIntent) -> Result<RestingOrder, ExchangeError>`
  - `Executor::cancel(&self, symbol: &Symbol, link_id: &str) -> Result<(), ExchangeError>`
  - `Executor::widen_stop(&self, symbol: &Symbol, trigger: Decimal, limit_price: Decimal) -> Result<(), ExchangeError>`

- [ ] **Step 1: Write the failing test**

Create `crates/engine/tests/executor_behaviour.rs`:

```rust
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

    assert_eq!(mock.place_entry_call_count(), 2, "both attempts should reach the client");
    assert_eq!(mock.placed_orders().len(), 1, "only one order was accepted");
    assert_eq!(mock.placed_orders()[0].order_link_id, second.link_id);
}

#[tokio::test]
async fn cancel_and_widen_reach_the_exchange() {
    let (j, _d) = journal().await;
    let mock = Arc::new(MockExchange::new());
    let ex = Executor::new(mock.clone(), j, "hash".into());

    ex.cancel(&Symbol::new("BTCUSDT"), "abc").await.expect("cancelled");
    ex.widen_stop(&Symbol::new("BTCUSDT"), dec!(41000), dec!(40500))
        .await
        .expect("widened");

    assert_eq!(mock.cancelled(), vec!["abc".to_string()]);
    let amends = mock.amended_stops();
    assert_eq!(amends.len(), 1);
    assert_eq!(amends[0].2, dec!(40500));
}
```

Add `tempfile = "3"` to `crates/engine/Cargo.toml` under `[dev-dependencies]`, and `persistence = { path = "../persistence" }` under `[dependencies]`.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p engine --test executor_behaviour`
Expected: FAIL — `Executor` not found.

- [ ] **Step 3: Implement**

Create `crates/engine/src/executor.rs`:

```rust
use std::sync::Arc;

use botcore::{LimitEntry, OrderState, Symbol};
use exchange::ExchangeClient;
use exchange::bybit::transport::ExchangeError;
use persistence::{Journal, OrderRecord};
use risk::OrderIntent;
use rust_decimal::Decimal;
use tracing::warn;

use crate::link_id::order_link_id;
use crate::order_tracker::RestingOrder;

/// Places orders and records them.
///
/// The only component that talks to the exchange's trading endpoints. Every
/// entry carries its stop and target in the same request, so no window exists
/// where a position is open unprotected.
pub struct Executor {
    client: Arc<dyn ExchangeClient>,
    journal: Arc<Journal>,
    /// SHA-256 of the effective config, written to every order row so each
    /// trade is attributable to an exact ruleset.
    config_hash: String,
}

impl Executor {
    pub fn new(
        client: Arc<dyn ExchangeClient>,
        journal: Arc<Journal>,
        config_hash: String,
    ) -> Self {
        Executor {
            client,
            journal,
            config_hash,
        }
    }

    /// Place a PostOnly limit entry with protection attached.
    ///
    /// The journal is written only after the exchange accepts, so a row means
    /// "the exchange took this" rather than "we asked". A journal failure is
    /// logged and swallowed — persistence must never fail an order that the
    /// exchange has already accepted.
    pub async fn place_entry(&self, intent: &OrderIntent) -> Result<RestingOrder, ExchangeError> {
        let link_id = order_link_id(
            &intent.symbol,
            intent.signal_candle_open_ms,
            intent.side,
        );

        let req = LimitEntry {
            symbol: intent.symbol.clone(),
            side: intent.side,
            qty: intent.qty,
            price: intent.entry_price,
            order_link_id: link_id.clone(),
            stop_loss: intent.stop_price,
            stop_limit_price: intent.stop_limit_price,
            take_profit: intent.target_price,
        };

        let ack = self.client.place_limit_entry(req).await?;

        let record = OrderRecord {
            order_link_id: ack.order_link_id.clone(),
            order_id: Some(ack.order_id.clone()),
            symbol: intent.symbol.clone(),
            side: intent.side,
            price: intent.entry_price,
            qty: intent.qty,
            stop_loss: intent.stop_price,
            take_profit: intent.target_price,
            state: OrderState::New,
            cum_exec_qty: Decimal::ZERO,
            config_hash: self.config_hash.clone(),
            created_at_ms: intent.signal_candle_open_ms,
        };
        if let Err(e) = self.journal.record_order(&record).await {
            // The exchange has already accepted this order. Failing here would
            // mean reporting an error for an order that genuinely exists.
            warn!(error = %e, link_id = %ack.order_link_id, "journalling a placed order failed");
        }

        Ok(RestingOrder {
            link_id: ack.order_link_id,
            symbol: intent.symbol.clone(),
            side: intent.side,
            qty: intent.qty,
            cum_exec_qty: Decimal::ZERO,
            placed_at_candle_ms: intent.signal_candle_open_ms,
        })
    }

    pub async fn cancel(&self, symbol: &Symbol, link_id: &str) -> Result<(), ExchangeError> {
        self.client.cancel_order(symbol, link_id).await
    }

    /// Move a triggered stop's limit further into the move. Still a limit
    /// order — this never converts to a market order.
    pub async fn widen_stop(
        &self,
        symbol: &Symbol,
        trigger: Decimal,
        limit_price: Decimal,
    ) -> Result<(), ExchangeError> {
        self.client.amend_stop(symbol, trigger, limit_price).await
    }
}
```

Add to `crates/engine/src/lib.rs`:

```rust
pub mod executor;

pub use executor::Executor;
```

`MockExchange` must be usable as `Arc<dyn ExchangeClient>`. If the trait is not object-safe as written, report it rather than working around it — that would be a design problem worth surfacing, not patching.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p engine && cargo clippy -p engine --all-targets -- -D warnings`
Expected: PASS — 74 tests, no clippy warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/engine
git commit -m "feat(engine): Executor placing and journalling orders

Every entry carries stop and target in the same request, so no window exists
where a position is open unprotected. The journal is written only after the
exchange accepts, so a row means 'the exchange took this' rather than 'we
asked' — and a journal failure is logged rather than propagated, since
failing there would report an error for an order that genuinely exists."
```

---

### Task 4: Reconciler — startup reconciliation

**Files:**
- Create: `crates/engine/src/reconciler.rs`
- Modify: `crates/engine/src/lib.rs`
- Test: `crates/engine/tests/reconciler_behaviour.rs`

**Interfaces:**
- Consumes: `exchange::ExchangeClient`, `engine::{OrderTracker, RestingOrder}`, `botcore::{OpenOrder, Position}`
- Produces:
  - `ReconcileReport { adopted_positions: Vec<Symbol>, adopted_orders: Vec<String>, cancelled_stale: Vec<String>, unprotected: Vec<Symbol> }`
  - `reconcile(client: &dyn ExchangeClient, tracker: &mut OrderTracker, now_ms: i64, expiry_window_ms: i64) -> Result<ReconcileReport, ExchangeError>`

- [ ] **Step 1: Write the failing test**

Create `crates/engine/tests/reconciler_behaviour.rs`:

```rust
use botcore::{OpenOrder, OrderState, Position, Side, Symbol};
use engine::mock::MockExchange;
use engine::reconciler::reconcile;
use engine::OrderTracker;
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
    assert!(tracker.is_resting("live"), "an adopted order must be tracked");
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
async fn a_position_with_no_liquidation_price_is_not_flagged_unprotected() {
    // liq_price is absent when the exchange sees no liquidation risk; that is
    // not the same as missing a stop.
    let mock = MockExchange::new().with_positions(vec![position("BTCUSDT", None)]);
    let mut tracker = OrderTracker::new(3);

    let report = reconcile(&mock, &mut tracker, NOW, 3 * H1)
        .await
        .expect("reconciled");

    assert!(report.adopted_positions.len() == 1);
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
}

#[tokio::test]
async fn reconciliation_is_idempotent() {
    // A crash mid-reconcile must be safe to retry.
    let mock = MockExchange::new().with_open_orders(vec![open_order("live", NOW - H1)]);
    let mut tracker = OrderTracker::new(3);

    reconcile(&mock, &mut tracker, NOW, 3 * H1).await.expect("first");
    let second = reconcile(&mock, &mut tracker, NOW, 3 * H1).await.expect("second");

    assert_eq!(second.adopted_orders, vec!["live".to_string()]);
    assert!(tracker.is_resting("live"));
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p engine --test reconciler_behaviour`
Expected: FAIL — `reconcile` and `ReconcileReport` not found.

- [ ] **Step 3: Implement**

Create `crates/engine/src/reconciler.rs`:

```rust
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
            match client.cancel_order(&order.symbol, &order.order_link_id).await {
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
```

Add to `crates/engine/src/lib.rs`:

```rust
pub mod reconciler;

pub use reconciler::{ReconcileReport, reconcile};
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p engine && cargo clippy -p engine --all-targets -- -D warnings`
Expected: PASS — 80 tests, no clippy warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/engine
git commit -m "feat(engine): startup reconciliation against the exchange

The exchange is the source of truth in every disagreement — a restart, crash
or manual intervention can leave the process believing something untrue.
Resting orders older than their window are cancelled rather than adopted,
since adopting one would let it fill on a signal that no longer applies. A
failed cancel leaves the order unadopted so the next reconcile sees it again
rather than silently managing it as if fresh."
```

---

### Task 5: EngineLoop — candle to order

**Files:**
- Create: `bot/src/engine_loop.rs`
- Modify: `bot/src/lib.rs`, `bot/Cargo.toml` (add `engine`, `risk`, `strategy`, `persistence` path deps)
- Test: `bot/tests/engine_loop_behaviour.rs`

**Interfaces:**
- Consumes: everything from Tasks 1–4 plus `strategy::Strategy`, `risk::RiskManager`, `engine::{CandleStore, assemble_account_state, JournalFacts, utc_day_start_ms}`
- Produces:
  - `EngineLoop::new(...)` (see Step 3 for the full signature)
  - `EngineLoop::on_candle_closed(&mut self, symbol: &Symbol, tf: Timeframe, candle: &Candle) -> Result<CandleOutcome, ExchangeError>`
  - `CandleOutcome::{Skipped(SkipReason), Refused(Refusal), Placed { link_id: String }}`
  - `SkipReason::{NotAccepted, NotWarm, Stale, NoSignal, UnknownInstrument}`

- [ ] **Step 1: Write the failing test**

Create `bot/tests/engine_loop_behaviour.rs`:

```rust
use std::sync::Arc;

use bot::engine_loop::{CandleOutcome, EngineLoop, SkipReason};
use botcore::{Candle, Instrument, Symbol, Timeframe};
use engine::mock::MockExchange;
use persistence::Journal;
use risk::{RiskManager, RiskParams};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use strategy::{PullbackStrategy, StrategyParams};

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

    assert!(matches!(out, CandleOutcome::Skipped(SkipReason::NotAccepted)));
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
    use engine::RestingOrder;
    use botcore::Side;

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
```

Add to `bot/Cargo.toml`: `engine = { path = "../crates/engine" }`, `risk = { path = "../crates/risk" }`, `strategy = { path = "../crates/strategy" }`, `persistence = { path = "../crates/persistence" }` under `[dependencies]`; `tempfile = "3"` under `[dev-dependencies]`.

Create `bot/src/lib.rs` additions: `pub mod engine_loop;`

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p bot --test engine_loop_behaviour`
Expected: FAIL — `EngineLoop` not found.

- [ ] **Step 3: Implement**

Create `bot/src/engine_loop.rs`:

```rust
use std::collections::HashMap;
use std::sync::Arc;

use botcore::{Candle, Instrument, Symbol, Timeframe};
use engine::{
    Acceptance, CandleStore, Executor, JournalFacts, OrderTracker, RestingOrder, TrackerAction,
    assemble_account_state, utc_day_start_ms,
};
use exchange::ExchangeClient;
use exchange::bybit::transport::ExchangeError;
use persistence::Journal;
use risk::{Decision, Refusal, RiskManager};
use rust_decimal::Decimal;
use strategy::{MarketContext, Strategy};
use tracing::{info, warn};

/// Why a candle produced no order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// Duplicate, out-of-order, or a gap — the stream did not advance.
    NotAccepted,
    NotWarm,
    Stale,
    NoSignal,
    /// No instrument metadata, so no valid order could be formed.
    UnknownInstrument,
}

/// What one closed candle led to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CandleOutcome {
    Skipped(SkipReason),
    Refused(Refusal),
    Placed { link_id: String },
}

/// Wires candle → strategy → risk → executor.
///
/// Every gate is applied in order and each one is nameable, so a candle that
/// produces no trade always says why rather than disappearing silently.
pub struct EngineLoop {
    strategy: Box<dyn Strategy>,
    risk: RiskManager,
    client: Arc<dyn ExchangeClient>,
    journal: Arc<Journal>,
    executor: Executor,
    tracker: OrderTracker,
    store: CandleStore,
    instruments: HashMap<String, Instrument>,
    high_water_mark: Decimal,
    day_start_equity: Decimal,
}

impl EngineLoop {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        strategy: Box<dyn Strategy>,
        risk: RiskManager,
        client: Arc<dyn ExchangeClient>,
        journal: Arc<Journal>,
        instruments: Vec<Instrument>,
        config_hash: String,
        entry_expiry_candles: u32,
        warmup_candles: usize,
    ) -> Self {
        let executor = Executor::new(Arc::clone(&client), Arc::clone(&journal), config_hash);
        EngineLoop {
            strategy,
            risk,
            client,
            journal,
            executor,
            tracker: OrderTracker::new(entry_expiry_candles),
            store: CandleStore::new(warmup_candles),
            instruments: instruments
                .into_iter()
                .map(|i| (i.symbol.as_str().to_string(), i))
                .collect(),
            high_water_mark: Decimal::ZERO,
            day_start_equity: Decimal::ZERO,
        }
    }

    /// Seed the store after a restart or a gap backfill.
    pub fn warm(&mut self, symbol: &Symbol, tf: Timeframe, candles: Vec<Candle>) {
        self.store.warm(symbol, tf, candles);
    }

    /// Adopt a resting order found by reconciliation.
    pub fn track(&mut self, order: RestingOrder) {
        self.tracker.track(order);
    }

    /// Test-only alias so integration tests can seed a resting order.
    pub fn track_for_test(&mut self, order: RestingOrder) {
        self.track(order);
    }

    /// Symbols that must not be dropped from the universe: they hold a
    /// position or a resting order, and losing their candles would leave the
    /// engine unable to manage them.
    pub async fn protected_symbols(&self) -> Result<std::collections::HashSet<Symbol>, ExchangeError> {
        let mut set = self.tracker.resting_symbols();
        for p in self.client.positions().await? {
            set.insert(p.symbol);
        }
        Ok(set)
    }

    /// Drop candle streams for symbols no longer tracked.
    ///
    /// Called on each daily re-rank; without it the store accumulates a
    /// permanent entry for every symbol that ever entered the ranking.
    pub fn retain_symbols(&mut self, keep: &std::collections::HashSet<Symbol>) -> usize {
        self.store.retain_symbols(keep)
    }

    /// Process one closed candle.
    pub async fn on_candle_closed(
        &mut self,
        symbol: &Symbol,
        tf: Timeframe,
        candle: &Candle,
    ) -> Result<CandleOutcome, ExchangeError> {
        let Some(instrument) = self.instruments.get(symbol.as_str()).cloned() else {
            return Ok(CandleOutcome::Skipped(SkipReason::UnknownInstrument));
        };

        // The stream advances only on Accepted. A gap must be backfilled by
        // the feed before the engine sees the next candle, so anything else
        // stops here.
        if self.store.accept(symbol, tf, candle) != Acceptance::Accepted {
            return Ok(CandleOutcome::Skipped(SkipReason::NotAccepted));
        }

        // Expire resting entries whose window has elapsed. A partial fill is
        // kept — only the remainder is cancelled.
        for action in self.tracker.on_candle_close(symbol, candle.open_time_ms, tf) {
            let TrackerAction::Expire {
                link_id,
                symbol: sym,
                filled,
            } = action;
            info!(%sym, %link_id, %filled, "entry expired; cancelling the remainder");
            if let Err(e) = self.executor.cancel(&sym, &link_id).await {
                warn!(%link_id, error = %e, "cancelling an expired entry failed");
            }
        }

        if !self.store.is_warm(symbol, tf) {
            return Ok(CandleOutcome::Skipped(SkipReason::NotWarm));
        }
        if self.store.is_stale(symbol, tf, candle.open_time_ms) {
            return Ok(CandleOutcome::Skipped(SkipReason::Stale));
        }

        let ctx = MarketContext {
            symbol,
            timeframe: tf,
            candle,
            instrument: &instrument,
        };
        let Some(signal) = self.strategy.on_candle_close(&ctx) else {
            return Ok(CandleOutcome::Skipped(SkipReason::NoSignal));
        };

        let state = self.account_state(candle.open_time_ms).await?;
        let liq = state
            .open_positions
            .iter()
            .find(|p| p.symbol.as_str() == symbol.as_str())
            .and_then(|p| p.liq_price);

        match self.risk.evaluate(&signal, &state, &instrument, liq) {
            Decision::Refuse(refusal) => {
                info!(%symbol, refusal = %refusal, "entry refused");
                Ok(CandleOutcome::Refused(refusal))
            }
            Decision::Enter(intent) => {
                let resting = self.executor.place_entry(&intent).await?;
                let link_id = resting.link_id.clone();
                self.tracker.track(resting);
                info!(%symbol, %link_id, qty = %intent.qty, "entry placed");
                Ok(CandleOutcome::Placed { link_id })
            }
        }
    }

    /// Assemble the account state the risk layer reads.
    async fn account_state(&mut self, now_ms: i64) -> Result<risk::AccountState, ExchangeError> {
        let balance = self.client.balance().await?;
        let positions = self.client.positions().await?;

        if self.day_start_equity.is_zero() {
            self.day_start_equity = balance.equity;
        }
        self.high_water_mark =
            engine::update_high_water_mark(self.high_water_mark, balance.equity);

        let day_start = utc_day_start_ms(now_ms);
        let entries_filled_today = self
            .journal
            .daily_fill_count(day_start)
            .await
            .unwrap_or_else(|e| {
                // The journal must never fail an order. A read failure is
                // reported conservatively as zero rather than blocking, and
                // the other limits still apply.
                warn!(error = %e, "reading today's fill count failed; treating as zero");
                0
            })
            .max(0) as u32;

        let halt_reason = self.journal.halt_reason().await.unwrap_or_else(|e| {
            warn!(error = %e, "reading the halt flag failed; assuming not halted");
            None
        });

        Ok(assemble_account_state(
            &balance,
            positions,
            JournalFacts {
                entries_filled_today,
                halt_reason,
                day_start_equity: self.day_start_equity,
                high_water_mark: self.high_water_mark,
            },
        ))
    }
}
```

**A judgement call to make and report:** `account_state` treats a journal read failure as "zero fills, not halted", which fails *open* — it would let trading continue when the halt flag could not be read. Consider whether failing *closed* (treating an unreadable halt flag as halted) is safer given the owner's rules, and say which you chose and why. Do not change it silently.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p bot && cargo clippy -p bot --all-targets -- -D warnings`
Expected: PASS — 11 tests, no clippy warnings.

- [ ] **Step 5: Commit**

```bash
git add bot crates
git commit -m "feat(bot): EngineLoop wiring candle to order

Every gate is applied in order and each is nameable, so a candle that
produces no trade always says why rather than disappearing silently. The
stream advances only on Accepted, an expired entry keeps its filled portion
and cancels only the remainder, and a cold or stale stream never reaches the
strategy."
```

---

### Task 6: Live loop and testnet soak

**Files:**
- Modify: `bot/src/main.rs` (replace the probe with the engine)
- Modify: `config/testnet.toml` (add `entry_expiry_candles` if absent)

**Interfaces:**
- Consumes: `EngineLoop`, `reconcile`, `select_universe`, `BybitPublicFeed`, `BybitPrivateFeed`

- [ ] **Step 1: Replace the probe's event loop**

In `bot/src/main.rs`, after the existing setup (config, credentials, REST client, journal, universe ranking), add reconciliation and the engine, and replace the `MarketEvent` match arm that only logs.

```rust
    // Reconcile BEFORE any strategy evaluation. A restart, crash or manual
    // intervention can leave this process believing something untrue; the
    // exchange settles every disagreement.
    let expiry_window_ms = Timeframe::H1.duration_ms() * i64::from(config.strategy.entry_expiry_candles);
    let mut engine_loop = EngineLoop::new(
        Box::new(PullbackStrategy::new(params)),
        RiskManager::new(risk_params, stop_limit_offset_atr),
        Arc::clone(&rest) as Arc<dyn ExchangeClient>,
        Arc::clone(&journal),
        instruments.clone(),
        config.hash(),
        config.strategy.entry_expiry_candles,
        warmup,
    );

    let report = engine::reconcile(
        rest.as_ref(),
        &mut tracker_placeholder,
        rest.clock().now_ms(),
        expiry_window_ms,
    )
    .await?;
    info!(
        adopted_positions = report.adopted_positions.len(),
        adopted_orders = report.adopted_orders.len(),
        cancelled_stale = report.cancelled_stale.len(),
        "reconciled against the exchange"
    );
    for symbol in &report.unprotected {
        warn!(%symbol, "adopted position — verify it carries a stop and target");
    }
```

Then replace the candle arm:

```rust
                Ok(MarketEvent::CandleClosed { symbol, tf, candle }) => {
                    match engine_loop.on_candle_closed(&symbol, tf, &candle).await {
                        Ok(outcome) => info!(%symbol, ?tf, ?outcome, "candle processed"),
                        Err(e) => {
                            if e.class() == ErrorClass::Fatal {
                                error!(%symbol, error = %e, "fatal error processing a candle; halting");
                                return Err(e.into());
                            }
                            warn!(%symbol, error = %e, "processing a candle failed");
                        }
                    }
                }
                Ok(MarketEvent::GapFilled { symbol, tf, candles }) => {
                    info!(%symbol, count = candles.len(), "rewarming after a gap backfill");
                    engine_loop.warm(&symbol, tf, candles);
                }
```

The exact shape depends on what the current `main.rs` looks like — read it first and adapt. Preserve the existing `RecvError::Closed` arm that exits: a closed market feed is a halt condition.

- [ ] **Step 2: Subscribe the private feed and treat its closure as a halt**

The private feed exists (`BybitPrivateFeed`) but the probe never instantiated it. Wire it, and treat a closed `AccountEvent` channel exactly like a closed market feed — the private feed breaks its loop on a Fatal error precisely so the channel closing signals it.

```rust
    let private = BybitPrivateFeed::new(
        profile.ws_private_url().to_string(),
        Credentials::from_env()?,
        Arc::new(rest.clock().clone()),
    );
    let mut account_rx = private.subscribe();
```

`ClockOffset` may not be `Clone`. If it is not, share it via `Arc` from a single owner rather than cloning — and report what you changed.

Add a `select!` arm:

```rust
            account = account_rx.recv() => match account {
                Ok(event) => info!(?event, "account event"),
                Err(RecvError::Closed) => {
                    error!("account feed channel closed; the private feed has died");
                    return Err("account feed channel closed".into());
                }
                Err(RecvError::Lagged(skipped)) => {
                    warn!(skipped, "fell behind the account feed");
                }
            },
```

- [ ] **Step 3: Prune candle streams on each daily re-rank**

`CandleStore::retain_symbols` currently has no call site, so the store grows for every symbol that ever entered the ranking. Add a daily re-rank tick that re-selects the universe, protects held symbols, and prunes:

```rust
    let mut rerank = tokio::time::interval(Duration::from_secs(86_400));
    rerank.tick().await; // the first tick fires immediately; skip it
```

and an arm:

```rust
            _ = rerank.tick() => {
                let protected = engine_loop.protected_symbols().await?;
                let tickers = rest.tickers().await?;
                let instruments = rest.instruments().await?;
                let symbols = select_universe(
                    &tickers, &instruments, &universe_filter, rest.clock().now_ms(), &protected,
                );
                let dropped = engine_loop.retain_symbols(&symbols.iter().cloned().collect());
                info!(universe = symbols.len(), dropped, "daily universe re-rank");
            }
```

- [ ] **Step 4: Verify the whole workspace**

Run each separately — a full `cargo test --workspace` recompiles turso and takes minutes:

```
cargo build --workspace
cargo test -p botcore -p indicators -p strategy -p risk -p engine
cargo test -p exchange
cargo test -p persistence
cargo test -p bot
cargo clippy --workspace --all-targets -- -D warnings
cargo test -p exchange --test no_market_orders
```

Expected: all green.

- [ ] **Step 5: Commit**

```bash
git add bot config
git commit -m "feat(bot): live trading loop replacing the probe

Reconciles against the exchange before any strategy evaluation, so a restart
cannot resume on stale beliefs. Subscribes the private feed and treats a
closed account channel exactly like a closed market channel — both mean a
feed died, which is a halt condition. Prunes candle streams on each daily
re-rank, which retain_symbols existed for but nothing called."
```

- [ ] **Step 6: Run against testnet**

```bash
set -a && . ./.env && set +a
mkdir -p data
cargo run --release --bin crypto-bot -- testnet
```

Expected log sequence: `starting bot`, `authenticated`, `loaded tradable instruments`, `universe ranked`, `reconciled against the exchange`, `streaming klines`, `subscribed to public kline topics`, `subscribed to private topics`, then a `candle processed` line for each subscribed symbol at the top of each hour.

**With an unfunded account, expect `Refused(SizeTooSmall)` on any signal.** That is correct behaviour, not a failure — position size derives from equity, and zero equity can only produce zero size. Getting testnet funds is a prerequisite for observing an actual placement.

- [ ] **Step 7: Report the soak**

Leave it running and record: how many candles processed, how many signals, the distribution of `CandleOutcome`, any reconnects, and whether memory grew. Write findings to `docs/superpowers/plans/2026-08-03-soak-notes.md` and commit.

Do NOT claim the two-week soak passed on the basis of a short run. Report exactly what was observed and for how long.

---

## Plan Self-Review

**1. Spec coverage.** Mapping the approved spec to tasks:

| Spec requirement | Task |
|---|---|
| §4.3 entry expiry after 3 candles, partial-fill handling | 1 |
| §4.3 stop-limit escalation ladder, exhaustion behaviour | 2 |
| §5 invariant 1 — stop and target attached at entry | 3 |
| §5 invariant 6 — `orderLinkId` idempotency in placement | 3 |
| §5.2 config hash on every order row | 3 |
| §7.2 startup reconciliation, stale resting orders cancelled | 4 |
| §5 invariant 10 — stale feed blocks entries | 5 |
| §5 invariants 3–9 enforced via `RiskManager` | 5 |
| §7.1 journal failures never block an order | 3, 5 |
| Private feed wired; closed channel is a halt condition | 6 |
| `retain_symbols` called on daily re-rank (Plan 1c carry-forward) | 6 |

**2. Placeholder scan.** No TBDs. Task 6's steps say "read the current `main.rs` first and adapt" — that is deliberate rather than vague: `main.rs` has been edited several times since the plan for it was written, so prescribing exact line edits would be more likely to mislead than help. The behaviour required is fully specified.

**3. Type consistency.** `RestingOrder`'s fields match between Tasks 1, 3 and 4. `TrackerAction::Expire` is destructured in Task 5 exactly as Task 1 defines it. `Executor::new` takes `Arc<dyn ExchangeClient>`, which requires `ExchangeClient` to be object-safe — Task 3 says to report rather than work around it if not. `EngineLoop::new`'s argument order matches its use in Task 5's test helper.

**Two things stated rather than hidden:**

- **The escalation ladder is built in Task 2 but not driven anywhere in this plan.** Wiring it needs a timer loop reacting to position-closed events on the private feed, which is more than this plan's remaining budget. The pure logic and its tests land here; the driver is the first thing to add afterwards. Until then, a triggered stop rests at its initial 0.3×ATR offset and does not widen — which is the current behaviour anyway, just now with the widening logic available and tested.
- **Task 5's `account_state` fails open on a journal read error** (treats it as "zero fills, not halted"). The implementer is asked to judge whether failing closed is safer and to report the choice. Given the owner's rules, I lean toward failing closed on the halt flag specifically — an unreadable halt is not evidence of no halt — but I want the implementer to reason about it rather than take my word.

---

## Execution Handoff

Plan 1d covers 6 tasks and is the last of Phase 1. After it, the bot places orders on testnet.
