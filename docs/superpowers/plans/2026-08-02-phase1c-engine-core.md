# Phase 1c — Engine Core Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the deterministic state the trading engine runs on — idempotent order identity, candle bookkeeping with warmup and staleness detection, universe selection, account-state assembly — plus the `MockExchange` that the execution layer's integration tests will drive.

**Architecture:** A new `engine` crate holding pure, synchronous state machines with no network I/O. Everything here is a function of its inputs, so the whole layer is testable without a socket. The one async surface is `MockExchange`, which implements the existing `ExchangeClient` trait so Plan 1d can drive the executor and reconciler through failure modes a real exchange would only produce by accident.

**Tech Stack:** Rust 1.97 (edition 2024), rust_decimal, sha2, botcore, exchange, async-trait, tokio (test-only), proptest.

## Global Constraints

- All monetary and quantity values use `rust_decimal::Decimal`. **Never `f64`** for prices, quantities, or balances.
- The domain crate is **`botcore`**, never `core` — that name shadows Rust's sysroot crate and breaks `thiserror` derives.
- **No code path may construct `orderType: "Market"`.** `crates/exchange/tests/no_market_orders.rs` greps every `.rs` file under `crates/` and `bot/` for the quoted literal `"Market"` and fails the build. `"MarkPrice"` is a different string and is fine.
- `orderLinkId` is capped at **36 characters** by Bybit and must be unique per order.
- A stale feed **blocks new entries**: if no candle has arrived for a subscribed symbol within 2× its timeframe, the engine must not trade it.
- Daily counters roll at **00:00 UTC**. Drawdown baselines are the 00:00 UTC equity mark and the all-time high-water mark.
- Symbols with an open position or a resting entry order are **never dropped** from the universe mid-trade, regardless of ranking.
- Rust edition 2024, resolver 3, rust-version 1.97.
- `rust_decimal_macros` (`dec!`) is permitted in `#[cfg(test)]` code only. Use `Decimal::from(n)` / `Decimal::new(mantissa, scale)` in production code.

## What already exists

- **`botcore`** — `Candle { open_time_ms, open, high, low, close, volume, turnover }`, `Timeframe::{H1,H4}` (`as_bybit_interval()`, `duration_ms()`), `Symbol` (`new()`, `as_str()`), `Instrument { symbol, tick_size, qty_step, min_order_qty, launch_time_ms }` (`age_days(now_ms)`, `qty_is_valid(qty)`), `Side::{Buy,Sell}` (`as_bybit()`, `opposite()`), `Position { symbol, side, size, entry_price, liq_price: Option<Decimal>, unrealized_pnl }`, `Balance { equity, available }`, `LimitEntry`, `OrderAck`, `OpenOrder { symbol, order_id, order_link_id, side, price, qty, cum_exec_qty, state, created_time_ms }`, `OrderState::{New,PartiallyFilled,Filled,Cancelled,Rejected}`, `ErrorClass::{Retryable,Rejected,Fatal}`, `money::{round_down_to_step, round_price_away_from_market}`.
- **`exchange`** — `ExchangeClient` trait (async: `instruments`, `tickers`, `klines`, `place_limit_entry`, `amend_stop`, `cancel_order`, `positions`, `open_orders`, `set_leverage`, `balance`), `MarketFeed`, `MarketEvent::{CandleClosed,GapFilled}`, `Subscription { symbol, timeframe }`, `bybit::wire::Ticker { symbol, turnover_24h, last_price }`, `bybit::transport::ExchangeError`.
- **`risk`** — `AccountState { equity, available, open_positions, day_start_equity, high_water_mark, entries_filled_today, halt_reason }`, `RiskParams`, `Refusal`, `Decision`, `OrderIntent`, `RiskManager`.
- **`strategy`** — `Signal`, `MarketContext`, `Strategy`, `PullbackStrategy`, `StrategyParams`.
- **`persistence`** — `Journal` with `daily_fill_count(utc_day_start_ms)`, `halt_reason()`, `record_equity(equity, at_ms)`.

---

## File Structure

```
crates/engine/
├── Cargo.toml
└── src/
    ├── lib.rs           re-exports
    ├── link_id.rs       deterministic, idempotent orderLinkId
    ├── candle_store.rs  rolling candles per (symbol, timeframe); warmup + staleness
    ├── universe.rs      daily re-rank with liquidity and age filters
    ├── account.rs       AccountState assembly, UTC day boundary, high-water mark
    └── mock.rs          MockExchange test double implementing ExchangeClient
```

Each file answers one question: *what is this order called*, *what have we seen*, *what may we trade*, *what shape is the account*, and *how do we test all of it without a network*.

---

### Task 1: Deterministic orderLinkId

**Files:**
- Create: `crates/engine/Cargo.toml`, `crates/engine/src/lib.rs`, `crates/engine/src/link_id.rs`
- Modify: root `Cargo.toml` (add `"crates/engine"` to members)

**Interfaces:**
- Consumes: `botcore::{Side, Symbol}`
- Produces: `order_link_id(symbol: &Symbol, signal_candle_open_ms: i64, side: Side) -> String`

- [ ] **Step 1: Create the crate manifest**

Create `crates/engine/Cargo.toml`:

```toml
[package]
name = "engine"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true

[dependencies]
botcore = { path = "../botcore" }
exchange = { path = "../exchange" }
risk = { path = "../risk" }
strategy = { path = "../strategy" }
async-trait = "0.1"
rust_decimal.workspace = true
sha2 = "0.10"
hex = "0.4"
thiserror.workspace = true
tokio.workspace = true
tracing.workspace = true

[dev-dependencies]
rust_decimal_macros.workspace = true
proptest = "1"
```

Add `"crates/engine"` to the `members` array in the root `Cargo.toml`.

- [ ] **Step 2: Write the failing test**

Create `crates/engine/src/link_id.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use botcore::{Side, Symbol};

    #[test]
    fn the_same_signal_always_produces_the_same_id() {
        // This is the whole point: a retry after a network timeout must reuse
        // the same id so Bybit deduplicates it instead of opening a second
        // position.
        let a = order_link_id(&Symbol::new("BTCUSDT"), 1_700_000_000_000, Side::Buy);
        let b = order_link_id(&Symbol::new("BTCUSDT"), 1_700_000_000_000, Side::Buy);
        assert_eq!(a, b);
    }

    #[test]
    fn every_input_changes_the_id() {
        let base = order_link_id(&Symbol::new("BTCUSDT"), 1_700_000_000_000, Side::Buy);
        assert_ne!(base, order_link_id(&Symbol::new("ETHUSDT"), 1_700_000_000_000, Side::Buy));
        assert_ne!(base, order_link_id(&Symbol::new("BTCUSDT"), 1_700_000_003_600, Side::Buy));
        assert_ne!(base, order_link_id(&Symbol::new("BTCUSDT"), 1_700_000_000_000, Side::Sell));
    }

    #[test]
    fn the_id_fits_bybits_thirty_six_character_limit() {
        let id = order_link_id(&Symbol::new("SOMEVERYLONGSYMBOLNAMEUSDT"), i64::MAX, Side::Sell);
        assert!(id.len() <= 36, "id was {} chars: {id}", id.len());
        assert!(!id.is_empty());
    }

    #[test]
    fn the_id_is_safe_for_a_url_and_a_json_body() {
        // A raw symbol could in principle carry characters that need escaping;
        // a hex digest cannot.
        let id = order_link_id(&Symbol::new("BTCUSDT"), 1_700_000_000_000, Side::Buy);
        assert!(
            id.chars().all(|c| c.is_ascii_alphanumeric()),
            "id contained a character needing escaping: {id}"
        );
    }

    #[test]
    fn adjacent_candles_do_not_collide() {
        // Consecutive 1h candles are one hour apart; their ids must differ.
        let a = order_link_id(&Symbol::new("BTCUSDT"), 1_700_000_000_000, Side::Buy);
        let b = order_link_id(&Symbol::new("BTCUSDT"), 1_700_000_000_000 + 3_600_000, Side::Buy);
        assert_ne!(a, b);
    }
}
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test -p engine`
Expected: FAIL — compile error, `order_link_id` not found.

- [ ] **Step 4: Implement**

Prepend to `crates/engine/src/link_id.rs`:

```rust
use botcore::{Side, Symbol};
use sha2::{Digest, Sha256};

/// Prefix so an id is recognisably ours in Bybit's UI and logs.
const PREFIX: &str = "cb";

/// Hex characters kept from the digest. Combined with the prefix this yields a
/// 34-character id, inside Bybit's 36-character limit with room to spare.
const DIGEST_CHARS: usize = 32;

/// A deterministic, idempotent order identifier.
///
/// Derived from the signal rather than randomly generated, so a retry after a
/// network timeout reuses the same id and Bybit deduplicates it. Without this,
/// a request that timed out *after* the exchange accepted it would be retried
/// and open a second position — the single most expensive failure mode
/// available to an order-placing bot.
///
/// A hex digest also guarantees the id needs no escaping in a query string or
/// JSON body, which matters because the same bytes are both transmitted and
/// signed.
pub fn order_link_id(symbol: &Symbol, signal_candle_open_ms: i64, side: Side) -> String {
    let mut hasher = Sha256::new();
    hasher.update(symbol.as_str().as_bytes());
    hasher.update(b"|");
    hasher.update(signal_candle_open_ms.to_be_bytes());
    hasher.update(b"|");
    hasher.update(side.as_bybit().as_bytes());
    let digest = hex::encode(hasher.finalize());
    format!("{PREFIX}{}", &digest[..DIGEST_CHARS])
}
```

Create `crates/engine/src/lib.rs`:

```rust
pub mod link_id;

pub use link_id::order_link_id;
```

- [ ] **Step 5: Run the tests**

Run: `cargo test -p engine && cargo clippy -p engine --all-targets -- -D warnings`
Expected: PASS — 5 tests, no clippy warnings.

- [ ] **Step 6: Commit**

```bash
git add crates/engine Cargo.toml Cargo.lock
git commit -m "feat(engine): deterministic idempotent orderLinkId

Derived from the signal rather than randomly generated, so a retry after a
network timeout reuses the same id and the exchange deduplicates it. A request
that timed out after the exchange accepted it would otherwise be retried and
open a second position. Hex output needs no escaping in the query string that
is both transmitted and signed."
```

---

### Task 2: CandleStore — warmup, rolling window, staleness

**Files:**
- Create: `crates/engine/src/candle_store.rs`
- Modify: `crates/engine/src/lib.rs`

**Interfaces:**
- Consumes: `botcore::{Candle, Symbol, Timeframe}`
- Produces:
  - `CandleStore::new(warmup_candles: usize) -> Self`
  - `CandleStore::warm(&mut self, symbol: &Symbol, tf: Timeframe, candles: Vec<Candle>)`
  - `CandleStore::accept(&mut self, symbol: &Symbol, tf: Timeframe, candle: &Candle) -> Acceptance`
  - `CandleStore::last_open_ms(&self, symbol: &Symbol, tf: Timeframe) -> Option<i64>`
  - `CandleStore::is_warm(&self, symbol: &Symbol, tf: Timeframe) -> bool`
  - `CandleStore::is_stale(&self, symbol: &Symbol, tf: Timeframe, now_ms: i64) -> bool`
  - `Acceptance::{Accepted, Duplicate, OutOfOrder, Gap { missing: i64 }}`

- [ ] **Step 1: Write the failing test**

Create `crates/engine/src/candle_store.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use botcore::{Candle, Symbol, Timeframe};
    use rust_decimal::Decimal;

    const H1: i64 = 3_600_000;

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

    fn btc() -> Symbol {
        Symbol::new("BTCUSDT")
    }

    #[test]
    fn a_fresh_store_is_cold_and_has_no_last_candle() {
        let s = CandleStore::new(10);
        assert!(!s.is_warm(&btc(), Timeframe::H1));
        assert_eq!(s.last_open_ms(&btc(), Timeframe::H1), None);
    }

    #[test]
    fn warming_with_enough_history_makes_the_stream_warm() {
        let mut s = CandleStore::new(10);
        let history: Vec<Candle> = (0..10).map(|i| candle(i * H1)).collect();
        s.warm(&btc(), Timeframe::H1, history);
        assert!(s.is_warm(&btc(), Timeframe::H1));
        assert_eq!(s.last_open_ms(&btc(), Timeframe::H1), Some(9 * H1));
    }

    #[test]
    fn warming_with_insufficient_history_leaves_the_stream_cold() {
        // Acting on a half-warm indicator is how a restart produces a bad
        // trade; the store must be able to say "not yet".
        let mut s = CandleStore::new(10);
        let history: Vec<Candle> = (0..9).map(|i| candle(i * H1)).collect();
        s.warm(&btc(), Timeframe::H1, history);
        assert!(!s.is_warm(&btc(), Timeframe::H1));
    }

    #[test]
    fn a_consecutive_candle_is_accepted() {
        let mut s = CandleStore::new(2);
        s.warm(&btc(), Timeframe::H1, vec![candle(0), candle(H1)]);
        assert_eq!(
            s.accept(&btc(), Timeframe::H1, &candle(2 * H1)),
            Acceptance::Accepted
        );
        assert_eq!(s.last_open_ms(&btc(), Timeframe::H1), Some(2 * H1));
    }

    #[test]
    fn a_repeated_candle_is_a_duplicate_and_does_not_advance_the_stream() {
        // Bybit can redeliver a confirmed candle after a reconnect. Processing
        // it twice would feed the same bar into indicators twice.
        let mut s = CandleStore::new(1);
        s.warm(&btc(), Timeframe::H1, vec![candle(0)]);
        assert_eq!(
            s.accept(&btc(), Timeframe::H1, &candle(0)),
            Acceptance::Duplicate
        );
        assert_eq!(s.last_open_ms(&btc(), Timeframe::H1), Some(0));
    }

    #[test]
    fn an_older_candle_is_out_of_order_and_does_not_rewind_the_stream() {
        let mut s = CandleStore::new(1);
        s.warm(&btc(), Timeframe::H1, vec![candle(5 * H1)]);
        assert_eq!(
            s.accept(&btc(), Timeframe::H1, &candle(3 * H1)),
            Acceptance::OutOfOrder
        );
        assert_eq!(s.last_open_ms(&btc(), Timeframe::H1), Some(5 * H1));
    }

    #[test]
    fn a_skipped_candle_is_reported_as_a_gap_and_does_not_advance_the_stream() {
        // The engine must backfill before resuming; advancing here would make
        // the hole permanently undetectable.
        let mut s = CandleStore::new(1);
        s.warm(&btc(), Timeframe::H1, vec![candle(0)]);
        assert_eq!(
            s.accept(&btc(), Timeframe::H1, &candle(2 * H1)),
            Acceptance::Gap { missing: 1 }
        );
        assert_eq!(s.last_open_ms(&btc(), Timeframe::H1), Some(0));
    }

    #[test]
    fn the_first_candle_of_a_cold_stream_is_accepted_without_a_gap() {
        let mut s = CandleStore::new(1);
        assert_eq!(
            s.accept(&btc(), Timeframe::H1, &candle(9 * H1)),
            Acceptance::Accepted
        );
    }

    #[test]
    fn a_stream_is_stale_after_twice_its_timeframe() {
        let mut s = CandleStore::new(1);
        s.warm(&btc(), Timeframe::H1, vec![candle(0)]);
        // The candle opened at 0 and covers up to H1. Two timeframes past its
        // open is the threshold.
        assert!(!s.is_stale(&btc(), Timeframe::H1, 2 * H1 - 1));
        assert!(s.is_stale(&btc(), Timeframe::H1, 2 * H1));
    }

    #[test]
    fn a_stream_that_has_never_produced_a_candle_is_stale() {
        // Never having heard from a subscribed symbol is not a healthy state.
        let s = CandleStore::new(1);
        assert!(s.is_stale(&btc(), Timeframe::H1, 1_700_000_000_000));
    }

    #[test]
    fn streams_are_tracked_per_symbol_and_per_timeframe() {
        let mut s = CandleStore::new(1);
        s.warm(&btc(), Timeframe::H1, vec![candle(0)]);
        assert_eq!(s.last_open_ms(&btc(), Timeframe::H4), None);
        assert_eq!(s.last_open_ms(&Symbol::new("ETHUSDT"), Timeframe::H1), None);
    }

    #[test]
    fn the_window_is_bounded_to_the_warmup_length() {
        // Unbounded growth over a 24/7 run is a slow leak.
        let mut s = CandleStore::new(3);
        for i in 0..100 {
            s.accept(&btc(), Timeframe::H1, &candle(i * H1));
        }
        assert_eq!(s.window_len(&btc(), Timeframe::H1), 3);
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p engine`
Expected: FAIL — `CandleStore` and `Acceptance` not found.

- [ ] **Step 3: Implement**

Prepend to `crates/engine/src/candle_store.rs`:

```rust
use std::collections::HashMap;
use std::collections::VecDeque;

use botcore::{Candle, Symbol, Timeframe};

/// What the store did with an offered candle.
///
/// Every variant except `Accepted` means the stream did NOT advance — the
/// caller must not feed the candle to a strategy, and in the `Gap` case must
/// backfill before resuming.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Acceptance {
    Accepted,
    /// Already seen. Bybit redelivers confirmed candles after a reconnect.
    Duplicate,
    /// Older than the last candle seen. Never rewind the stream.
    OutOfOrder,
    /// `missing` candles are absent between the last one and this one.
    Gap { missing: i64 },
}

#[derive(Debug, Default)]
struct Stream {
    /// Bounded to the warmup length, newest last.
    window: VecDeque<Candle>,
    last_open_ms: Option<i64>,
}

/// Per-(symbol, timeframe) candle bookkeeping.
///
/// Answers three questions the engine cannot trade without: have we seen
/// enough history to trust an indicator, is this candle the next one, and has
/// this stream gone quiet.
#[derive(Debug)]
pub struct CandleStore {
    warmup_candles: usize,
    streams: HashMap<(Symbol, Timeframe), Stream>,
}

impl CandleStore {
    pub fn new(warmup_candles: usize) -> Self {
        CandleStore {
            warmup_candles,
            streams: HashMap::new(),
        }
    }

    /// Seed a stream with historical candles after a restart or a gap backfill.
    ///
    /// Candles are sorted by open time before insertion, so a caller handing
    /// over a newest-first REST response cannot silently reverse the series.
    pub fn warm(&mut self, symbol: &Symbol, tf: Timeframe, mut candles: Vec<Candle>) {
        candles.sort_by_key(|c| c.open_time_ms);
        let stream = self
            .streams
            .entry((symbol.clone(), tf))
            .or_default();
        stream.window.clear();
        stream.last_open_ms = candles.last().map(|c| c.open_time_ms);
        for c in candles {
            stream.window.push_back(c);
        }
        while stream.window.len() > self.warmup_candles {
            stream.window.pop_front();
        }
    }

    /// Offer the next candle. The stream advances only on `Accepted`.
    pub fn accept(&mut self, symbol: &Symbol, tf: Timeframe, candle: &Candle) -> Acceptance {
        let warmup = self.warmup_candles;
        let stream = self
            .streams
            .entry((symbol.clone(), tf))
            .or_default();

        if let Some(last) = stream.last_open_ms {
            let step = tf.duration_ms();
            let delta = candle.open_time_ms - last;
            if delta == 0 {
                return Acceptance::Duplicate;
            }
            if delta < 0 {
                return Acceptance::OutOfOrder;
            }
            if delta > step {
                return Acceptance::Gap {
                    missing: delta / step - 1,
                };
            }
        }

        stream.last_open_ms = Some(candle.open_time_ms);
        stream.window.push_back(candle.clone());
        while stream.window.len() > warmup {
            stream.window.pop_front();
        }
        Acceptance::Accepted
    }

    pub fn last_open_ms(&self, symbol: &Symbol, tf: Timeframe) -> Option<i64> {
        self.streams
            .get(&(symbol.clone(), tf))
            .and_then(|s| s.last_open_ms)
    }

    /// How many candles the bounded window currently holds.
    pub fn window_len(&self, symbol: &Symbol, tf: Timeframe) -> usize {
        self.streams
            .get(&(symbol.clone(), tf))
            .map(|s| s.window.len())
            .unwrap_or(0)
    }

    /// Whether enough history has been seen to trust an indicator built from it.
    pub fn is_warm(&self, symbol: &Symbol, tf: Timeframe) -> bool {
        self.window_len(symbol, tf) >= self.warmup_candles
    }

    /// Whether the stream has gone quiet.
    ///
    /// A stream that has never produced a candle counts as stale: never having
    /// heard from a subscribed symbol is not a healthy state, and trading it
    /// would mean acting with no market data at all.
    pub fn is_stale(&self, symbol: &Symbol, tf: Timeframe, now_ms: i64) -> bool {
        match self.last_open_ms(symbol, tf) {
            None => true,
            Some(last) => now_ms - last >= 2 * tf.duration_ms(),
        }
    }
}
```

Add to `crates/engine/src/lib.rs`:

```rust
pub mod candle_store;

pub use candle_store::{Acceptance, CandleStore};
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p engine && cargo clippy -p engine --all-targets -- -D warnings`
Expected: PASS — 17 tests, no clippy warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/engine
git commit -m "feat(engine): CandleStore with warmup, ordering and staleness

The stream advances only on Accepted. A duplicate (Bybit redelivers confirmed
candles after a reconnect), an out-of-order candle, or a gap all leave
last_open_ms untouched — advancing past a hole would make it permanently
undetectable. A stream that has never produced a candle counts as stale, since
never hearing from a subscribed symbol is not a healthy state."
```

---

### Task 3: Universe selection

**Files:**
- Create: `crates/engine/src/universe.rs`
- Modify: `crates/engine/src/lib.rs`

**Interfaces:**
- Consumes: `botcore::{Instrument, Symbol}`, `exchange::bybit::wire::Ticker`
- Produces:
  - `UniverseFilter { size: usize, min_turnover_24h: Decimal, min_listing_age_days: i64 }` with `UniverseFilter::defaults()`
  - `select_universe(tickers: &[Ticker], instruments: &[Instrument], filter: &UniverseFilter, now_ms: i64, protected: &HashSet<Symbol>) -> Vec<Symbol>`

- [ ] **Step 1: Write the failing test**

Create `crates/engine/src/universe.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use botcore::{Instrument, Symbol};
    use exchange::bybit::wire::Ticker;
    use rust_decimal_macros::dec;
    use std::collections::HashSet;

    const DAY: i64 = 86_400_000;
    const NOW: i64 = 1_700_000_000_000;

    fn ticker(sym: &str, turnover: Decimal) -> Ticker {
        Ticker {
            symbol: Symbol::new(sym),
            turnover_24h: turnover,
            last_price: dec!(100),
        }
    }

    fn instrument(sym: &str, age_days: i64) -> Instrument {
        Instrument {
            symbol: Symbol::new(sym),
            tick_size: dec!(0.1),
            qty_step: dec!(0.001),
            min_order_qty: dec!(0.001),
            launch_time_ms: NOW - age_days * DAY,
        }
    }

    #[test]
    fn symbols_are_ranked_by_turnover_descending() {
        let tickers = vec![
            ticker("AAAUSDT", dec!(5_000_000)),
            ticker("BBBUSDT", dec!(9_000_000)),
            ticker("CCCUSDT", dec!(7_000_000)),
        ];
        let instruments = vec![
            instrument("AAAUSDT", 100),
            instrument("BBBUSDT", 100),
            instrument("CCCUSDT", 100),
        ];
        let f = UniverseFilter {
            size: 3,
            min_turnover_24h: dec!(1_000_000),
            min_listing_age_days: 30,
        };
        let out = select_universe(&tickers, &instruments, &f, NOW, &HashSet::new());
        assert_eq!(
            out.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            vec!["BBBUSDT", "CCCUSDT", "AAAUSDT"]
        );
    }

    #[test]
    fn the_result_is_truncated_to_the_configured_size() {
        let tickers = vec![
            ticker("AAAUSDT", dec!(5_000_000)),
            ticker("BBBUSDT", dec!(9_000_000)),
            ticker("CCCUSDT", dec!(7_000_000)),
        ];
        let instruments = vec![
            instrument("AAAUSDT", 100),
            instrument("BBBUSDT", 100),
            instrument("CCCUSDT", 100),
        ];
        let f = UniverseFilter {
            size: 2,
            min_turnover_24h: dec!(1_000_000),
            min_listing_age_days: 30,
        };
        let out = select_universe(&tickers, &instruments, &f, NOW, &HashSet::new());
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].as_str(), "BBBUSDT");
    }

    #[test]
    fn symbols_below_the_turnover_floor_are_excluded() {
        let tickers = vec![
            ticker("THINUSDT", dec!(100)),
            ticker("DEEPUSDT", dec!(9_000_000)),
        ];
        let instruments = vec![instrument("THINUSDT", 100), instrument("DEEPUSDT", 100)];
        let f = UniverseFilter {
            size: 10,
            min_turnover_24h: dec!(1_000_000),
            min_listing_age_days: 30,
        };
        let out = select_universe(&tickers, &instruments, &f, NOW, &HashSet::new());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].as_str(), "DEEPUSDT");
    }

    #[test]
    fn freshly_listed_symbols_are_excluded() {
        // A new listing has no price history for indicators and unstable
        // liquidity; a backtest over it would also be meaningless.
        let tickers = vec![
            ticker("NEWUSDT", dec!(9_000_000)),
            ticker("OLDUSDT", dec!(9_000_000)),
        ];
        let instruments = vec![instrument("NEWUSDT", 5), instrument("OLDUSDT", 100)];
        let f = UniverseFilter {
            size: 10,
            min_turnover_24h: dec!(1_000_000),
            min_listing_age_days: 30,
        };
        let out = select_universe(&tickers, &instruments, &f, NOW, &HashSet::new());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].as_str(), "OLDUSDT");
    }

    #[test]
    fn a_ticker_with_no_matching_instrument_is_excluded() {
        // Without instrument metadata there is no tick size or qty step, so no
        // order could be correctly formed anyway.
        let tickers = vec![ticker("GHOSTUSDT", dec!(9_000_000))];
        let f = UniverseFilter {
            size: 10,
            min_turnover_24h: dec!(1_000_000),
            min_listing_age_days: 30,
        };
        let out = select_universe(&tickers, &[], &f, NOW, &HashSet::new());
        assert!(out.is_empty());
    }

    #[test]
    fn a_protected_symbol_is_retained_even_when_it_fails_every_filter() {
        // Holding a position in a symbol that just fell out of the ranking must
        // not orphan it — the engine still needs its candles to manage the exit.
        let tickers = vec![
            ticker("HELDUSDT", dec!(1)),
            ticker("DEEPUSDT", dec!(9_000_000)),
        ];
        let instruments = vec![instrument("HELDUSDT", 1), instrument("DEEPUSDT", 100)];
        let f = UniverseFilter {
            size: 1,
            min_turnover_24h: dec!(1_000_000),
            min_listing_age_days: 30,
        };
        let mut protected = HashSet::new();
        protected.insert(Symbol::new("HELDUSDT"));

        let out = select_universe(&tickers, &instruments, &f, NOW, &protected);
        let names: Vec<_> = out.iter().map(|s| s.as_str()).collect();
        assert!(names.contains(&"HELDUSDT"), "protected symbol was dropped: {names:?}");
        assert!(names.contains(&"DEEPUSDT"));
    }

    #[test]
    fn a_protected_symbol_is_not_duplicated_when_it_also_qualifies() {
        let tickers = vec![ticker("DEEPUSDT", dec!(9_000_000))];
        let instruments = vec![instrument("DEEPUSDT", 100)];
        let f = UniverseFilter {
            size: 10,
            min_turnover_24h: dec!(1_000_000),
            min_listing_age_days: 30,
        };
        let mut protected = HashSet::new();
        protected.insert(Symbol::new("DEEPUSDT"));

        let out = select_universe(&tickers, &instruments, &f, NOW, &protected);
        assert_eq!(out.len(), 1, "symbol appeared twice: {out:?}");
    }

    #[test]
    fn defaults_match_the_spec() {
        let f = UniverseFilter::defaults();
        assert_eq!(f.size, 20);
        assert_eq!(f.min_turnover_24h, dec!(50_000_000));
        assert_eq!(f.min_listing_age_days, 30);
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p engine`
Expected: FAIL — `UniverseFilter` and `select_universe` not found.

- [ ] **Step 3: Implement**

Prepend to `crates/engine/src/universe.rs`:

```rust
use std::collections::{HashMap, HashSet};

use botcore::{Instrument, Symbol};
use exchange::bybit::wire::Ticker;
use rust_decimal::Decimal;

/// Which symbols the bot is willing to trade.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UniverseFilter {
    pub size: usize,
    pub min_turnover_24h: Decimal,
    pub min_listing_age_days: i64,
}

impl UniverseFilter {
    /// The spec's defaults: top 20 by 24h turnover, at least 50M USDT turnover,
    /// listed at least 30 days.
    pub fn defaults() -> Self {
        UniverseFilter {
            size: 20,
            min_turnover_24h: Decimal::from(50_000_000),
            min_listing_age_days: 30,
        }
    }
}

/// Rank and filter the tradable universe.
///
/// `protected` holds symbols with an open position or a resting entry order.
/// Those are ALWAYS retained regardless of ranking or filters: dropping a
/// symbol the bot is still holding would stop its candles arriving, leaving the
/// engine unable to manage the exit. They are added on top of the ranked set,
/// so a protected symbol never consumes one of the `size` slots.
///
/// A ticker with no matching instrument is excluded — without tick size and
/// quantity step no valid order could be formed for it anyway.
pub fn select_universe(
    tickers: &[Ticker],
    instruments: &[Instrument],
    filter: &UniverseFilter,
    now_ms: i64,
    protected: &HashSet<Symbol>,
) -> Vec<Symbol> {
    let by_symbol: HashMap<&str, &Instrument> = instruments
        .iter()
        .map(|i| (i.symbol.as_str(), i))
        .collect();

    let mut qualified: Vec<&Ticker> = tickers
        .iter()
        .filter(|t| {
            let Some(inst) = by_symbol.get(t.symbol.as_str()) else {
                return false;
            };
            t.turnover_24h >= filter.min_turnover_24h
                && inst.age_days(now_ms) >= filter.min_listing_age_days
        })
        .collect();

    // Descending turnover. Ties break on symbol name so the ordering is
    // deterministic across runs rather than dependent on the exchange's
    // response order.
    qualified.sort_by(|a, b| {
        b.turnover_24h
            .cmp(&a.turnover_24h)
            .then_with(|| a.symbol.as_str().cmp(b.symbol.as_str()))
    });

    let mut out: Vec<Symbol> = qualified
        .into_iter()
        .take(filter.size)
        .map(|t| t.symbol.clone())
        .collect();

    let already: HashSet<&str> = out.iter().map(|s| s.as_str()).collect();
    let mut extras: Vec<Symbol> = protected
        .iter()
        .filter(|s| !already.contains(s.as_str()))
        .cloned()
        .collect();
    // Deterministic order for the appended protected symbols too.
    extras.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    out.extend(extras);

    out
}
```

Add to `crates/engine/src/lib.rs`:

```rust
pub mod universe;

pub use universe::{UniverseFilter, select_universe};
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p engine && cargo clippy -p engine --all-targets -- -D warnings`
Expected: PASS — 25 tests, no clippy warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/engine
git commit -m "feat(engine): universe selection with liquidity and age filters

Symbols with an open position or resting order are retained unconditionally
and appended outside the size limit — dropping a symbol still being held would
stop its candles arriving and leave the engine unable to manage the exit.
Ties break on symbol name so the ranking is deterministic across runs rather
than dependent on the exchange's response order."
```

---

### Task 4: Account state assembly

**Files:**
- Create: `crates/engine/src/account.rs`
- Modify: `crates/engine/src/lib.rs`

**Interfaces:**
- Consumes: `botcore::{Balance, Position}`, `risk::AccountState`
- Produces:
  - `utc_day_start_ms(now_ms: i64) -> i64`
  - `update_high_water_mark(previous: Decimal, current_equity: Decimal) -> Decimal`
  - `JournalFacts { entries_filled_today: u32, halt_reason: Option<String>, day_start_equity: Decimal, high_water_mark: Decimal }`
  - `assemble_account_state(balance: &Balance, positions: Vec<Position>, facts: JournalFacts) -> AccountState`

- [ ] **Step 1: Write the failing test**

Create `crates/engine/src/account.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use botcore::{Balance, Position, Side, Symbol};
    use rust_decimal_macros::dec;

    const DAY: i64 = 86_400_000;

    fn balance() -> Balance {
        Balance {
            equity: dec!(10000),
            available: dec!(9000),
        }
    }

    fn facts() -> JournalFacts {
        JournalFacts {
            entries_filled_today: 2,
            halt_reason: None,
            day_start_equity: dec!(10500),
            high_water_mark: dec!(12000),
        }
    }

    #[test]
    fn a_timestamp_exactly_on_midnight_is_its_own_day_start() {
        assert_eq!(utc_day_start_ms(5 * DAY), 5 * DAY);
    }

    #[test]
    fn a_timestamp_mid_day_floors_to_the_preceding_midnight() {
        assert_eq!(utc_day_start_ms(5 * DAY + 1), 5 * DAY);
        assert_eq!(utc_day_start_ms(5 * DAY + DAY - 1), 5 * DAY);
    }

    #[test]
    fn day_boundaries_are_contiguous_with_no_gap_or_overlap() {
        let a = utc_day_start_ms(5 * DAY + DAY - 1);
        let b = utc_day_start_ms(6 * DAY);
        assert_eq!(b - a, DAY);
    }

    #[test]
    fn the_high_water_mark_rises_with_a_new_peak() {
        assert_eq!(update_high_water_mark(dec!(10000), dec!(11000)), dec!(11000));
    }

    #[test]
    fn the_high_water_mark_never_falls() {
        // If it tracked equity downward, the total-drawdown halt could never
        // fire — the baseline would keep retreating to meet the loss.
        assert_eq!(update_high_water_mark(dec!(12000), dec!(9000)), dec!(12000));
        assert_eq!(update_high_water_mark(dec!(12000), dec!(12000)), dec!(12000));
    }

    #[test]
    fn assembly_carries_every_field_through_unchanged() {
        let positions = vec![Position {
            symbol: Symbol::new("BTCUSDT"),
            side: Side::Buy,
            size: dec!(1),
            entry_price: dec!(100),
            liq_price: None,
            unrealized_pnl: dec!(5),
        }];
        let state = assemble_account_state(&balance(), positions, facts());

        assert_eq!(state.equity, dec!(10000));
        assert_eq!(state.available, dec!(9000));
        assert_eq!(state.open_positions.len(), 1);
        assert_eq!(state.day_start_equity, dec!(10500));
        assert_eq!(state.high_water_mark, dec!(12000));
        assert_eq!(state.entries_filled_today, 2);
        assert_eq!(state.halt_reason, None);
    }

    #[test]
    fn a_persisted_halt_reason_survives_assembly() {
        // The halt must reach the risk layer, or a restart would silently
        // resume trading a halted account.
        let mut f = facts();
        f.halt_reason = Some("daily drawdown".into());
        let state = assemble_account_state(&balance(), vec![], f);
        assert_eq!(state.halt_reason.as_deref(), Some("daily drawdown"));
    }

    #[test]
    fn assembly_raises_a_stale_high_water_mark_to_current_equity() {
        // Equity above the recorded peak means the journal's mark is behind;
        // using it unraised would understate later drawdowns.
        let mut f = facts();
        f.high_water_mark = dec!(9000);
        let state = assemble_account_state(&balance(), vec![], f);
        assert_eq!(state.high_water_mark, dec!(10000));
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p engine`
Expected: FAIL — `utc_day_start_ms`, `JournalFacts`, `assemble_account_state` not found.

- [ ] **Step 3: Implement**

Prepend to `crates/engine/src/account.rs`:

```rust
use botcore::{Balance, Position};
use risk::AccountState;
use rust_decimal::Decimal;

const MS_PER_DAY: i64 = 86_400_000;

/// Floor a timestamp to the most recent 00:00 UTC boundary.
///
/// Both the daily entry cap and the daily drawdown baseline are defined
/// against this boundary, so every consumer must agree on exactly where it
/// falls.
pub fn utc_day_start_ms(now_ms: i64) -> i64 {
    now_ms.div_euclid(MS_PER_DAY) * MS_PER_DAY
}

/// The all-time peak equity, which may only rise.
///
/// If this tracked equity downward the total-drawdown halt could never fire —
/// the baseline would keep retreating to meet the loss.
pub fn update_high_water_mark(previous: Decimal, current_equity: Decimal) -> Decimal {
    if current_equity > previous {
        current_equity
    } else {
        previous
    }
}

/// What the journal knows that the exchange does not.
///
/// The exchange reports positions and balance; the journal holds today's fill
/// count, the persisted halt, and the drawdown baselines. Both halves are
/// needed before the risk layer can decide anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalFacts {
    pub entries_filled_today: u32,
    pub halt_reason: Option<String>,
    pub day_start_equity: Decimal,
    pub high_water_mark: Decimal,
}

/// Combine exchange truth and journal memory into the state the risk layer reads.
///
/// The high-water mark is raised to current equity when the journal's recorded
/// mark is behind — otherwise a session that made a new peak before the mark was
/// persisted would understate every drawdown measured afterwards.
pub fn assemble_account_state(
    balance: &Balance,
    positions: Vec<Position>,
    facts: JournalFacts,
) -> AccountState {
    AccountState {
        equity: balance.equity,
        available: balance.available,
        open_positions: positions,
        day_start_equity: facts.day_start_equity,
        high_water_mark: update_high_water_mark(facts.high_water_mark, balance.equity),
        entries_filled_today: facts.entries_filled_today,
        halt_reason: facts.halt_reason,
    }
}
```

Add to `crates/engine/src/lib.rs`:

```rust
pub mod account;

pub use account::{JournalFacts, assemble_account_state, update_high_water_mark, utc_day_start_ms};
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p engine && cargo clippy -p engine --all-targets -- -D warnings`
Expected: PASS — 33 tests, no clippy warnings.

- [ ] **Step 5: Add a property test for the day boundary**

Append to the `tests` module in `crates/engine/src/account.rs`:

```rust
    use proptest::prelude::*;

    proptest! {
        /// Every timestamp must land in exactly one day, and the boundary must
        /// never sit in the future relative to the timestamp itself. Negative
        /// timestamps (pre-1970) are included deliberately: integer division
        /// truncates toward zero and would place them in the wrong day, which
        /// `div_euclid` avoids.
        #[test]
        fn a_day_start_always_precedes_its_timestamp_and_is_within_one_day(
            now_ms in -1_000_000_000_000i64..4_000_000_000_000i64,
        ) {
            let start = utc_day_start_ms(now_ms);
            prop_assert!(start <= now_ms, "day start {start} was after {now_ms}");
            prop_assert!(
                now_ms - start < MS_PER_DAY,
                "day start {start} was more than a day before {now_ms}"
            );
            prop_assert_eq!(start % MS_PER_DAY, 0, "day start {} was not on a boundary", start);
        }
    }
```

- [ ] **Step 6: Run the property test**

Run: `cargo test -p engine && cargo clippy -p engine --all-targets -- -D warnings`
Expected: PASS — 34 tests, no clippy warnings.

If the property test fails on negative timestamps, the bug is in `utc_day_start_ms` using `/` instead of `div_euclid` — fix the function, not the test range.

- [ ] **Step 7: Commit**

```bash
git add crates/engine
git commit -m "feat(engine): account state assembly and UTC day boundary

The high-water mark may only rise; tracking equity downward would let the
baseline retreat to meet a loss so the total-drawdown halt could never fire.
Assembly raises a stale journal mark to current equity, since a peak reached
before the mark was persisted would otherwise understate later drawdowns.
utc_day_start_ms uses div_euclid so pre-epoch timestamps floor correctly."
```

---

### Task 5: MockExchange test double

**Files:**
- Create: `crates/engine/src/mock.rs`
- Modify: `crates/engine/src/lib.rs`
- Test: `crates/engine/tests/mock_behaviour.rs`

**Interfaces:**
- Consumes: `exchange::ExchangeClient`, `exchange::bybit::transport::ExchangeError`, `exchange::bybit::wire::Ticker`, all `botcore` order/position types
- Produces:
  - `MockExchange::new() -> Self`
  - Builders: `with_balance`, `with_positions`, `with_instruments`, `with_tickers`, `with_klines`, `with_open_orders`
  - Failure injection: `fail_place_entry_once(ExchangeError)`, `fail_place_entry_always(ExchangeError)`
  - Inspection: `placed_orders() -> Vec<LimitEntry>`, `cancelled() -> Vec<String>`, `amended_stops() -> Vec<(Symbol, Decimal, Decimal)>`, `place_entry_call_count() -> usize`

- [ ] **Step 1: Write the failing test**

Create `crates/engine/tests/mock_behaviour.rs`:

```rust
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
        vec![engine::mock::test_candle(0), engine::mock::test_candle(3_600_000)],
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
    let m = MockExchange::new()
        .fail_place_entry_once(ExchangeError::Decode("injected".into()));

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
    let m = MockExchange::new()
        .fail_place_entry_always(ExchangeError::Decode("injected".into()));
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
        m2.place_limit_entry(entry("spawned")).await.expect("placed");
    })
    .await
    .expect("task joined");
    assert_eq!(m.placed_orders().len(), 1);
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p engine --test mock_behaviour`
Expected: FAIL — `MockExchange` not found.

- [ ] **Step 3: Implement**

Create `crates/engine/src/mock.rs`:

```rust
use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use botcore::{
    Balance, Candle, Instrument, LimitEntry, OpenOrder, OrderAck, Position, Symbol, Timeframe,
};
use exchange::ExchangeClient;
use exchange::bybit::transport::ExchangeError;
use exchange::bybit::wire::Ticker;
use rust_decimal::Decimal;

/// Build a simple candle for tests.
pub fn test_candle(open_time_ms: i64) -> Candle {
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

#[derive(Debug, Default)]
struct Recorded {
    placed: Vec<LimitEntry>,
    cancelled: Vec<String>,
    amended: Vec<(Symbol, Decimal, Decimal)>,
    place_entry_calls: usize,
}

/// Programmable failure for `place_limit_entry`.
enum PlaceFailure {
    None,
    Once(ExchangeError),
    Always(ExchangeError),
}

/// An in-memory `ExchangeClient` for driving the execution layer through
/// failure modes a real exchange only produces by accident.
///
/// Uses interior mutability so it can sit behind an `Arc` and still record
/// calls through a shared reference — which is how the executor will hold it.
pub struct MockExchange {
    balance: Balance,
    positions: Vec<Position>,
    instruments: Vec<Instrument>,
    tickers: Vec<Ticker>,
    klines: HashMap<(String, Timeframe), Vec<Candle>>,
    open_orders: Vec<OpenOrder>,
    place_failure: Mutex<PlaceFailure>,
    recorded: Mutex<Recorded>,
}

impl Default for MockExchange {
    fn default() -> Self {
        Self::new()
    }
}

impl MockExchange {
    pub fn new() -> Self {
        MockExchange {
            balance: Balance {
                equity: Decimal::from(10_000),
                available: Decimal::from(9_000),
            },
            positions: Vec::new(),
            instruments: Vec::new(),
            tickers: Vec::new(),
            klines: HashMap::new(),
            open_orders: Vec::new(),
            place_failure: Mutex::new(PlaceFailure::None),
            recorded: Mutex::new(Recorded::default()),
        }
    }

    pub fn with_balance(mut self, balance: Balance) -> Self {
        self.balance = balance;
        self
    }

    pub fn with_positions(mut self, positions: Vec<Position>) -> Self {
        self.positions = positions;
        self
    }

    pub fn with_instruments(mut self, instruments: Vec<Instrument>) -> Self {
        self.instruments = instruments;
        self
    }

    pub fn with_tickers(mut self, tickers: Vec<Ticker>) -> Self {
        self.tickers = tickers;
        self
    }

    pub fn with_klines(mut self, symbol: &Symbol, tf: Timeframe, candles: Vec<Candle>) -> Self {
        self.klines
            .insert((symbol.as_str().to_string(), tf), candles);
        self
    }

    pub fn with_open_orders(mut self, orders: Vec<OpenOrder>) -> Self {
        self.open_orders = orders;
        self
    }

    /// Fail the next `place_limit_entry` only. Models a request that timed out
    /// and will be retried.
    pub fn fail_place_entry_once(self, err: ExchangeError) -> Self {
        *self.place_failure.lock().expect("mock lock") = PlaceFailure::Once(err);
        self
    }

    pub fn fail_place_entry_always(self, err: ExchangeError) -> Self {
        *self.place_failure.lock().expect("mock lock") = PlaceFailure::Always(err);
        self
    }

    pub fn placed_orders(&self) -> Vec<LimitEntry> {
        self.recorded.lock().expect("mock lock").placed.clone()
    }

    pub fn cancelled(&self) -> Vec<String> {
        self.recorded.lock().expect("mock lock").cancelled.clone()
    }

    pub fn amended_stops(&self) -> Vec<(Symbol, Decimal, Decimal)> {
        self.recorded.lock().expect("mock lock").amended.clone()
    }

    pub fn place_entry_call_count(&self) -> usize {
        self.recorded.lock().expect("mock lock").place_entry_calls
    }
}

#[async_trait]
impl ExchangeClient for MockExchange {
    async fn instruments(&self) -> Result<Vec<Instrument>, ExchangeError> {
        Ok(self.instruments.clone())
    }

    async fn tickers(&self) -> Result<Vec<Ticker>, ExchangeError> {
        Ok(self.tickers.clone())
    }

    async fn klines(
        &self,
        symbol: &Symbol,
        tf: Timeframe,
        _limit: u16,
    ) -> Result<Vec<Candle>, ExchangeError> {
        Ok(self
            .klines
            .get(&(symbol.as_str().to_string(), tf))
            .cloned()
            .unwrap_or_default())
    }

    async fn place_limit_entry(&self, req: LimitEntry) -> Result<OrderAck, ExchangeError> {
        {
            let mut rec = self.recorded.lock().expect("mock lock");
            rec.place_entry_calls += 1;
        }

        // A failed placement is deliberately NOT recorded as placed: the
        // execution layer's retry tests depend on distinguishing "the exchange
        // accepted this" from "we asked".
        let mut failure = self.place_failure.lock().expect("mock lock");
        match &*failure {
            PlaceFailure::Always(e) => return Err(clone_error(e)),
            PlaceFailure::Once(e) => {
                let err = clone_error(e);
                *failure = PlaceFailure::None;
                return Err(err);
            }
            PlaceFailure::None => {}
        }
        drop(failure);

        let ack = OrderAck {
            order_id: format!("mock-{}", req.order_link_id),
            order_link_id: req.order_link_id.clone(),
        };
        self.recorded.lock().expect("mock lock").placed.push(req);
        Ok(ack)
    }

    async fn amend_stop(
        &self,
        symbol: &Symbol,
        trigger: Decimal,
        limit_price: Decimal,
    ) -> Result<(), ExchangeError> {
        self.recorded
            .lock()
            .expect("mock lock")
            .amended
            .push((symbol.clone(), trigger, limit_price));
        Ok(())
    }

    async fn cancel_order(&self, _symbol: &Symbol, link_id: &str) -> Result<(), ExchangeError> {
        self.recorded
            .lock()
            .expect("mock lock")
            .cancelled
            .push(link_id.to_string());
        Ok(())
    }

    async fn positions(&self) -> Result<Vec<Position>, ExchangeError> {
        Ok(self.positions.clone())
    }

    async fn open_orders(&self) -> Result<Vec<OpenOrder>, ExchangeError> {
        Ok(self.open_orders.clone())
    }

    async fn set_leverage(&self, _symbol: &Symbol, _leverage: Decimal) -> Result<(), ExchangeError> {
        Ok(())
    }

    async fn balance(&self) -> Result<Balance, ExchangeError> {
        Ok(self.balance.clone())
    }
}

/// `ExchangeError` is not `Clone` (its `Http` variant wraps a `reqwest::Error`),
/// so injected failures are reproduced by variant rather than cloned.
fn clone_error(e: &ExchangeError) -> ExchangeError {
    match e {
        ExchangeError::Api { code, msg } => ExchangeError::Api {
            code: *code,
            msg: msg.clone(),
        },
        ExchangeError::Decode(m) => ExchangeError::Decode(m.clone()),
        ExchangeError::WebSocket(m) => ExchangeError::WebSocket(m.clone()),
        other => ExchangeError::Decode(format!("injected: {other}")),
    }
}
```

Add to `crates/engine/src/lib.rs`:

```rust
pub mod mock;
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p engine --test mock_behaviour`
Expected: PASS — 7 tests.

- [ ] **Step 5: Run the whole engine suite and lint**

Run: `cargo test -p engine && cargo clippy -p engine --all-targets -- -D warnings`
Expected: PASS — 41 tests, no clippy warnings.

- [ ] **Step 6: Verify the limit-only guard still passes with the new crate in scope**

Run: `cargo test -p exchange --test no_market_orders`
Expected: PASS — the source-grep walks `crates/`, which now includes `engine`.

- [ ] **Step 7: Commit**

```bash
git add crates/engine
git commit -m "feat(engine): MockExchange test double with failure injection

Implements ExchangeClient in memory so the execution layer can be driven
through failure modes a real exchange only produces by accident. A failed
placement is deliberately not recorded as placed, so retry tests can
distinguish 'the exchange accepted this' from 'we asked' — the distinction the
whole orderLinkId idempotency scheme rests on. Interior mutability lets it
record calls from behind an Arc, which is how the executor will hold it."
```

---

## Plan Self-Review

**1. Spec coverage.** Mapping the approved spec to tasks:

| Spec requirement | Task |
|---|---|
| §5 invariant 6 — deterministic `orderLinkId`, ≤36 chars | 1 |
| §5 invariant 10 — feed staleness blocks entries | 2 |
| §7.2 step 7 — indicator warmup after restart | 2 |
| §4.3 — gap detection before resuming | 2 |
| §6 — daily universe re-rank, turnover floor, listing age | 3 |
| §6 — symbols with a position never dropped mid-trade | 3 |
| §5 — daily counters and drawdown baselines at 00:00 UTC | 4 |
| §5 — high-water mark persisted, drawdown measured from it | 4 |
| §9 — `MockExchange` for engine integration tests | 5 |

**Deferred to Plan 1d (execution layer), by design:** §4.3 entry expiry and the stop-escalation ladder, §5 invariants 1 and 2 (stop attached at entry, no market orders at the client — already enforced in `exchange`), §7.2 startup reconciliation, `OrderTracker`, `Executor`, `Reconciler`, and the live loop that replaces the probe binary.

**2. Placeholder scan.** No TBDs, no "add error handling", no "similar to Task N". Every code step carries complete code.

**3. Type consistency.** `Acceptance::Gap { missing: i64 }` matches the `i64` arithmetic in `accept`. `UniverseFilter` field names match their uses in `select_universe`. `JournalFacts` field names match `AccountState`'s. `MockExchange`'s trait impl signatures were taken from the existing `ExchangeClient` definition — `klines` takes `u16`, `amend_stop` takes two `Decimal`s, `cancel_order` takes `&str`. `test_candle` is `pub` because the integration test in Task 5 calls it as `engine::mock::test_candle`.

**One thing stated rather than hidden:** `MockExchange::fail_place_entry_once` takes `self` by value (builder style) but mutates through a `Mutex`, which is slightly unusual. It is done that way so failure injection composes with the other `with_*` builders in a single expression. The alternative — a `&self` setter — would force two statements at every call site.

---

## Execution Handoff

Plan 1c covers 5 tasks producing the `engine` crate's deterministic core plus the test double Plan 1d depends on.

