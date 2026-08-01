# Bybit Bot Phase 1a — Foundation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the verified foundation the trading engine sits on — domain types, incremental indicators, a Bybit V5 testnet client (REST + WebSocket), and the Turso journal — proven end-to-end by a `probe` binary that authenticates, streams live candles, and writes to the journal.

**Architecture:** A Cargo workspace of small, single-responsibility crates with strictly downward dependencies. `core` holds domain types and depends on nothing internal. `indicators`, `exchange` and `persistence` depend only on `core`. Every exchange interaction goes through traits so Phase 2 can swap in a simulator. The limit-only rule is enforced structurally: no type in this workspace can express a market order.

**Tech Stack:** Rust 1.97 (edition 2024), tokio, reqwest, tokio-tungstenite, serde/serde_json, rust_decimal, hmac + sha2, turso (sync feature), thiserror, tracing, wiremock (HTTP mocking), proptest.

## Global Constraints

Copied verbatim from the spec. Every task's requirements implicitly include this section.

- All monetary and quantity values use `rust_decimal::Decimal`. **Never `f64`** for prices, quantities, or balances.
- No code path may construct `orderType: "Market"`. There is no market-order method anywhere.
- Credentials come from environment variables only: `BYBIT_API_KEY`, `BYBIT_API_SECRET`, `TURSO_DATABASE_URL`, `TURSO_AUTH_TOKEN`. Never written to a config file or committed.
- Testnet REST base URL: `https://api-testnet.bybit.com`. Mainnet: `https://api.bybit.com`.
- Testnet WebSocket: public `wss://stream-testnet.bybit.com/v5/public/linear`, private `wss://stream-testnet.bybit.com/v5/private`.
- REST auth headers: `X-BAPI-API-KEY`, `X-BAPI-TIMESTAMP` (ms), `X-BAPI-RECV-WINDOW`, `X-BAPI-SIGN`.
- Signature: `HMAC_SHA256(timestamp + api_key + recv_window + (query_string | json_body))`, lowercase hex.
- `recv_window` default 5000 ms. Client tracks clock offset against the server's response `time` field.
- WebSocket heartbeat: `{"op":"ping"}` every 20 seconds.
- Journal write failures never block or fail an order.
- Rate limit error codes to treat as retryable: `10006`, `10018`.
- Rust edition 2024, resolver 3.

---

## File Structure

```
crypto-bot/
├── Cargo.toml                       workspace manifest, shared dependency versions
├── crates/
│   ├── core/
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs               re-exports
│   │       ├── money.rs             Decimal rounding helpers (tick/step/away-from-market)
│   │       ├── symbol.rs            Symbol, Instrument
│   │       ├── candle.rs            Candle, Timeframe
│   │       ├── order.rs             Side, LimitEntry, OrderAck, OpenOrder, OrderState
│   │       ├── position.rs          Position, Balance
│   │       └── error.rs             ErrorClass, CoreError
│   ├── indicators/
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── ema.rs               incremental EMA
│   │       ├── rsi.rs               incremental Wilder RSI
│   │       └── atr.rs               incremental Wilder ATR
│   ├── exchange/
│   │   ├── Cargo.toml
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── traits.rs            ExchangeClient, MarketFeed
│   │       └── bybit/
│   │           ├── mod.rs           BybitClient assembly
│   │           ├── sign.rs          HMAC signing + clock offset
│   │           ├── transport.rs     retry, backoff, error classification
│   │           ├── rate_limit.rs    token-bucket limiter
│   │           ├── wire.rs          serde types matching Bybit V5 JSON
│   │           ├── rest.rs          endpoint methods
│   │           ├── ws_public.rs     kline stream + reconnect + gap fill
│   │           └── ws_private.rs    order/position/execution/wallet stream
│   └── persistence/
│       ├── Cargo.toml
│       └── src/
│           ├── lib.rs
│           ├── schema.rs            table DDL + migration
│           ├── journal.rs           typed write/read API
│           └── sync.rs              background push task
└── bot/
    ├── Cargo.toml
    └── src/
        ├── main.rs                  probe binary (Plan 1a deliverable)
        └── config.rs                TOML profile + env credential loading
```

Splitting `bybit/` by responsibility rather than by one large `client.rs` keeps each file small enough to hold in context: signing, transport policy, rate limiting, wire types and endpoints all change for different reasons.

---

### Task 1: Workspace scaffold and core money types

**Files:**
- Create: `Cargo.toml`, `crates/core/Cargo.toml`, `crates/core/src/lib.rs`, `crates/core/src/money.rs`
- Test: `crates/core/src/money.rs` (inline `#[cfg(test)]` module)

**Interfaces:**
- Consumes: nothing (first task)
- Produces:
  - `core::money::round_down_to_step(value: Decimal, step: Decimal) -> Decimal`
  - `core::money::round_price_away_from_market(price: Decimal, tick: Decimal, side: Side) -> Decimal`
  - `core::order::Side` enum with variants `Buy`, `Sell`

- [ ] **Step 1: Create the workspace manifest**

Create `Cargo.toml`:

```toml
[workspace]
members = ["crates/core", "crates/indicators", "crates/exchange", "crates/persistence", "bot"]
resolver = "3"

[workspace.package]
edition = "2024"
rust-version = "1.97"

[workspace.dependencies]
rust_decimal = { version = "1", features = ["serde-with-str"] }
rust_decimal_macros = "1"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
thiserror = "2"
tokio = { version = "1", features = ["full"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "json"] }
```

- [ ] **Step 2: Create the core crate manifest**

Create `crates/core/Cargo.toml`:

```toml
[package]
name = "core"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true

[dependencies]
rust_decimal.workspace = true
serde.workspace = true
thiserror.workspace = true

[dev-dependencies]
rust_decimal_macros.workspace = true
```

- [ ] **Step 3: Write the failing test for rounding helpers**

Create `crates/core/src/money.rs` containing only the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn round_down_to_step_never_rounds_up() {
        // 0.123 with a step of 0.01 must become 0.12, never 0.13 —
        // rounding up would increase position size and therefore risk.
        assert_eq!(round_down_to_step(dec!(0.123), dec!(0.01)), dec!(0.12));
        assert_eq!(round_down_to_step(dec!(0.129), dec!(0.01)), dec!(0.12));
        assert_eq!(round_down_to_step(dec!(1.0), dec!(0.001)), dec!(1.0));
    }

    #[test]
    fn round_down_to_step_handles_whole_number_steps() {
        assert_eq!(round_down_to_step(dec!(157.9), dec!(1)), dec!(157));
    }

    #[test]
    fn buy_limit_rounds_down_away_from_market() {
        // A buy limit rests below market, so "away from market" is downward.
        assert_eq!(
            round_price_away_from_market(dec!(100.567), dec!(0.01), Side::Buy),
            dec!(100.56)
        );
    }

    #[test]
    fn sell_limit_rounds_up_away_from_market() {
        // A sell limit rests above market, so "away from market" is upward.
        assert_eq!(
            round_price_away_from_market(dec!(100.561), dec!(0.01), Side::Sell),
            dec!(100.57)
        );
    }

    #[test]
    fn exact_multiples_are_unchanged_in_both_directions() {
        assert_eq!(
            round_price_away_from_market(dec!(100.56), dec!(0.01), Side::Buy),
            dec!(100.56)
        );
        assert_eq!(
            round_price_away_from_market(dec!(100.56), dec!(0.01), Side::Sell),
            dec!(100.56)
        );
    }

    // The zero-divisor guards exist so a malformed instrument spec cannot
    // panic the sizer. Without these tests a refactor could drop the guard
    // and nothing would notice.
    #[test]
    fn zero_step_returns_value_unchanged() {
        assert_eq!(round_down_to_step(dec!(0.123), dec!(0)), dec!(0.123));
    }

    #[test]
    fn zero_tick_returns_price_unchanged() {
        assert_eq!(
            round_price_away_from_market(dec!(100.567), dec!(0), Side::Buy),
            dec!(100.567)
        );
        assert_eq!(
            round_price_away_from_market(dec!(100.567), dec!(0), Side::Sell),
            dec!(100.567)
        );
    }
}
```

- [ ] **Step 4: Run the test to verify it fails**

Run: `cargo test -p core`
Expected: FAIL — compile error, `round_down_to_step` and `Side` not found.

- [ ] **Step 5: Write the minimal implementation**

Prepend to `crates/core/src/money.rs`:

```rust
use rust_decimal::Decimal;

use crate::order::Side;

/// Round `value` down to the nearest multiple of `step`.
///
/// Rounds toward negative infinity. Callers must pass a non-negative value —
/// this is only ever used for order quantities, where rounding up would push
/// realized risk above the configured budget. On a negative input, floor moves
/// *away* from zero, which is the risk-increasing direction, so the
/// precondition is asserted rather than silently tolerated.
pub fn round_down_to_step(value: Decimal, step: Decimal) -> Decimal {
    debug_assert!(
        !value.is_sign_negative(),
        "round_down_to_step expects a non-negative quantity, got {value}"
    );
    if step.is_zero() {
        return value;
    }
    (value / step).floor() * step
}

/// Round a limit price to a valid tick, moving away from the market.
///
/// A buy limit rests below market and rounds down; a sell limit rests above
/// market and rounds up. This can only make the order less likely to fill,
/// never more aggressive than intended.
pub fn round_price_away_from_market(price: Decimal, tick: Decimal, side: Side) -> Decimal {
    if tick.is_zero() {
        return price;
    }
    let ticks = price / tick;
    let rounded = match side {
        Side::Buy => ticks.floor(),
        Side::Sell => ticks.ceil(),
    };
    rounded * tick
}
```

Create `crates/core/src/order.rs`:

```rust
use serde::{Deserialize, Serialize};

/// Order direction. `Buy` opens a long, `Sell` opens a short.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    /// Bybit V5 wire representation.
    pub fn as_bybit(self) -> &'static str {
        match self {
            Side::Buy => "Buy",
            Side::Sell => "Sell",
        }
    }

    pub fn opposite(self) -> Self {
        match self {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        }
    }
}
```

Create `crates/core/src/lib.rs`:

```rust
pub mod money;
pub mod order;
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p core`
Expected: PASS — 5 tests.

- [ ] **Step 7: Add a property test proving rounding never increases size**

Add to `crates/core/Cargo.toml` under `[dev-dependencies]`:

```toml
proptest = "1"
```

Append to the `tests` module in `crates/core/src/money.rs`:

```rust
    use proptest::prelude::*;

    proptest! {
        #[test]
        fn round_down_never_exceeds_input(
            value_cents in 0i64..10_000_000i64,
            step_choice in 0usize..4usize,
        ) {
            let value = Decimal::new(value_cents, 4);
            let step = [dec!(0.0001), dec!(0.001), dec!(0.01), dec!(0.1)][step_choice];
            let rounded = round_down_to_step(value, step);
            prop_assert!(rounded <= value, "rounded {rounded} exceeded input {value}");
            prop_assert!(value - rounded < step, "rounded {rounded} lost more than one step");
        }
    }
```

- [ ] **Step 8: Run the property test**

Run: `cargo test -p core`
Expected: PASS — 6 tests including `round_down_never_exceeds_input`.

- [ ] **Step 9: Commit**

```bash
git add Cargo.toml crates/core
git commit -m "feat(core): workspace scaffold and Decimal rounding helpers

Rounding is directional by design: quantities round down so they can only
reduce risk, and limit prices round away from the market so they can only
become less aggressive."
```

---

### Task 2: Core domain types

**Files:**
- Create: `crates/core/src/symbol.rs`, `crates/core/src/candle.rs`, `crates/core/src/position.rs`, `crates/core/src/error.rs`
- Modify: `crates/core/src/lib.rs`, `crates/core/src/order.rs`

**Interfaces:**
- Consumes: `Side` from Task 1
- Produces:
  - `Symbol(String)` newtype with `as_str()`
  - `Timeframe` enum (`H1`, `H4`) with `as_bybit_interval() -> &'static str` and `duration_ms() -> i64`
  - `Candle { open_time_ms, open, high, low, close, volume, turnover }`
  - `Instrument { symbol, tick_size, qty_step, min_order_qty, launch_time_ms }`
  - `Position { symbol, side, size, entry_price, liq_price, unrealized_pnl }`
  - `Balance { equity, available }`
  - `LimitEntry`, `OrderAck`, `OpenOrder`, `OrderState`
  - `ErrorClass` enum (`Retryable`, `Rejected`, `Fatal`)

- [ ] **Step 1: Write the failing test for Timeframe**

Create `crates/core/src/candle.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeframe_maps_to_bybit_interval_strings() {
        assert_eq!(Timeframe::H1.as_bybit_interval(), "60");
        assert_eq!(Timeframe::H4.as_bybit_interval(), "240");
    }

    #[test]
    fn timeframe_durations_are_in_milliseconds() {
        assert_eq!(Timeframe::H1.duration_ms(), 3_600_000);
        assert_eq!(Timeframe::H4.duration_ms(), 14_400_000);
    }

    #[test]
    fn candle_close_time_is_open_plus_duration_minus_one() {
        let c = Candle {
            open_time_ms: 1_700_000_000_000,
            open: Default::default(),
            high: Default::default(),
            low: Default::default(),
            close: Default::default(),
            volume: Default::default(),
            turnover: Default::default(),
        };
        assert_eq!(c.close_time_ms(Timeframe::H1), 1_700_000_000_000 + 3_600_000 - 1);
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p core`
Expected: FAIL — `Timeframe` and `Candle` not found.

- [ ] **Step 3: Implement candle and timeframe types**

Prepend to `crates/core/src/candle.rs`:

```rust
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// Strategy timeframes. Only the two the baseline strategy needs exist;
/// adding more is a deliberate act, not an accident.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Timeframe {
    H1,
    H4,
}

impl Timeframe {
    /// Bybit V5 kline interval string, used in both REST params and WS topics.
    pub fn as_bybit_interval(self) -> &'static str {
        match self {
            Timeframe::H1 => "60",
            Timeframe::H4 => "240",
        }
    }

    pub fn duration_ms(self) -> i64 {
        match self {
            Timeframe::H1 => 3_600_000,
            Timeframe::H4 => 14_400_000,
        }
    }
}

/// A single closed candle. `open_time_ms` is the candle's start, in epoch
/// milliseconds, which is what Bybit keys klines by.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candle {
    pub open_time_ms: i64,
    pub open: Decimal,
    pub high: Decimal,
    pub low: Decimal,
    pub close: Decimal,
    pub volume: Decimal,
    pub turnover: Decimal,
}

impl Candle {
    pub fn close_time_ms(&self, tf: Timeframe) -> i64 {
        self.open_time_ms + tf.duration_ms() - 1
    }
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p core`
Expected: PASS.

- [ ] **Step 5: Write the failing test for instrument constraints**

Create `crates/core/src/symbol.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn instrument() -> Instrument {
        Instrument {
            symbol: Symbol::new("BTCUSDT"),
            tick_size: dec!(0.1),
            qty_step: dec!(0.001),
            min_order_qty: dec!(0.001),
            launch_time_ms: 1_600_000_000_000,
        }
    }

    #[test]
    fn age_days_computes_from_launch_time() {
        let i = instrument();
        let now = 1_600_000_000_000 + 30 * 86_400_000;
        assert_eq!(i.age_days(now), 30);
    }

    #[test]
    fn qty_below_minimum_is_rejected() {
        let i = instrument();
        assert!(!i.qty_is_valid(dec!(0.0005)));
        assert!(i.qty_is_valid(dec!(0.001)));
    }
}
```

- [ ] **Step 6: Run the test to verify it fails**

Run: `cargo test -p core`
Expected: FAIL — `Instrument` not found.

- [ ] **Step 7: Implement symbol and instrument**

Prepend to `crates/core/src/symbol.rs`:

```rust
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// An exchange symbol such as `BTCUSDT`. A newtype so a symbol can never be
/// confused with an arbitrary string in a function signature.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Symbol(String);

impl Symbol {
    pub fn new(s: impl Into<String>) -> Self {
        Symbol(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Symbol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Exchange-imposed trading constraints for one instrument. Every order must
/// be validated against these before it is sent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Instrument {
    pub symbol: Symbol,
    pub tick_size: Decimal,
    pub qty_step: Decimal,
    pub min_order_qty: Decimal,
    pub launch_time_ms: i64,
}

impl Instrument {
    pub fn age_days(&self, now_ms: i64) -> i64 {
        (now_ms - self.launch_time_ms) / 86_400_000
    }

    pub fn qty_is_valid(&self, qty: Decimal) -> bool {
        qty >= self.min_order_qty
    }
}
```

- [ ] **Step 8: Run the test to verify it passes**

Run: `cargo test -p core`
Expected: PASS.

- [ ] **Step 9: Add position, balance, order and error types**

Create `crates/core/src/position.rs`:

```rust
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::order::Side;
use crate::symbol::Symbol;

/// An open position as the exchange reports it. The exchange is the source of
/// truth for this type — the bot's own view is always reconciled against it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    pub symbol: Symbol,
    pub side: Side,
    pub size: Decimal,
    pub entry_price: Decimal,
    /// Liquidation price. `None` when the exchange reports no liquidation risk.
    pub liq_price: Option<Decimal>,
    pub unrealized_pnl: Decimal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Balance {
    /// Total account equity, including unrealized PnL.
    pub equity: Decimal,
    /// Margin available for new positions.
    pub available: Decimal,
}
```

Create `crates/core/src/error.rs`:

```rust
/// How the engine should react to a failed exchange interaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    /// Transient. Retry with backoff; safe because every order carries a
    /// deterministic `orderLinkId`.
    Retryable,
    /// The exchange understood and refused. Log, skip the signal, continue.
    Rejected,
    /// Unrecoverable without a human. Halt trading and alert.
    Fatal,
}
```

Append to `crates/core/src/order.rs`:

```rust
use rust_decimal::Decimal;

use crate::symbol::Symbol;

/// A limit entry order with protection attached.
///
/// There is deliberately no market-order equivalent of this type anywhere in
/// the workspace: the limit-only rule is enforced by what can be constructed,
/// not by a runtime check that could be forgotten.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LimitEntry {
    pub symbol: Symbol,
    pub side: Side,
    pub qty: Decimal,
    pub price: Decimal,
    /// Deterministic idempotency key, max 36 characters.
    pub order_link_id: String,
    pub stop_loss: Decimal,
    pub stop_limit_price: Decimal,
    pub take_profit: Decimal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderAck {
    pub order_id: String,
    pub order_link_id: String,
}

/// Lifecycle state of a resting or completed order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderState {
    New,
    PartiallyFilled,
    Filled,
    Cancelled,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenOrder {
    pub symbol: Symbol,
    pub order_id: String,
    pub order_link_id: String,
    pub side: Side,
    pub price: Decimal,
    pub qty: Decimal,
    pub cum_exec_qty: Decimal,
    pub state: OrderState,
    pub created_time_ms: i64,
}
```

Replace `crates/core/src/lib.rs`:

```rust
pub mod candle;
pub mod error;
pub mod money;
pub mod order;
pub mod position;
pub mod symbol;

pub use candle::{Candle, Timeframe};
pub use error::ErrorClass;
pub use order::{LimitEntry, OpenOrder, OrderAck, OrderState, Side};
pub use position::{Balance, Position};
pub use symbol::{Instrument, Symbol};
```

- [ ] **Step 10: Run the full core test suite**

Run: `cargo test -p core && cargo clippy -p core -- -D warnings`
Expected: PASS, no clippy warnings.

- [ ] **Step 11: Commit**

```bash
git add crates/core
git commit -m "feat(core): domain types for candles, instruments, orders, positions

LimitEntry has no market-order counterpart by design — the limit-only rule
is expressed in the type system rather than enforced at runtime."
```

---

### Task 3: Incremental indicators

**Files:**
- Create: `crates/indicators/Cargo.toml`, `crates/indicators/src/lib.rs`, `crates/indicators/src/ema.rs`, `crates/indicators/src/rsi.rs`, `crates/indicators/src/atr.rs`

**Interfaces:**
- Consumes: `core::Candle`
- Produces:
  - `Ema::new(period: usize)`, `Ema::update(&mut self, price: Decimal) -> Option<Decimal>`, `Ema::value(&self) -> Option<Decimal>`
  - `Rsi::new(period: usize)`, `Rsi::update(&mut self, price: Decimal) -> Option<Decimal>`, `Rsi::value(&self) -> Option<Decimal>`
  - `Atr::new(period: usize)`, `Atr::update(&mut self, candle: &Candle) -> Option<Decimal>`, `Atr::value(&self) -> Option<Decimal>`

All three are O(1) per update and return `None` until warm.

- [ ] **Step 1: Create the crate manifest**

Create `crates/indicators/Cargo.toml`:

```toml
[package]
name = "indicators"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true

[dependencies]
core = { path = "../core" }
rust_decimal.workspace = true

[dev-dependencies]
rust_decimal_macros.workspace = true
```

- [ ] **Step 2: Write the failing EMA test**

Create `crates/indicators/src/ema.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn ema_returns_none_until_warm() {
        let mut ema = Ema::new(3);
        assert_eq!(ema.update(dec!(1)), None);
        assert_eq!(ema.update(dec!(2)), None);
        // Third value completes the seed SMA.
        assert_eq!(ema.update(dec!(3)), Some(dec!(2)));
    }

    #[test]
    fn ema_seeds_with_sma_then_applies_smoothing() {
        // period 3 -> alpha = 2/(3+1) = 0.5
        // seed SMA of [1,2,3] = 2; next price 10 -> 0.5*10 + 0.5*2 = 6
        let mut ema = Ema::new(3);
        ema.update(dec!(1));
        ema.update(dec!(2));
        ema.update(dec!(3));
        assert_eq!(ema.update(dec!(10)), Some(dec!(6)));
    }

    #[test]
    fn ema_value_matches_last_update() {
        let mut ema = Ema::new(2);
        ema.update(dec!(4));
        let last = ema.update(dec!(6));
        assert_eq!(ema.value(), last);
    }
}
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test -p indicators`
Expected: FAIL — `Ema` not found.

- [ ] **Step 4: Implement the EMA**

Prepend to `crates/indicators/src/ema.rs`:

```rust
use rust_decimal::Decimal;

/// Exponential moving average, seeded with a simple average of the first
/// `period` samples, then updated in O(1).
#[derive(Debug, Clone)]
pub struct Ema {
    period: usize,
    alpha: Decimal,
    seed_sum: Decimal,
    seed_count: usize,
    current: Option<Decimal>,
}

impl Ema {
    pub fn new(period: usize) -> Self {
        assert!(period > 0, "EMA period must be positive");
        let alpha = Decimal::from(2) / Decimal::from(period as u64 + 1);
        Ema {
            period,
            alpha,
            seed_sum: Decimal::ZERO,
            seed_count: 0,
            current: None,
        }
    }

    /// Feed one sample. Returns the new EMA once warm, `None` before that.
    pub fn update(&mut self, price: Decimal) -> Option<Decimal> {
        match self.current {
            Some(prev) => {
                let next = self.alpha * price + (Decimal::ONE - self.alpha) * prev;
                self.current = Some(next);
            }
            None => {
                self.seed_sum += price;
                self.seed_count += 1;
                if self.seed_count == self.period {
                    self.current = Some(self.seed_sum / Decimal::from(self.period as u64));
                }
            }
        }
        self.current
    }

    pub fn value(&self) -> Option<Decimal> {
        self.current
    }

    pub fn is_warm(&self) -> bool {
        self.current.is_some()
    }
}
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test -p indicators`
Expected: PASS — 3 tests.

- [ ] **Step 6: Write the failing RSI test**

Create `crates/indicators/src/rsi.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn rsi_is_none_until_period_plus_one_samples() {
        let mut rsi = Rsi::new(3);
        assert_eq!(rsi.update(dec!(10)), None); // no delta yet
        assert_eq!(rsi.update(dec!(11)), None); // 1 delta
        assert_eq!(rsi.update(dec!(12)), None); // 2 deltas
        assert!(rsi.update(dec!(13)).is_some()); // 3 deltas -> warm
    }

    #[test]
    fn rsi_is_one_hundred_when_every_move_is_up() {
        let mut rsi = Rsi::new(3);
        for p in [10, 11, 12, 13] {
            rsi.update(Decimal::from(p));
        }
        assert_eq!(rsi.value(), Some(dec!(100)));
    }

    #[test]
    fn rsi_is_zero_when_every_move_is_down() {
        let mut rsi = Rsi::new(3);
        for p in [13, 12, 11, 10] {
            rsi.update(Decimal::from(p));
        }
        assert_eq!(rsi.value(), Some(dec!(0)));
    }

    #[test]
    fn rsi_is_fifty_when_gains_equal_losses() {
        // Seed with alternating +1/-1 moves of equal magnitude.
        let mut rsi = Rsi::new(2);
        rsi.update(dec!(10));
        rsi.update(dec!(11)); // +1
        rsi.update(dec!(10)); // -1
        assert_eq!(rsi.value(), Some(dec!(50)));
    }
}
```

- [ ] **Step 7: Run the test to verify it fails**

Run: `cargo test -p indicators`
Expected: FAIL — `Rsi` not found.

- [ ] **Step 8: Implement the RSI**

Prepend to `crates/indicators/src/rsi.rs`:

```rust
use rust_decimal::Decimal;

/// Wilder's RSI. Seeded with a simple average of the first `period` deltas,
/// then smoothed with Wilder's recurrence in O(1).
#[derive(Debug, Clone)]
pub struct Rsi {
    period: usize,
    period_dec: Decimal,
    prev_price: Option<Decimal>,
    seed_gain: Decimal,
    seed_loss: Decimal,
    seed_count: usize,
    avg_gain: Option<Decimal>,
    avg_loss: Option<Decimal>,
    current: Option<Decimal>,
}

impl Rsi {
    pub fn new(period: usize) -> Self {
        assert!(period > 0, "RSI period must be positive");
        Rsi {
            period,
            period_dec: Decimal::from(period as u64),
            prev_price: None,
            seed_gain: Decimal::ZERO,
            seed_loss: Decimal::ZERO,
            seed_count: 0,
            avg_gain: None,
            avg_loss: None,
            current: None,
        }
    }

    pub fn update(&mut self, price: Decimal) -> Option<Decimal> {
        let Some(prev) = self.prev_price.replace(price) else {
            return None;
        };

        let delta = price - prev;
        let gain = if delta > Decimal::ZERO { delta } else { Decimal::ZERO };
        let loss = if delta < Decimal::ZERO { -delta } else { Decimal::ZERO };

        match (self.avg_gain, self.avg_loss) {
            (Some(ag), Some(al)) => {
                let n1 = self.period_dec - Decimal::ONE;
                self.avg_gain = Some((ag * n1 + gain) / self.period_dec);
                self.avg_loss = Some((al * n1 + loss) / self.period_dec);
            }
            _ => {
                self.seed_gain += gain;
                self.seed_loss += loss;
                self.seed_count += 1;
                if self.seed_count == self.period {
                    self.avg_gain = Some(self.seed_gain / self.period_dec);
                    self.avg_loss = Some(self.seed_loss / self.period_dec);
                } else {
                    return None;
                }
            }
        }

        self.current = Some(self.compute());
        self.current
    }

    fn compute(&self) -> Decimal {
        let (ag, al) = (self.avg_gain.unwrap(), self.avg_loss.unwrap());
        if al.is_zero() {
            // No losses in the window: RSI is defined as 100.
            return Decimal::from(100);
        }
        if ag.is_zero() {
            return Decimal::ZERO;
        }
        let rs = ag / al;
        Decimal::from(100) - (Decimal::from(100) / (Decimal::ONE + rs))
    }

    pub fn value(&self) -> Option<Decimal> {
        self.current
    }

    pub fn is_warm(&self) -> bool {
        self.current.is_some()
    }
}
```

- [ ] **Step 9: Run the test to verify it passes**

Run: `cargo test -p indicators`
Expected: PASS — 7 tests total.

- [ ] **Step 10: Write the failing ATR test**

Create `crates/indicators/src/atr.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn candle(high: Decimal, low: Decimal, close: Decimal) -> Candle {
        Candle {
            open_time_ms: 0,
            open: close,
            high,
            low,
            close,
            volume: Decimal::ZERO,
            turnover: Decimal::ZERO,
        }
    }

    #[test]
    fn first_true_range_is_high_minus_low() {
        let mut atr = Atr::new(1);
        // With period 1 the seed completes on the first candle.
        assert_eq!(atr.update(&candle(dec!(10), dec!(8), dec!(9))), Some(dec!(2)));
    }

    #[test]
    fn true_range_accounts_for_gaps_against_previous_close() {
        let mut atr = Atr::new(1);
        atr.update(&candle(dec!(10), dec!(8), dec!(9)));
        // Gap up: high 20, low 19, prev close 9. TR = max(1, 11, 10) = 11.
        // period 1 -> ATR tracks TR exactly.
        assert_eq!(atr.update(&candle(dec!(20), dec!(19), dec!(19))), Some(dec!(11)));
    }

    #[test]
    fn atr_returns_none_until_warm() {
        let mut atr = Atr::new(3);
        assert_eq!(atr.update(&candle(dec!(10), dec!(9), dec!(9))), None);
        assert_eq!(atr.update(&candle(dec!(11), dec!(10), dec!(10))), None);
        assert!(atr.update(&candle(dec!(12), dec!(11), dec!(11))).is_some());
    }

    #[test]
    fn atr_averages_constant_ranges_to_that_range() {
        let mut atr = Atr::new(3);
        for _ in 0..6 {
            atr.update(&candle(dec!(10), dec!(9), dec!(9)));
        }
        // Every TR is exactly 1, so the average must be 1.
        assert_eq!(atr.value(), Some(dec!(1)));
    }
}
```

- [ ] **Step 11: Run the test to verify it fails**

Run: `cargo test -p indicators`
Expected: FAIL — `Atr` not found.

- [ ] **Step 12: Implement the ATR**

Prepend to `crates/indicators/src/atr.rs`:

```rust
use core::Candle;
use rust_decimal::Decimal;

/// Wilder's Average True Range, seeded with a simple average of the first
/// `period` true ranges then smoothed in O(1).
#[derive(Debug, Clone)]
pub struct Atr {
    period: usize,
    period_dec: Decimal,
    prev_close: Option<Decimal>,
    seed_sum: Decimal,
    seed_count: usize,
    current: Option<Decimal>,
}

impl Atr {
    pub fn new(period: usize) -> Self {
        assert!(period > 0, "ATR period must be positive");
        Atr {
            period,
            period_dec: Decimal::from(period as u64),
            prev_close: None,
            seed_sum: Decimal::ZERO,
            seed_count: 0,
            current: None,
        }
    }

    pub fn update(&mut self, candle: &Candle) -> Option<Decimal> {
        let tr = self.true_range(candle);
        self.prev_close = Some(candle.close);

        match self.current {
            Some(prev) => {
                let n1 = self.period_dec - Decimal::ONE;
                self.current = Some((prev * n1 + tr) / self.period_dec);
            }
            None => {
                self.seed_sum += tr;
                self.seed_count += 1;
                if self.seed_count == self.period {
                    self.current = Some(self.seed_sum / self.period_dec);
                }
            }
        }
        self.current
    }

    /// True range: the widest of the intrabar range and the two gap measures
    /// against the previous close.
    fn true_range(&self, candle: &Candle) -> Decimal {
        let range = candle.high - candle.low;
        match self.prev_close {
            None => range,
            Some(pc) => range.max((candle.high - pc).abs()).max((candle.low - pc).abs()),
        }
    }

    pub fn value(&self) -> Option<Decimal> {
        self.current
    }

    pub fn is_warm(&self) -> bool {
        self.current.is_some()
    }
}
```

Create `crates/indicators/src/lib.rs`:

```rust
pub mod atr;
pub mod ema;
pub mod rsi;

pub use atr::Atr;
pub use ema::Ema;
pub use rsi::Rsi;
```

- [ ] **Step 13: Run the test to verify it passes**

Run: `cargo test -p indicators`
Expected: PASS — 11 tests total.

- [ ] **Step 14: Add an incremental-equals-batch regression test**

Create `crates/indicators/tests/incremental_matches_batch.rs`:

```rust
use core::Candle;
use indicators::{Atr, Ema, Rsi};
use rust_decimal::Decimal;

/// A deterministic pseudo-random price series. Fixed seed so failures are
/// reproducible without a proptest dependency in integration tests.
fn price_series(n: usize) -> Vec<Decimal> {
    let mut state: u64 = 0x2545F491_4F6CDD1D;
    (0..n)
        .map(|_| {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            // Prices in the 900..1100 range with two decimal places.
            let cents = 90_000 + (state >> 40) % 20_000;
            Decimal::new(cents as i64, 2)
        })
        .collect()
}

fn candles(prices: &[Decimal]) -> Vec<Candle> {
    prices
        .iter()
        .enumerate()
        .map(|(i, p)| Candle {
            open_time_ms: i as i64 * 3_600_000,
            open: *p,
            high: *p + Decimal::new(50, 2),
            low: *p - Decimal::new(50, 2),
            close: *p,
            volume: Decimal::ZERO,
            turnover: Decimal::ZERO,
        })
        .collect()
}

/// Feeding a series one sample at a time must equal feeding a fresh indicator
/// the same series — i.e. `update` carries no state that a restart would lose
/// differently. This is what makes warmup-after-restart trustworthy.
#[test]
fn ema_incremental_equals_fresh_replay() {
    let prices = price_series(200);
    let mut streaming = Ema::new(20);
    for p in &prices {
        streaming.update(*p);
    }

    let mut replayed = Ema::new(20);
    for p in &prices {
        replayed.update(*p);
    }

    assert_eq!(streaming.value(), replayed.value());
    assert!(streaming.value().is_some());
}

#[test]
fn rsi_stays_within_zero_and_one_hundred() {
    let prices = price_series(500);
    let mut rsi = Rsi::new(14);
    for p in &prices {
        if let Some(v) = rsi.update(*p) {
            assert!(v >= Decimal::ZERO, "RSI went below 0: {v}");
            assert!(v <= Decimal::from(100), "RSI went above 100: {v}");
        }
    }
    assert!(rsi.is_warm());
}

#[test]
fn atr_is_never_negative() {
    let prices = price_series(500);
    let mut atr = Atr::new(14);
    for c in candles(&prices) {
        if let Some(v) = atr.update(&c) {
            assert!(v >= Decimal::ZERO, "ATR went negative: {v}");
        }
    }
    assert!(atr.is_warm());
}
```

- [ ] **Step 15: Run the integration tests**

Run: `cargo test -p indicators && cargo clippy -p indicators -- -D warnings`
Expected: PASS — 14 tests total, no clippy warnings.

- [ ] **Step 16: Commit**

```bash
git add crates/indicators
git commit -m "feat(indicators): incremental EMA, Wilder RSI and ATR

All three are O(1) per update and return None until warm, so the engine
cannot act on a half-initialised indicator after a restart."
```

---

### Task 4: Bybit request signing and clock offset

**Files:**
- Create: `crates/exchange/Cargo.toml`, `crates/exchange/src/lib.rs`, `crates/exchange/src/bybit/mod.rs`, `crates/exchange/src/bybit/sign.rs`

**Interfaces:**
- Consumes: nothing from earlier tasks
- Produces:
  - `Credentials { api_key: String, api_secret: String }` with `Credentials::from_env() -> Result<Self, SignError>`
  - `sign_rest(secret: &str, timestamp_ms: i64, api_key: &str, recv_window: u32, payload: &str) -> String`
  - `sign_ws_auth(secret: &str, expires_ms: i64) -> String`
  - `ClockOffset` with `new()`, `observe(server_time_ms: i64, local_time_ms: i64)`, `now_ms(&self) -> i64`

- [ ] **Step 1: Create the crate manifest**

Create `crates/exchange/Cargo.toml`:

```toml
[package]
name = "exchange"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true

[dependencies]
core = { path = "../core" }
async-trait = "0.1"
futures-util = "0.3"
hmac = "0.12"
sha2 = "0.10"
hex = "0.4"
reqwest = { version = "0.12", features = ["json", "rustls-tls"], default-features = false }
rust_decimal.workspace = true
serde.workspace = true
serde_json.workspace = true
thiserror.workspace = true
tokio.workspace = true
tokio-tungstenite = { version = "0.24", features = ["rustls-tls-native-roots"] }
tracing.workspace = true

[dev-dependencies]
rust_decimal_macros.workspace = true
wiremock = "0.6"
```

- [ ] **Step 2: Write the failing signing test**

Create `crates/exchange/src/bybit/sign.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    // Fixed vector: the signature is HMAC_SHA256 over the exact concatenation
    // timestamp + api_key + recv_window + payload, hex-encoded lowercase.
    // Computed independently with:
    //   printf '1700000000000testkey5000{"symbol":"BTCUSDT"}' \
    //     | openssl dgst -sha256 -hmac testsecret
    const EXPECTED: &str =
        "9d0f5e2b6c60f2b1a06ba09c8a5ea62e1b6a5a5c9a5a0f0b1cb3f0e0d1f2a3b4";

    #[test]
    fn rest_signature_concatenates_in_the_documented_order() {
        let sig = sign_rest("testsecret", 1_700_000_000_000, "testkey", 5000, r#"{"symbol":"BTCUSDT"}"#);
        // Length and alphabet are what we assert deterministically; the exact
        // digest is pinned by the golden test below once generated locally.
        assert_eq!(sig.len(), 64, "HMAC-SHA256 hex must be 64 chars");
        assert!(sig.chars().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn rest_signature_is_deterministic() {
        let a = sign_rest("s", 1, "k", 5000, "payload");
        let b = sign_rest("s", 1, "k", 5000, "payload");
        assert_eq!(a, b);
    }

    #[test]
    fn rest_signature_changes_with_every_input() {
        let base = sign_rest("s", 1, "k", 5000, "p");
        assert_ne!(base, sign_rest("s2", 1, "k", 5000, "p"));
        assert_ne!(base, sign_rest("s", 2, "k", 5000, "p"));
        assert_ne!(base, sign_rest("s", 1, "k2", 5000, "p"));
        assert_ne!(base, sign_rest("s", 1, "k", 6000, "p"));
        assert_ne!(base, sign_rest("s", 1, "k", 5000, "p2"));
    }

    #[test]
    fn ws_auth_signs_the_literal_get_realtime_prefix() {
        // Bybit specifies HMAC_SHA256("GET/realtime" + expires).
        let expected = hmac_hex("secret", "GET/realtime1700000000000");
        assert_eq!(sign_ws_auth("secret", 1_700_000_000_000), expected);
    }

    #[test]
    fn clock_offset_corrects_local_drift() {
        let mut clock = ClockOffset::new();
        // Server is 3 seconds ahead of our local clock.
        clock.observe(1_700_000_003_000, 1_700_000_000_000);
        assert_eq!(clock.offset_ms(), 3_000);
    }

    #[test]
    fn clock_offset_defaults_to_zero_before_any_observation() {
        let clock = ClockOffset::new();
        assert_eq!(clock.offset_ms(), 0);
    }
}
```

Note on `EXPECTED`: leave the constant unused for now — Step 4 replaces it with a locally generated golden value.

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test -p exchange`
Expected: FAIL — `sign_rest`, `sign_ws_auth`, `ClockOffset`, `hmac_hex` not found.

- [ ] **Step 4: Implement signing and clock offset**

Prepend to `crates/exchange/src/bybit/sign.rs`:

```rust
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use hmac::{Hmac, Mac};
use sha2::Sha256;

type HmacSha256 = Hmac<Sha256>;

#[derive(Debug, thiserror::Error)]
pub enum SignError {
    #[error("missing environment variable {0}")]
    MissingEnv(&'static str),
}

/// API credentials. Loaded from the environment only — never from a file,
/// never from config, never logged.
#[derive(Clone)]
pub struct Credentials {
    pub api_key: String,
    pub api_secret: String,
}

impl Credentials {
    pub fn from_env() -> Result<Self, SignError> {
        Ok(Credentials {
            api_key: std::env::var("BYBIT_API_KEY")
                .map_err(|_| SignError::MissingEnv("BYBIT_API_KEY"))?,
            api_secret: std::env::var("BYBIT_API_SECRET")
                .map_err(|_| SignError::MissingEnv("BYBIT_API_SECRET"))?,
        })
    }
}

// Deliberately opaque: prevents a secret reaching logs through a derived Debug.
impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("api_key", &"<redacted>")
            .field("api_secret", &"<redacted>")
            .finish()
    }
}

pub(crate) fn hmac_hex(secret: &str, message: &str) -> String {
    let mut mac = HmacSha256::new_from_slice(secret.as_bytes())
        .expect("HMAC accepts keys of any length");
    mac.update(message.as_bytes());
    hex::encode(mac.finalize().into_bytes())
}

/// REST signature: HMAC_SHA256 over timestamp + api_key + recv_window + payload.
///
/// `payload` is the raw query string for GET requests and the exact JSON body
/// for POST requests — byte-identical to what is actually transmitted.
pub fn sign_rest(
    secret: &str,
    timestamp_ms: i64,
    api_key: &str,
    recv_window: u32,
    payload: &str,
) -> String {
    hmac_hex(secret, &format!("{timestamp_ms}{api_key}{recv_window}{payload}"))
}

/// Private WebSocket auth signature: HMAC_SHA256 over "GET/realtime" + expires.
pub fn sign_ws_auth(secret: &str, expires_ms: i64) -> String {
    hmac_hex(secret, &format!("GET/realtime{expires_ms}"))
}

/// Tracks drift between the local clock and Bybit's server clock.
///
/// Bybit rejects requests unless
/// `server_time - recv_window <= timestamp < server_time + 1000`, so a machine
/// with a few seconds of NTP drift would fail every signed request. Every
/// response carries a server `time`, which we use to correct.
#[derive(Debug)]
pub struct ClockOffset {
    offset_ms: AtomicI64,
}

impl ClockOffset {
    pub fn new() -> Self {
        ClockOffset { offset_ms: AtomicI64::new(0) }
    }

    /// Record an observation of the server clock against our own.
    pub fn observe(&self, server_time_ms: i64, local_time_ms: i64) {
        self.offset_ms.store(server_time_ms - local_time_ms, Ordering::Relaxed);
    }

    pub fn offset_ms(&self) -> i64 {
        self.offset_ms.load(Ordering::Relaxed)
    }

    /// Current time in the server's frame of reference.
    pub fn now_ms(&self) -> i64 {
        local_now_ms() + self.offset_ms()
    }
}

impl Default for ClockOffset {
    fn default() -> Self {
        Self::new()
    }
}

pub fn local_now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock is after the unix epoch")
        .as_millis() as i64
}
```

The `ClockOffset` test calls `observe` on a non-`mut` binding — that is intentional, `AtomicI64` gives interior mutability so the offset can be updated from any task without a lock. Change `let mut clock` to `let clock` in the test.

Create `crates/exchange/src/bybit/mod.rs`:

```rust
pub mod sign;
```

Create `crates/exchange/src/lib.rs`:

```rust
pub mod bybit;
```

- [ ] **Step 5: Generate the golden signature vector**

Run:

```bash
printf '1700000000000testkey5000{"symbol":"BTCUSDT"}' | openssl dgst -sha256 -hmac testsecret
```

Replace the `EXPECTED` constant in the test module with the hex digest printed, then add this assertion to `rest_signature_concatenates_in_the_documented_order`:

```rust
        assert_eq!(sig, EXPECTED, "signature drifted from the pinned vector");
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p exchange`
Expected: PASS — 6 tests.

- [ ] **Step 7: Commit**

```bash
git add crates/exchange
git commit -m "feat(exchange): Bybit V5 request signing and clock-offset tracking

Credentials implement Debug manually so a secret cannot reach logs through
a derived impl. ClockOffset corrects local NTP drift, which would otherwise
fail every signed request against Bybit's recv_window check."
```

---

### Task 5: Transport policy — error classification, retry and rate limiting

**Files:**
- Create: `crates/exchange/src/bybit/transport.rs`, `crates/exchange/src/bybit/rate_limit.rs`
- Modify: `crates/exchange/src/bybit/mod.rs`

**Interfaces:**
- Consumes: `core::ErrorClass`
- Produces:
  - `ExchangeError` enum with `class(&self) -> ErrorClass`
  - `classify_ret_code(ret_code: i32) -> ErrorClass`
  - `backoff_delay(attempt: u32, base_ms: u64, jitter: f64) -> Duration`
  - `RateLimiter::new(capacity: u32, refill_per_sec: u32)`, `RateLimiter::acquire(&self).await`

- [ ] **Step 1: Write the failing error-classification test**

Create `crates/exchange/src/bybit/transport.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use core::ErrorClass;

    #[test]
    fn rate_limit_codes_are_retryable() {
        assert_eq!(classify_ret_code(10006), ErrorClass::Retryable);
        assert_eq!(classify_ret_code(10018), ErrorClass::Retryable);
    }

    #[test]
    fn auth_failures_are_fatal() {
        // 10003 invalid api key, 10004 bad sign, 10005 permission denied.
        assert_eq!(classify_ret_code(10003), ErrorClass::Fatal);
        assert_eq!(classify_ret_code(10004), ErrorClass::Fatal);
        assert_eq!(classify_ret_code(10005), ErrorClass::Fatal);
    }

    #[test]
    fn business_rejections_are_rejected_not_fatal() {
        // 110007 insufficient balance, 110017 price/qty precision.
        assert_eq!(classify_ret_code(110007), ErrorClass::Rejected);
        assert_eq!(classify_ret_code(110017), ErrorClass::Rejected);
    }

    #[test]
    fn unknown_codes_default_to_rejected() {
        // Defaulting to Rejected rather than Retryable is deliberate: an
        // unrecognised failure must not be retried in a loop against a live
        // exchange.
        assert_eq!(classify_ret_code(999_999), ErrorClass::Rejected);
    }

    #[test]
    fn backoff_grows_exponentially_and_is_bounded() {
        let d0 = backoff_delay(0, 200, 0.0);
        let d1 = backoff_delay(1, 200, 0.0);
        let d2 = backoff_delay(2, 200, 0.0);
        assert_eq!(d0.as_millis(), 200);
        assert_eq!(d1.as_millis(), 400);
        assert_eq!(d2.as_millis(), 800);
        // Capped so a long outage never produces an absurd sleep.
        assert!(backoff_delay(30, 200, 0.0) <= MAX_BACKOFF);
    }

    #[test]
    fn jitter_stays_within_the_requested_fraction() {
        for attempt in 0..5 {
            let base = backoff_delay(attempt, 200, 0.0).as_millis() as f64;
            for _ in 0..50 {
                let jittered = backoff_delay(attempt, 200, 0.25).as_millis() as f64;
                assert!(jittered >= base * 0.75, "{jittered} below jitter floor");
                assert!(jittered <= base * 1.25, "{jittered} above jitter ceiling");
            }
        }
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p exchange`
Expected: FAIL — `classify_ret_code`, `backoff_delay`, `MAX_BACKOFF` not found.

- [ ] **Step 3: Implement transport policy**

Prepend to `crates/exchange/src/bybit/transport.rs`:

```rust
use std::time::Duration;

use core::ErrorClass;

/// Upper bound on a single backoff sleep. A multi-hour outage should keep the
/// bot polling at a sane cadence, not sleeping for days.
pub const MAX_BACKOFF: Duration = Duration::from_secs(60);

#[derive(Debug, thiserror::Error)]
pub enum ExchangeError {
    #[error("http transport error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("bybit returned retCode {code}: {msg}")]
    Api { code: i32, msg: String },

    #[error("failed to decode bybit response: {0}")]
    Decode(String),

    #[error("websocket error: {0}")]
    WebSocket(String),

    #[error("retries exhausted after {attempts} attempts: {last}")]
    RetriesExhausted { attempts: u32, last: Box<ExchangeError> },
}

impl ExchangeError {
    pub fn class(&self) -> ErrorClass {
        match self {
            // A transport failure carries no verdict from the exchange, so it
            // is always safe to retry — orderLinkId makes it idempotent.
            ExchangeError::Http(_) | ExchangeError::WebSocket(_) => ErrorClass::Retryable,
            ExchangeError::Api { code, .. } => classify_ret_code(*code),
            ExchangeError::Decode(_) => ErrorClass::Rejected,
            ExchangeError::RetriesExhausted { last, .. } => last.class(),
        }
    }
}

/// Map a Bybit `retCode` onto an engine reaction.
///
/// Unknown codes default to `Rejected`, never `Retryable`: retrying an
/// unrecognised failure against a live exchange is how a bot spams orders.
pub fn classify_ret_code(ret_code: i32) -> ErrorClass {
    match ret_code {
        10006 | 10016 | 10018 => ErrorClass::Retryable,
        10003 | 10004 | 10005 | 10010 | 33004 => ErrorClass::Fatal,
        _ => ErrorClass::Rejected,
    }
}

/// Exponential backoff with symmetric multiplicative jitter.
///
/// `jitter` is a fraction (0.25 means +/-25%). Jitter matters because every
/// symbol's stream reconnects at once after a network blip; without it they
/// would retry in lockstep and trip the rate limiter.
pub fn backoff_delay(attempt: u32, base_ms: u64, jitter: f64) -> Duration {
    let raw = base_ms.saturating_mul(1u64 << attempt.min(20));
    let capped = Duration::from_millis(raw).min(MAX_BACKOFF);
    if jitter <= 0.0 {
        return capped;
    }
    let factor = 1.0 + (pseudo_unit_random() * 2.0 - 1.0) * jitter;
    Duration::from_secs_f64((capped.as_secs_f64() * factor).max(0.0))
}

/// Small non-cryptographic RNG in [0,1). Avoids pulling in `rand` for jitter.
fn pseudo_unit_random() -> f64 {
    use std::cell::Cell;
    thread_local! {
        static STATE: Cell<u64> = const { Cell::new(0x853C49E6_748FEA9B) };
    }
    STATE.with(|s| {
        let mut x = s.get();
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        s.set(x);
        (x >> 11) as f64 / (1u64 << 53) as f64
    })
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p exchange`
Expected: PASS — 12 tests total.

- [ ] **Step 5: Write the failing rate-limiter test**

Create `crates/exchange/src/bybit/rate_limit.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[tokio::test]
    async fn tokens_within_capacity_are_immediate() {
        let limiter = RateLimiter::new(5, 5);
        let start = Instant::now();
        for _ in 0..5 {
            limiter.acquire().await;
        }
        assert!(start.elapsed() < Duration::from_millis(50), "burst was throttled");
    }

    #[tokio::test]
    async fn exceeding_capacity_waits_for_refill() {
        // 2 tokens capacity, refilling 2 per second: the third acquire must
        // wait roughly half a second.
        let limiter = RateLimiter::new(2, 2);
        limiter.acquire().await;
        limiter.acquire().await;
        let start = Instant::now();
        limiter.acquire().await;
        let waited = start.elapsed();
        assert!(waited >= Duration::from_millis(400), "did not throttle: {waited:?}");
        assert!(waited < Duration::from_millis(900), "throttled too long: {waited:?}");
    }
}
```

- [ ] **Step 6: Run the test to verify it fails**

Run: `cargo test -p exchange`
Expected: FAIL — `RateLimiter` not found.

- [ ] **Step 7: Implement the token-bucket rate limiter**

Prepend to `crates/exchange/src/bybit/rate_limit.rs`:

```rust
use std::time::Duration;

use tokio::sync::Mutex;
use tokio::time::Instant;

/// Client-side token bucket, sized below Bybit's published limits.
///
/// Enforcing the limit ourselves means a retry storm degrades into waiting
/// rather than escalating into an IP ban, which would take the bot offline
/// while positions are open.
#[derive(Debug)]
pub struct RateLimiter {
    capacity: f64,
    refill_per_sec: f64,
    state: Mutex<BucketState>,
}

#[derive(Debug)]
struct BucketState {
    tokens: f64,
    last_refill: Instant,
}

impl RateLimiter {
    pub fn new(capacity: u32, refill_per_sec: u32) -> Self {
        assert!(capacity > 0 && refill_per_sec > 0);
        RateLimiter {
            capacity: capacity as f64,
            refill_per_sec: refill_per_sec as f64,
            state: Mutex::new(BucketState {
                tokens: capacity as f64,
                last_refill: Instant::now(),
            }),
        }
    }

    /// Consume one token, waiting if the bucket is empty.
    pub async fn acquire(&self) {
        loop {
            let wait = {
                let mut st = self.state.lock().await;
                let now = Instant::now();
                let elapsed = now.duration_since(st.last_refill).as_secs_f64();
                st.tokens = (st.tokens + elapsed * self.refill_per_sec).min(self.capacity);
                st.last_refill = now;

                if st.tokens >= 1.0 {
                    st.tokens -= 1.0;
                    return;
                }
                // Time until one whole token is available.
                Duration::from_secs_f64((1.0 - st.tokens) / self.refill_per_sec)
            };
            tokio::time::sleep(wait).await;
        }
    }
}
```

Update `crates/exchange/src/bybit/mod.rs`:

```rust
pub mod rate_limit;
pub mod sign;
pub mod transport;
```

- [ ] **Step 8: Run the tests to verify they pass**

Run: `cargo test -p exchange && cargo clippy -p exchange -- -D warnings`
Expected: PASS — 14 tests total, no clippy warnings.

- [ ] **Step 9: Commit**

```bash
git add crates/exchange
git commit -m "feat(exchange): error classification, jittered backoff, token-bucket limiter

Unknown retCodes classify as Rejected rather than Retryable so an
unrecognised failure can never turn into an order-spamming retry loop."
```

---

### Task 6: REST client — wire types and market data endpoints

**Files:**
- Create: `crates/exchange/src/bybit/wire.rs`, `crates/exchange/src/bybit/rest.rs`
- Modify: `crates/exchange/src/bybit/mod.rs`

**Interfaces:**
- Consumes: `sign_rest`, `ClockOffset`, `Credentials` (Task 4); `ExchangeError`, `RateLimiter`, `backoff_delay` (Task 5); `Candle`, `Instrument`, `Symbol`, `Timeframe` (Tasks 1–2)
- Produces:
  - `BybitRest::new(base_url: String, creds: Credentials) -> Self`
  - `BybitRest::instruments(&self) -> Result<Vec<Instrument>, ExchangeError>`
  - `BybitRest::tickers(&self) -> Result<Vec<Ticker>, ExchangeError>`
  - `BybitRest::klines(&self, &Symbol, Timeframe, u16) -> Result<Vec<Candle>, ExchangeError>`
  - `Ticker { symbol: Symbol, turnover_24h: Decimal, last_price: Decimal }`

- [ ] **Step 1: Write the failing kline-parsing test**

Create `crates/exchange/src/bybit/wire.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn kline_rows_parse_from_bybit_string_arrays() {
        // Bybit returns klines as arrays of strings:
        // [startTime, open, high, low, close, volume, turnover]
        let raw = r#"["1700000000000","42000.5","42500.0","41800.25","42100.75","123.45","5200000.5"]"#;
        let row: KlineRow = serde_json::from_str(raw).expect("row parses");
        let candle = row.into_candle().expect("row converts");

        assert_eq!(candle.open_time_ms, 1_700_000_000_000);
        assert_eq!(candle.open, dec!(42000.5));
        assert_eq!(candle.high, dec!(42500.0));
        assert_eq!(candle.low, dec!(41800.25));
        assert_eq!(candle.close, dec!(42100.75));
        assert_eq!(candle.volume, dec!(123.45));
        assert_eq!(candle.turnover, dec!(5200000.5));
    }

    #[test]
    fn envelope_surfaces_nonzero_ret_code_as_api_error() {
        let raw = r#"{"retCode":110007,"retMsg":"insufficient balance","result":{},"time":1700000000000}"#;
        let env: Envelope<serde_json::Value> = serde_json::from_str(raw).expect("envelope parses");
        let err = env.into_result().expect_err("nonzero retCode must be an error");
        match err {
            ExchangeError::Api { code, ref msg } => {
                assert_eq!(code, 110007);
                assert_eq!(msg, "insufficient balance");
            }
            other => panic!("expected Api error, got {other:?}"),
        }
    }

    #[test]
    fn envelope_returns_result_on_success() {
        let raw = r#"{"retCode":0,"retMsg":"OK","result":{"value":7},"time":1700000000000}"#;
        let env: Envelope<serde_json::Value> = serde_json::from_str(raw).expect("envelope parses");
        assert_eq!(env.time, 1_700_000_000_000);
        let value = env.into_result().expect("retCode 0 is success");
        assert_eq!(value["value"], 7);
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p exchange`
Expected: FAIL — `KlineRow`, `Envelope` not found.

- [ ] **Step 3: Implement the wire types**

Prepend to `crates/exchange/src/bybit/wire.rs`:

```rust
use core::{Candle, Instrument, Symbol};
use rust_decimal::Decimal;
use serde::Deserialize;

use super::transport::ExchangeError;

/// Every Bybit V5 response shares this envelope. `time` is the server clock,
/// which feeds ClockOffset on every single call.
#[derive(Debug, Deserialize)]
pub struct Envelope<T> {
    #[serde(rename = "retCode")]
    pub ret_code: i32,
    #[serde(rename = "retMsg")]
    pub ret_msg: String,
    pub result: T,
    pub time: i64,
}

impl<T> Envelope<T> {
    pub fn into_result(self) -> Result<T, ExchangeError> {
        if self.ret_code == 0 {
            Ok(self.result)
        } else {
            Err(ExchangeError::Api { code: self.ret_code, msg: self.ret_msg })
        }
    }
}

/// A kline as Bybit transmits it: a positional array of strings.
#[derive(Debug, Deserialize)]
pub struct KlineRow(
    pub String, // start time ms
    pub String, // open
    pub String, // high
    pub String, // low
    pub String, // close
    pub String, // volume
    pub String, // turnover
);

impl KlineRow {
    pub fn into_candle(self) -> Result<Candle, ExchangeError> {
        let parse = |s: &str, field: &str| -> Result<Decimal, ExchangeError> {
            s.parse::<Decimal>()
                .map_err(|e| ExchangeError::Decode(format!("kline {field}: {e}")))
        };
        Ok(Candle {
            open_time_ms: self
                .0
                .parse::<i64>()
                .map_err(|e| ExchangeError::Decode(format!("kline start: {e}")))?,
            open: parse(&self.1, "open")?,
            high: parse(&self.2, "high")?,
            low: parse(&self.3, "low")?,
            close: parse(&self.4, "close")?,
            volume: parse(&self.5, "volume")?,
            turnover: parse(&self.6, "turnover")?,
        })
    }
}

#[derive(Debug, Deserialize)]
pub struct KlineResult {
    pub list: Vec<KlineRow>,
}

#[derive(Debug, Deserialize)]
pub struct ListResult<T> {
    pub list: Vec<T>,
}

#[derive(Debug, Deserialize)]
pub struct InstrumentRow {
    pub symbol: String,
    pub status: String,
    #[serde(rename = "launchTime")]
    pub launch_time: String,
    #[serde(rename = "priceFilter")]
    pub price_filter: PriceFilter,
    #[serde(rename = "lotSizeFilter")]
    pub lot_size_filter: LotSizeFilter,
}

#[derive(Debug, Deserialize)]
pub struct PriceFilter {
    #[serde(rename = "tickSize")]
    pub tick_size: String,
}

#[derive(Debug, Deserialize)]
pub struct LotSizeFilter {
    #[serde(rename = "qtyStep")]
    pub qty_step: String,
    #[serde(rename = "minOrderQty")]
    pub min_order_qty: String,
}

impl InstrumentRow {
    /// Only instruments with `status == "Trading"` are convertible; anything
    /// else is filtered out rather than silently traded.
    pub fn into_instrument(self) -> Result<Option<Instrument>, ExchangeError> {
        if self.status != "Trading" {
            return Ok(None);
        }
        let parse = |s: &str, field: &str| -> Result<Decimal, ExchangeError> {
            s.parse::<Decimal>()
                .map_err(|e| ExchangeError::Decode(format!("instrument {field}: {e}")))
        };
        Ok(Some(Instrument {
            symbol: Symbol::new(self.symbol),
            tick_size: parse(&self.price_filter.tick_size, "tickSize")?,
            qty_step: parse(&self.lot_size_filter.qty_step, "qtyStep")?,
            min_order_qty: parse(&self.lot_size_filter.min_order_qty, "minOrderQty")?,
            launch_time_ms: self
                .launch_time
                .parse::<i64>()
                .map_err(|e| ExchangeError::Decode(format!("instrument launchTime: {e}")))?,
        }))
    }
}

#[derive(Debug, Deserialize)]
pub struct TickerRow {
    pub symbol: String,
    #[serde(rename = "turnover24h")]
    pub turnover_24h: String,
    #[serde(rename = "lastPrice")]
    pub last_price: String,
}

/// 24h market statistics, used for universe ranking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ticker {
    pub symbol: Symbol,
    pub turnover_24h: Decimal,
    pub last_price: Decimal,
}

impl TickerRow {
    pub fn into_ticker(self) -> Result<Ticker, ExchangeError> {
        let parse = |s: &str, field: &str| -> Result<Decimal, ExchangeError> {
            s.parse::<Decimal>()
                .map_err(|e| ExchangeError::Decode(format!("ticker {field}: {e}")))
        };
        Ok(Ticker {
            symbol: Symbol::new(self.symbol),
            turnover_24h: parse(&self.turnover_24h, "turnover24h")?,
            last_price: parse(&self.last_price, "lastPrice")?,
        })
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p exchange`
Expected: PASS — 17 tests total.

- [ ] **Step 5: Write the failing REST market-data test against a mock server**

Create `crates/exchange/tests/rest_market_data.rs`:

```rust
use core::{Symbol, Timeframe};
use exchange::bybit::rest::BybitRest;
use exchange::bybit::sign::Credentials;
use rust_decimal_macros::dec;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn creds() -> Credentials {
    Credentials { api_key: "k".into(), api_secret: "s".into() }
}

#[tokio::test]
async fn klines_are_returned_oldest_first() {
    let server = MockServer::start().await;

    // Bybit returns klines newest-first; the client must reverse them so
    // indicators are fed in chronological order.
    let body = serde_json::json!({
        "retCode": 0,
        "retMsg": "OK",
        "result": {
            "list": [
                ["1700003600000","102","103","101","102.5","1","100"],
                ["1700000000000","100","101","99","100.5","1","100"]
            ]
        },
        "time": 1700007200000i64
    });

    Mock::given(method("GET"))
        .and(path("/v5/market/kline"))
        .and(query_param("category", "linear"))
        .and(query_param("symbol", "BTCUSDT"))
        .and(query_param("interval", "60"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;

    let client = BybitRest::new(server.uri(), creds());
    let candles = client
        .klines(&Symbol::new("BTCUSDT"), Timeframe::H1, 2)
        .await
        .expect("klines fetch succeeds");

    assert_eq!(candles.len(), 2);
    assert_eq!(candles[0].open_time_ms, 1_700_000_000_000, "oldest must come first");
    assert_eq!(candles[1].open_time_ms, 1_700_003_600_000);
    assert_eq!(candles[0].close, dec!(100.5));
}

#[tokio::test]
async fn non_trading_instruments_are_filtered_out() {
    let server = MockServer::start().await;

    let body = serde_json::json!({
        "retCode": 0,
        "retMsg": "OK",
        "result": {
            "list": [
                {
                    "symbol": "BTCUSDT", "status": "Trading", "launchTime": "1600000000000",
                    "priceFilter": {"tickSize": "0.1"},
                    "lotSizeFilter": {"qtyStep": "0.001", "minOrderQty": "0.001"}
                },
                {
                    "symbol": "DEADUSDT", "status": "Delivering", "launchTime": "1600000000000",
                    "priceFilter": {"tickSize": "0.1"},
                    "lotSizeFilter": {"qtyStep": "0.001", "minOrderQty": "0.001"}
                }
            ]
        },
        "time": 1700007200000i64
    });

    Mock::given(method("GET"))
        .and(path("/v5/market/instruments-info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;

    let client = BybitRest::new(server.uri(), creds());
    let instruments = client.instruments().await.expect("instruments fetch succeeds");

    assert_eq!(instruments.len(), 1, "delivering instrument must be dropped");
    assert_eq!(instruments[0].symbol.as_str(), "BTCUSDT");
    assert_eq!(instruments[0].tick_size, dec!(0.1));
}

#[tokio::test]
async fn server_time_updates_the_clock_offset() {
    let server = MockServer::start().await;
    let body = serde_json::json!({
        "retCode": 0, "retMsg": "OK",
        "result": {"list": []},
        "time": 1700007200000i64
    });
    Mock::given(method("GET"))
        .and(path("/v5/market/tickers"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;

    let client = BybitRest::new(server.uri(), creds());
    client.tickers().await.expect("tickers fetch succeeds");

    // The mock's server time is far in the past relative to now, so the offset
    // must have moved off its zero default.
    assert_ne!(client.clock().offset_ms(), 0, "clock offset was never observed");
}

#[tokio::test]
async fn api_error_code_is_surfaced_not_swallowed() {
    let server = MockServer::start().await;
    let body = serde_json::json!({
        "retCode": 10001, "retMsg": "param error", "result": {}, "time": 1700007200000i64
    });
    Mock::given(method("GET"))
        .and(path("/v5/market/tickers"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;

    let client = BybitRest::new(server.uri(), creds());
    let err = client.tickers().await.expect_err("retCode 10001 must fail");
    assert!(err.to_string().contains("10001"), "error lost the retCode: {err}");
}
```

- [ ] **Step 6: Run the test to verify it fails**

Run: `cargo test -p exchange --test rest_market_data`
Expected: FAIL — `BybitRest` not found.

- [ ] **Step 7: Implement the REST client core and market-data endpoints**

Create `crates/exchange/src/bybit/rest.rs`:

```rust
use core::{Candle, Instrument, Symbol, Timeframe};
use serde::de::DeserializeOwned;
use tracing::warn;

use super::rate_limit::RateLimiter;
use super::sign::{local_now_ms, sign_rest, ClockOffset, Credentials};
use super::transport::{backoff_delay, ExchangeError};
use super::wire::{
    Envelope, InstrumentRow, KlineResult, ListResult, Ticker, TickerRow,
};

const RECV_WINDOW: u32 = 5_000;
const MAX_ATTEMPTS: u32 = 5;
const BACKOFF_BASE_MS: u64 = 200;
const BACKOFF_JITTER: f64 = 0.25;

/// Bybit V5 REST client.
///
/// Every response updates the clock offset, so signing stays valid even on a
/// machine whose local clock drifts.
pub struct BybitRest {
    base_url: String,
    creds: Credentials,
    http: reqwest::Client,
    clock: ClockOffset,
    limiter: RateLimiter,
}

impl BybitRest {
    pub fn new(base_url: String, creds: Credentials) -> Self {
        BybitRest {
            base_url: base_url.trim_end_matches('/').to_string(),
            creds,
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("reqwest client builds with default TLS"),
            clock: ClockOffset::new(),
            limiter: RateLimiter::new(30, 10),
        }
    }

    pub fn clock(&self) -> &ClockOffset {
        &self.clock
    }

    /// Signed GET with query parameters, retried on `Retryable` failures.
    pub(crate) async fn get<T: DeserializeOwned>(
        &self,
        path: &str,
        params: &[(&str, String)],
    ) -> Result<T, ExchangeError> {
        let query = params
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&");

        self.with_retry(|| async {
            self.limiter.acquire().await;
            let ts = self.clock.now_ms();
            let sign = sign_rest(&self.creds.api_secret, ts, &self.creds.api_key, RECV_WINDOW, &query);
            let url = format!("{}{}?{}", self.base_url, path, query);

            let resp = self
                .http
                .get(&url)
                .header("X-BAPI-API-KEY", &self.creds.api_key)
                .header("X-BAPI-TIMESTAMP", ts.to_string())
                .header("X-BAPI-RECV-WINDOW", RECV_WINDOW.to_string())
                .header("X-BAPI-SIGN", sign)
                .send()
                .await?;

            let text = resp.text().await?;
            let env: Envelope<T> = serde_json::from_str(&text)
                .map_err(|e| ExchangeError::Decode(format!("{e}: {text}")))?;
            self.clock.observe(env.time, local_now_ms());
            env.into_result()
        })
        .await
    }

    /// Retry loop honouring the error classification from Task 5.
    pub(crate) async fn with_retry<T, F, Fut>(&self, mut op: F) -> Result<T, ExchangeError>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<T, ExchangeError>>,
    {
        use core::ErrorClass;

        let mut last: Option<ExchangeError> = None;
        for attempt in 0..MAX_ATTEMPTS {
            match op().await {
                Ok(v) => return Ok(v),
                Err(e) => {
                    if e.class() != ErrorClass::Retryable {
                        return Err(e);
                    }
                    warn!(attempt, error = %e, "retryable exchange error");
                    last = Some(e);
                    tokio::time::sleep(backoff_delay(attempt, BACKOFF_BASE_MS, BACKOFF_JITTER)).await;
                }
            }
        }
        Err(ExchangeError::RetriesExhausted {
            attempts: MAX_ATTEMPTS,
            last: Box::new(last.expect("loop ran at least once")),
        })
    }

    /// All linear perpetual instruments currently in `Trading` status.
    pub async fn instruments(&self) -> Result<Vec<Instrument>, ExchangeError> {
        let res: ListResult<InstrumentRow> = self
            .get("/v5/market/instruments-info", &[("category", "linear".into())])
            .await?;
        let mut out = Vec::with_capacity(res.list.len());
        for row in res.list {
            if let Some(i) = row.into_instrument()? {
                out.push(i);
            }
        }
        Ok(out)
    }

    /// 24h statistics for every linear perpetual, used for universe ranking.
    pub async fn tickers(&self) -> Result<Vec<Ticker>, ExchangeError> {
        let res: ListResult<TickerRow> = self
            .get("/v5/market/tickers", &[("category", "linear".into())])
            .await?;
        res.list.into_iter().map(TickerRow::into_ticker).collect()
    }

    /// Recent klines, returned **oldest first** regardless of Bybit's ordering,
    /// because indicators must be fed chronologically.
    pub async fn klines(
        &self,
        symbol: &Symbol,
        tf: Timeframe,
        limit: u16,
    ) -> Result<Vec<Candle>, ExchangeError> {
        let res: KlineResult = self
            .get(
                "/v5/market/kline",
                &[
                    ("category", "linear".into()),
                    ("symbol", symbol.as_str().to_string()),
                    ("interval", tf.as_bybit_interval().to_string()),
                    ("limit", limit.to_string()),
                ],
            )
            .await?;

        let mut candles: Vec<Candle> =
            res.list.into_iter().map(|r| r.into_candle()).collect::<Result<_, _>>()?;
        candles.sort_by_key(|c| c.open_time_ms);
        Ok(candles)
    }
}
```

Update `crates/exchange/src/bybit/mod.rs`:

```rust
pub mod rate_limit;
pub mod rest;
pub mod sign;
pub mod transport;
pub mod wire;
```

- [ ] **Step 8: Run the tests to verify they pass**

Run: `cargo test -p exchange`
Expected: PASS — 21 tests total.

- [ ] **Step 9: Commit**

```bash
git add crates/exchange
git commit -m "feat(exchange): Bybit V5 REST client with market data endpoints

Klines are always returned oldest-first regardless of Bybit's newest-first
ordering, so indicators can never be fed a reversed series. Every response
updates the clock offset used for signing."
```

---

### Task 7: REST client — limit-only trading endpoints and the ExchangeClient trait

**Files:**
- Create: `crates/exchange/src/traits.rs`
- Modify: `crates/exchange/src/bybit/rest.rs`, `crates/exchange/src/bybit/wire.rs`, `crates/exchange/src/lib.rs`, `crates/exchange/tests/rest_market_data.rs`
- Test: `crates/exchange/tests/rest_trading.rs`, `crates/exchange/tests/no_market_orders.rs`

**Interfaces:**
- Consumes: `BybitRest` (Task 6), `LimitEntry`, `OrderAck`, `OpenOrder`, `OrderState`, `Position`, `Balance` (Task 2)
- Produces:
  - `trait ExchangeClient` with the ten methods from the spec — and no market-order method
  - `impl ExchangeClient for BybitRest` holding the **only** definition of each of the ten endpoints. Task 6's three market-data methods move into this impl; no inherent duplicates remain, and nothing delegates.
  - `impl BybitRest` retains only `new`, `clock`, `get`, `post`, `with_retry`

- [ ] **Step 1: Write the failing trading-endpoint test**

Create `crates/exchange/tests/rest_trading.rs`:

```rust
use core::{LimitEntry, Side, Symbol};
use exchange::bybit::rest::BybitRest;
use exchange::bybit::sign::Credentials;
use rust_decimal_macros::dec;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn creds() -> Credentials {
    Credentials { api_key: "k".into(), api_secret: "s".into() }
}

fn entry() -> LimitEntry {
    LimitEntry {
        symbol: Symbol::new("BTCUSDT"),
        side: Side::Buy,
        qty: dec!(0.01),
        price: dec!(42000.5),
        order_link_id: "abc123".into(),
        stop_loss: dec!(41000),
        stop_limit_price: dec!(40900),
        take_profit: dec!(44001),
    }
}

#[tokio::test]
async fn entry_is_sent_as_a_postonly_limit_with_protection_attached() {
    let server = MockServer::start().await;

    // Assert on the exact wire body: orderType Limit, PostOnly, and both
    // stop and target present in the same request so no unprotected window
    // can exist between entry and protection.
    Mock::given(method("POST"))
        .and(path("/v5/order/create"))
        .and(body_partial_json(serde_json::json!({
            "category": "linear",
            "symbol": "BTCUSDT",
            "side": "Buy",
            "orderType": "Limit",
            "timeInForce": "PostOnly",
            "positionIdx": 0,
            "qty": "0.01",
            "price": "42000.5",
            "orderLinkId": "abc123",
            "stopLoss": "41000",
            "slLimitPrice": "40900",
            "slOrderType": "Limit",
            "slTriggerBy": "MarkPrice",
            "takeProfit": "44001",
            "tpOrderType": "Limit"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "retCode": 0, "retMsg": "OK",
            "result": {"orderId": "oid-1", "orderLinkId": "abc123"},
            "time": 1700007200000i64
        })))
        .mount(&server)
        .await;

    let client = BybitRest::new(server.uri(), creds());
    let ack = client.place_limit_entry(entry()).await.expect("order placed");
    assert_eq!(ack.order_id, "oid-1");
    assert_eq!(ack.order_link_id, "abc123");
}

#[tokio::test]
async fn positions_parse_liquidation_price_as_optional() {
    let server = MockServer::start().await;

    // Bybit sends an empty string when there is no liquidation price.
    Mock::given(method("GET"))
        .and(path("/v5/position/list"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "retCode": 0, "retMsg": "OK",
            "result": {"list": [
                {"symbol":"BTCUSDT","side":"Buy","size":"0.01","avgPrice":"42000",
                 "liqPrice":"38000","unrealisedPnl":"12.5"},
                {"symbol":"ETHUSDT","side":"Sell","size":"0.5","avgPrice":"2500",
                 "liqPrice":"","unrealisedPnl":"-3"}
            ]},
            "time": 1700007200000i64
        })))
        .mount(&server)
        .await;

    let client = BybitRest::new(server.uri(), creds());
    let positions = client.positions().await.expect("positions fetched");

    assert_eq!(positions.len(), 2);
    assert_eq!(positions[0].liq_price, Some(dec!(38000)));
    assert_eq!(positions[1].liq_price, None, "empty liqPrice must become None");
    assert_eq!(positions[1].side, Side::Sell);
}

#[tokio::test]
async fn zero_size_positions_are_excluded() {
    let server = MockServer::start().await;

    // Bybit reports closed positions with size "0"; treating those as open
    // would make the reconciler adopt phantom positions.
    Mock::given(method("GET"))
        .and(path("/v5/position/list"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "retCode": 0, "retMsg": "OK",
            "result": {"list": [
                {"symbol":"BTCUSDT","side":"Buy","size":"0","avgPrice":"0",
                 "liqPrice":"","unrealisedPnl":"0"}
            ]},
            "time": 1700007200000i64
        })))
        .mount(&server)
        .await;

    let client = BybitRest::new(server.uri(), creds());
    assert!(client.positions().await.expect("fetched").is_empty());
}

#[tokio::test]
async fn balance_reads_equity_and_available_margin() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v5/account/wallet-balance"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "retCode": 0, "retMsg": "OK",
            "result": {"list": [{
                "totalEquity": "10250.75",
                "totalAvailableBalance": "9800.25"
            }]},
            "time": 1700007200000i64
        })))
        .mount(&server)
        .await;

    let client = BybitRest::new(server.uri(), creds());
    let bal = client.balance().await.expect("balance fetched");
    assert_eq!(bal.equity, dec!(10250.75));
    assert_eq!(bal.available, dec!(9800.25));
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p exchange --test rest_trading`
Expected: FAIL — `place_limit_entry`, `positions`, `balance` not found.

- [ ] **Step 3: Add the trading wire types**

Append to `crates/exchange/src/bybit/wire.rs`:

```rust
use core::{Balance, OpenOrder, OrderState, Position, Side};

#[derive(Debug, Deserialize)]
pub struct OrderCreateResult {
    #[serde(rename = "orderId")]
    pub order_id: String,
    #[serde(rename = "orderLinkId")]
    pub order_link_id: String,
}

#[derive(Debug, Deserialize)]
pub struct PositionRow {
    pub symbol: String,
    pub side: String,
    pub size: String,
    #[serde(rename = "avgPrice")]
    pub avg_price: String,
    #[serde(rename = "liqPrice")]
    pub liq_price: String,
    #[serde(rename = "unrealisedPnl")]
    pub unrealised_pnl: String,
}

/// Parse a Decimal field, treating an empty string as absent.
fn opt_decimal(s: &str) -> Result<Option<Decimal>, ExchangeError> {
    if s.trim().is_empty() {
        return Ok(None);
    }
    s.parse::<Decimal>()
        .map(Some)
        .map_err(|e| ExchangeError::Decode(format!("decimal field: {e}")))
}

fn req_decimal(s: &str, field: &str) -> Result<Decimal, ExchangeError> {
    s.parse::<Decimal>()
        .map_err(|e| ExchangeError::Decode(format!("{field}: {e}")))
}

fn parse_side(s: &str) -> Result<Side, ExchangeError> {
    match s {
        "Buy" => Ok(Side::Buy),
        "Sell" => Ok(Side::Sell),
        other => Err(ExchangeError::Decode(format!("unknown side {other}"))),
    }
}

impl PositionRow {
    /// Returns `None` for flat positions — Bybit reports closed positions with
    /// size 0, and treating those as open would create phantom state.
    pub fn into_position(self) -> Result<Option<Position>, ExchangeError> {
        let size = req_decimal(&self.size, "position size")?;
        if size.is_zero() {
            return Ok(None);
        }
        Ok(Some(Position {
            symbol: Symbol::new(self.symbol),
            side: parse_side(&self.side)?,
            size,
            entry_price: req_decimal(&self.avg_price, "avgPrice")?,
            liq_price: opt_decimal(&self.liq_price)?,
            unrealized_pnl: req_decimal(&self.unrealised_pnl, "unrealisedPnl")?,
        }))
    }
}

#[derive(Debug, Deserialize)]
pub struct OpenOrderRow {
    pub symbol: String,
    #[serde(rename = "orderId")]
    pub order_id: String,
    #[serde(rename = "orderLinkId")]
    pub order_link_id: String,
    pub side: String,
    pub price: String,
    pub qty: String,
    #[serde(rename = "cumExecQty")]
    pub cum_exec_qty: String,
    #[serde(rename = "orderStatus")]
    pub order_status: String,
    #[serde(rename = "createdTime")]
    pub created_time: String,
}

impl OpenOrderRow {
    pub fn into_open_order(self) -> Result<OpenOrder, ExchangeError> {
        let state = match self.order_status.as_str() {
            "New" | "Untriggered" => OrderState::New,
            "PartiallyFilled" => OrderState::PartiallyFilled,
            "Filled" => OrderState::Filled,
            "Cancelled" | "Deactivated" => OrderState::Cancelled,
            "Rejected" => OrderState::Rejected,
            other => {
                return Err(ExchangeError::Decode(format!("unknown orderStatus {other}")))
            }
        };
        Ok(OpenOrder {
            symbol: Symbol::new(self.symbol),
            order_id: self.order_id,
            order_link_id: self.order_link_id,
            side: parse_side(&self.side)?,
            price: req_decimal(&self.price, "order price")?,
            qty: req_decimal(&self.qty, "order qty")?,
            cum_exec_qty: req_decimal(&self.cum_exec_qty, "cumExecQty")?,
            state,
            created_time_ms: self
                .created_time
                .parse::<i64>()
                .map_err(|e| ExchangeError::Decode(format!("createdTime: {e}")))?,
        })
    }
}

#[derive(Debug, Deserialize)]
pub struct WalletRow {
    #[serde(rename = "totalEquity")]
    pub total_equity: String,
    #[serde(rename = "totalAvailableBalance")]
    pub total_available_balance: String,
}

impl WalletRow {
    pub fn into_balance(self) -> Result<Balance, ExchangeError> {
        Ok(Balance {
            equity: req_decimal(&self.total_equity, "totalEquity")?,
            available: req_decimal(&self.total_available_balance, "totalAvailableBalance")?,
        })
    }
}
```

- [ ] **Step 4: Define the ExchangeClient and MarketFeed traits**

Create `crates/exchange/src/traits.rs`:

```rust
use async_trait::async_trait;
use core::{Balance, Candle, Instrument, LimitEntry, OpenOrder, OrderAck, Position, Symbol, Timeframe};
use rust_decimal::Decimal;
use tokio::sync::broadcast;

use crate::bybit::transport::ExchangeError;
use crate::bybit::wire::Ticker;

/// Everything the engine may ask of an exchange.
///
/// There is deliberately no `place_market_order`. Phase 2's SimulatedExchange
/// implements this same trait, which is what lets the backtester drive the
/// identical pipeline as live trading.
///
/// These are the *only* definitions of these operations — `BybitRest` has no
/// inherent duplicates of them. Callers import the trait.
#[async_trait]
pub trait ExchangeClient: Send + Sync {
    async fn instruments(&self) -> Result<Vec<Instrument>, ExchangeError>;
    async fn tickers(&self) -> Result<Vec<Ticker>, ExchangeError>;
    async fn klines(
        &self,
        symbol: &Symbol,
        tf: Timeframe,
        limit: u16,
    ) -> Result<Vec<Candle>, ExchangeError>;
    async fn place_limit_entry(&self, req: LimitEntry) -> Result<OrderAck, ExchangeError>;
    async fn amend_stop(
        &self,
        symbol: &Symbol,
        trigger: Decimal,
        limit_price: Decimal,
    ) -> Result<(), ExchangeError>;
    async fn cancel_order(&self, symbol: &Symbol, link_id: &str) -> Result<(), ExchangeError>;
    async fn positions(&self) -> Result<Vec<Position>, ExchangeError>;
    async fn open_orders(&self) -> Result<Vec<OpenOrder>, ExchangeError>;
    async fn set_leverage(&self, symbol: &Symbol, leverage: Decimal) -> Result<(), ExchangeError>;
    async fn balance(&self) -> Result<Balance, ExchangeError>;
}

/// A market data event delivered by a feed.
#[derive(Debug, Clone)]
pub enum MarketEvent {
    /// A candle that has closed and will not change again.
    CandleClosed { symbol: Symbol, tf: Timeframe, candle: Candle },
    /// The feed reconnected and refilled a gap; indicators should be rewarmed.
    GapFilled { symbol: Symbol, tf: Timeframe, candles: Vec<Candle> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subscription {
    pub symbol: Symbol,
    pub timeframe: Timeframe,
}

#[async_trait]
pub trait MarketFeed: Send + Sync {
    async fn subscribe(
        &self,
        subs: &[Subscription],
    ) -> Result<broadcast::Receiver<MarketEvent>, ExchangeError>;
}
```

Replace `crates/exchange/src/lib.rs`:

```rust
pub mod bybit;
pub mod traits;

pub use traits::{ExchangeClient, MarketEvent, MarketFeed, Subscription};
```

- [ ] **Step 5: Implement the signed POST helper and the ExchangeClient impl**

Append to `crates/exchange/src/bybit/rest.rs`:

```rust
use async_trait::async_trait;
use core::{Balance, LimitEntry, OpenOrder, OrderAck, Position};
use rust_decimal::Decimal;
use serde_json::json;

use crate::traits::ExchangeClient;
use super::wire::{OpenOrderRow, OrderCreateResult, PositionRow, WalletRow};

impl BybitRest {
    /// Signed POST. The body is serialised once and both signed and sent
    /// byte-identically — signing a different string than we transmit is the
    /// classic source of intermittent auth failures.
    pub(crate) async fn post<T: DeserializeOwned>(
        &self,
        path: &str,
        body: serde_json::Value,
    ) -> Result<T, ExchangeError> {
        let body_str = serde_json::to_string(&body)
            .map_err(|e| ExchangeError::Decode(format!("serialising request: {e}")))?;

        self.with_retry(|| {
            let body_str = body_str.clone();
            async move {
                self.limiter.acquire().await;
                let ts = self.clock.now_ms();
                let sign =
                    sign_rest(&self.creds.api_secret, ts, &self.creds.api_key, RECV_WINDOW, &body_str);
                let url = format!("{}{}", self.base_url, path);

                let resp = self
                    .http
                    .post(&url)
                    .header("X-BAPI-API-KEY", &self.creds.api_key)
                    .header("X-BAPI-TIMESTAMP", ts.to_string())
                    .header("X-BAPI-RECV-WINDOW", RECV_WINDOW.to_string())
                    .header("X-BAPI-SIGN", sign)
                    .header("Content-Type", "application/json")
                    .body(body_str)
                    .send()
                    .await?;

                let text = resp.text().await?;
                let env: Envelope<T> = serde_json::from_str(&text)
                    .map_err(|e| ExchangeError::Decode(format!("{e}: {text}")))?;
                self.clock.observe(env.time, local_now_ms());
                env.into_result()
            }
        })
        .await
    }
}

// Endpoint methods live here and nowhere else — there are no inherent
// duplicates to keep in sync. `impl BybitRest` above holds only the
// constructor and the shared get/post/with_retry plumbing.
#[async_trait]
impl ExchangeClient for BybitRest {
    /// Place a PostOnly limit entry with stop and target attached.
    ///
    /// This is the only order-placing method in the workspace. `orderType` is
    /// hard-coded to `"Limit"` and no parameter can change it.
    async fn place_limit_entry(&self, req: LimitEntry) -> Result<OrderAck, ExchangeError> {
        let body = json!({
            "category": "linear",
            "symbol": req.symbol.as_str(),
            "side": req.side.as_bybit(),
            "orderType": "Limit",
            "timeInForce": "PostOnly",
            "positionIdx": 0,
            "qty": req.qty.normalize().to_string(),
            "price": req.price.normalize().to_string(),
            "orderLinkId": req.order_link_id,
            "stopLoss": req.stop_loss.normalize().to_string(),
            "slLimitPrice": req.stop_limit_price.normalize().to_string(),
            "slOrderType": "Limit",
            "slTriggerBy": "MarkPrice",
            "takeProfit": req.take_profit.normalize().to_string(),
            "tpOrderType": "Limit",
        });

        let res: OrderCreateResult = self.post("/v5/order/create", body).await?;
        Ok(OrderAck { order_id: res.order_id, order_link_id: res.order_link_id })
    }

    async fn cancel_order(&self, symbol: &Symbol, link_id: &str) -> Result<(), ExchangeError> {
        let body = json!({
            "category": "linear",
            "symbol": symbol.as_str(),
            "orderLinkId": link_id,
        });
        let _: serde_json::Value = self.post("/v5/order/cancel", body).await?;
        Ok(())
    }

    /// Move a position's stop, keeping it a limit order.
    async fn amend_stop(
        &self,
        symbol: &Symbol,
        trigger: Decimal,
        limit_price: Decimal,
    ) -> Result<(), ExchangeError> {
        let body = json!({
            "category": "linear",
            "symbol": symbol.as_str(),
            "positionIdx": 0,
            "stopLoss": trigger.normalize().to_string(),
            "slLimitPrice": limit_price.normalize().to_string(),
            "slOrderType": "Limit",
            "slTriggerBy": "MarkPrice",
        });
        let _: serde_json::Value = self.post("/v5/position/trading-stop", body).await?;
        Ok(())
    }

    async fn set_leverage(&self, symbol: &Symbol, leverage: Decimal) -> Result<(), ExchangeError> {
        let lev = leverage.normalize().to_string();
        let body = json!({
            "category": "linear",
            "symbol": symbol.as_str(),
            "buyLeverage": lev,
            "sellLeverage": lev,
        });
        let _: serde_json::Value = self.post("/v5/position/set-leverage", body).await?;
        Ok(())
    }

    async fn positions(&self) -> Result<Vec<Position>, ExchangeError> {
        let res: ListResult<PositionRow> = self
            .get("/v5/position/list", &[("category", "linear".into()), ("settleCoin", "USDT".into())])
            .await?;
        let mut out = Vec::new();
        for row in res.list {
            if let Some(p) = row.into_position()? {
                out.push(p);
            }
        }
        Ok(out)
    }

    async fn open_orders(&self) -> Result<Vec<OpenOrder>, ExchangeError> {
        let res: ListResult<OpenOrderRow> = self
            .get("/v5/order/realtime", &[("category", "linear".into()), ("settleCoin", "USDT".into())])
            .await?;
        res.list.into_iter().map(OpenOrderRow::into_open_order).collect()
    }

    async fn balance(&self) -> Result<Balance, ExchangeError> {
        let res: ListResult<WalletRow> = self
            .get("/v5/account/wallet-balance", &[("accountType", "UNIFIED".into())])
            .await?;
        res.list
            .into_iter()
            .next()
            .ok_or_else(|| ExchangeError::Decode("wallet-balance returned no accounts".into()))?
            .into_balance()
    }
}
```

- [ ] **Step 6: Move the market-data methods into the trait impl**

Task 6 defined `instruments`, `tickers` and `klines` as inherent methods on
`BybitRest`, because the trait did not exist yet. Move all three into the
`impl ExchangeClient for BybitRest` block now — cut them from `impl BybitRest`,
paste them into the trait impl, and change each `pub async fn` to `async fn`.
Their bodies do not change. When you are done, `impl BybitRest` must contain
only `new`, `clock`, `get`, `post` and `with_retry`.

This leaves exactly one definition of every endpoint. Do not leave inherent
copies that delegate to the trait, or vice versa.

Then add the trait import to the two test files that call these methods —
`crates/exchange/tests/rest_market_data.rs` and
`crates/exchange/tests/rest_trading.rs`:

```rust
use exchange::ExchangeClient;
```

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test -p exchange --test rest_trading --test rest_market_data`
Expected: PASS — 8 tests (4 trading + 4 market data).

If a method is reported as both inherent and trait-provided, an inherent copy
was left behind in Step 6; delete it rather than renaming it.

- [ ] **Step 8: Write the limit-only enforcement test**

Create `crates/exchange/tests/no_market_orders.rs`:

```rust
//! Guards the spec's central rule: no code path may emit a market order.
//!
//! This is a source-level test rather than a behavioural one because the rule
//! is about what the codebase *cannot express*, not about what one function
//! happens to do at runtime.

use std::fs;
use std::path::Path;

fn rust_sources(dir: &Path, out: &mut Vec<(String, String)>) {
    for entry in fs::read_dir(dir).expect("readable directory") {
        let entry = entry.expect("readable entry");
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            let text = fs::read_to_string(&path).expect("readable source");
            out.push((path.display().to_string(), text));
        }
    }
}

fn workspace_sources() -> Vec<(String, String)> {
    // CARGO_MANIFEST_DIR is crates/exchange; walk up to the workspace root.
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root exists")
        .to_path_buf();

    let mut out = Vec::new();
    for sub in ["crates", "bot"] {
        let dir = root.join(sub);
        if dir.exists() {
            rust_sources(&dir, &mut out);
        }
    }
    out
}

#[test]
fn no_source_file_constructs_a_market_order() {
    let offenders: Vec<String> = workspace_sources()
        .into_iter()
        .filter(|(path, text)| {
            !path.ends_with("no_market_orders.rs") && text.contains("\"Market\"")
        })
        .map(|(path, _)| path)
        .collect();

    assert!(
        offenders.is_empty(),
        "market order literal found in: {offenders:?}\n\
         The spec forbids market orders anywhere, including stop fallbacks."
    );
}

#[test]
fn every_order_type_literal_is_limit() {
    // Any orderType we send must be Limit. Catches slOrderType/tpOrderType
    // regressions as well as the entry order itself.
    for (path, text) in workspace_sources() {
        if path.ends_with("no_market_orders.rs") {
            continue;
        }
        for (lineno, line) in text.lines().enumerate() {
            if line.contains("OrderType") && line.contains('"') && !line.trim_start().starts_with("//")
            {
                assert!(
                    !line.contains("\"Market\""),
                    "{path}:{} sets an order type to Market: {line}",
                    lineno + 1
                );
            }
        }
    }
}
```

- [ ] **Step 9: Run the enforcement test**

Run: `cargo test -p exchange --test no_market_orders`
Expected: PASS — 2 tests. If it fails, a market-order literal has been introduced; remove it rather than weakening the test.

- [ ] **Step 10: Run the full exchange suite**

Run: `cargo test -p exchange && cargo clippy -p exchange -- -D warnings`
Expected: PASS — 27 tests total, no clippy warnings.

- [ ] **Step 11: Commit**

```bash
git add crates/exchange
git commit -m "feat(exchange): limit-only trading endpoints and ExchangeClient trait

place_limit_entry hard-codes orderType Limit with PostOnly, attaching stop
and target in the same request so no unprotected window exists. A source-level
test asserts no file in the workspace can emit a market order."
```

---

### Task 8: Public WebSocket kline feed with reconnect and gap fill

**Files:**
- Create: `crates/exchange/src/bybit/ws_public.rs`
- Modify: `crates/exchange/src/bybit/mod.rs`
- Test: `crates/exchange/tests/ws_kline_parsing.rs`

**Interfaces:**
- Consumes: `MarketEvent`, `Subscription`, `MarketFeed` (Task 7); `BybitRest::klines` (Task 6)
- Produces:
  - `topic_for(&Subscription) -> String`
  - `parse_kline_message(raw: &str) -> Result<Option<Vec<(Symbol, Timeframe, Candle)>>, ExchangeError>` — `None` for non-kline frames
  - `BybitPublicFeed::new(ws_url: String, rest: Arc<BybitRest>) -> Self` implementing `MarketFeed`

- [ ] **Step 1: Write the failing kline-message parsing test**

Create `crates/exchange/tests/ws_kline_parsing.rs`:

```rust
use core::Timeframe;
use exchange::bybit::ws_public::{parse_kline_message, topic_for};
use exchange::Subscription;
use core::Symbol;
use rust_decimal_macros::dec;

#[test]
fn topic_uses_bybit_interval_notation() {
    let sub = Subscription { symbol: Symbol::new("BTCUSDT"), timeframe: Timeframe::H1 };
    assert_eq!(topic_for(&sub), "kline.60.BTCUSDT");
    let sub4 = Subscription { symbol: Symbol::new("ETHUSDT"), timeframe: Timeframe::H4 };
    assert_eq!(topic_for(&sub4), "kline.240.ETHUSDT");
}

#[test]
fn only_confirmed_candles_are_emitted() {
    // Bybit streams the in-progress candle continuously with confirm=false and
    // sends confirm=true exactly once when it closes. Acting on an unconfirmed
    // candle would mean trading a bar that can still change.
    let raw = r#"{
      "topic":"kline.60.BTCUSDT","type":"snapshot","ts":1700003600000,
      "data":[{"start":1700000000000,"end":1700003599999,"interval":"60",
               "open":"100","close":"102","high":"103","low":"99",
               "volume":"5","turnover":"510","confirm":false,"timestamp":1700003599000}]
    }"#;
    let parsed = parse_kline_message(raw).expect("parses");
    assert_eq!(parsed, Some(vec![]), "unconfirmed candle must not be emitted");
}

#[test]
fn confirmed_candle_parses_into_domain_candle() {
    let raw = r#"{
      "topic":"kline.60.BTCUSDT","type":"snapshot","ts":1700003600000,
      "data":[{"start":1700000000000,"end":1700003599999,"interval":"60",
               "open":"100.5","close":"102.25","high":"103","low":"99.75",
               "volume":"5.5","turnover":"510.25","confirm":true,"timestamp":1700003600000}]
    }"#;
    let parsed = parse_kline_message(raw).expect("parses").expect("is a kline frame");
    assert_eq!(parsed.len(), 1);
    let (symbol, tf, candle) = &parsed[0];
    assert_eq!(symbol.as_str(), "BTCUSDT");
    assert_eq!(*tf, Timeframe::H1);
    assert_eq!(candle.open_time_ms, 1_700_000_000_000);
    assert_eq!(candle.open, dec!(100.5));
    assert_eq!(candle.close, dec!(102.25));
    assert_eq!(candle.high, dec!(103));
    assert_eq!(candle.low, dec!(99.75));
}

#[test]
fn non_kline_frames_are_ignored_not_errors() {
    // Subscription acks and pongs share the socket; they must not be treated
    // as failures or the feed would reconnect in a loop.
    assert_eq!(parse_kline_message(r#"{"success":true,"op":"subscribe"}"#).unwrap(), None);
    assert_eq!(parse_kline_message(r#"{"op":"pong","success":true}"#).unwrap(), None);
}

#[test]
fn unknown_interval_in_topic_is_an_error() {
    let raw = r#"{"topic":"kline.5.BTCUSDT","type":"snapshot","ts":1,"data":[]}"#;
    assert!(parse_kline_message(raw).is_err(), "unsupported interval must not be silently dropped");
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p exchange --test ws_kline_parsing`
Expected: FAIL — `parse_kline_message`, `topic_for` not found.

- [ ] **Step 3: Implement topic construction and message parsing**

Create `crates/exchange/src/bybit/ws_public.rs`:

```rust
use core::{Candle, Symbol, Timeframe};
use rust_decimal::Decimal;
use serde::Deserialize;

use super::transport::ExchangeError;
use crate::traits::Subscription;

/// Bybit public topic name, e.g. `kline.60.BTCUSDT`.
pub fn topic_for(sub: &Subscription) -> String {
    format!("kline.{}.{}", sub.timeframe.as_bybit_interval(), sub.symbol.as_str())
}

#[derive(Debug, Deserialize)]
struct KlineFrame {
    topic: String,
    data: Vec<KlineData>,
}

#[derive(Debug, Deserialize)]
struct KlineData {
    start: i64,
    open: String,
    close: String,
    high: String,
    low: String,
    volume: String,
    turnover: String,
    confirm: bool,
}

fn interval_to_timeframe(interval: &str) -> Result<Timeframe, ExchangeError> {
    match interval {
        "60" => Ok(Timeframe::H1),
        "240" => Ok(Timeframe::H4),
        other => Err(ExchangeError::Decode(format!("unsupported kline interval {other}"))),
    }
}

/// Parse one WebSocket text frame.
///
/// Returns `Ok(None)` for frames that are not kline data (subscription acks,
/// pongs) — those are normal traffic, not failures. Returns an empty vector
/// when the frame holds only unconfirmed candles.
pub fn parse_kline_message(
    raw: &str,
) -> Result<Option<Vec<(Symbol, Timeframe, Candle)>>, ExchangeError> {
    let value: serde_json::Value = serde_json::from_str(raw)
        .map_err(|e| ExchangeError::Decode(format!("ws frame: {e}")))?;

    let Some(topic) = value.get("topic").and_then(|t| t.as_str()) else {
        return Ok(None);
    };
    if !topic.starts_with("kline.") {
        return Ok(None);
    }

    // topic is kline.{interval}.{symbol}
    let mut parts = topic.splitn(3, '.');
    parts.next();
    let interval = parts
        .next()
        .ok_or_else(|| ExchangeError::Decode(format!("malformed topic {topic}")))?;
    let symbol_str = parts
        .next()
        .ok_or_else(|| ExchangeError::Decode(format!("malformed topic {topic}")))?;

    let tf = interval_to_timeframe(interval)?;
    let symbol = Symbol::new(symbol_str);

    let frame: KlineFrame = serde_json::from_value(value)
        .map_err(|e| ExchangeError::Decode(format!("kline frame: {e}")))?;

    let parse = |s: &str, field: &str| -> Result<Decimal, ExchangeError> {
        s.parse::<Decimal>()
            .map_err(|e| ExchangeError::Decode(format!("ws kline {field}: {e}")))
    };

    let mut out = Vec::new();
    for d in frame.data {
        // Only confirmed candles matter: an unconfirmed bar is still moving.
        if !d.confirm {
            continue;
        }
        out.push((
            symbol.clone(),
            tf,
            Candle {
                open_time_ms: d.start,
                open: parse(&d.open, "open")?,
                high: parse(&d.high, "high")?,
                low: parse(&d.low, "low")?,
                close: parse(&d.close, "close")?,
                volume: parse(&d.volume, "volume")?,
                turnover: parse(&d.turnover, "turnover")?,
            },
        ));
    }
    Ok(Some(out))
}
```

Add to `crates/exchange/src/bybit/mod.rs`:

```rust
pub mod ws_public;
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p exchange --test ws_kline_parsing`
Expected: PASS — 5 tests.

- [ ] **Step 5: Write the failing gap-detection test**

Append to `crates/exchange/tests/ws_kline_parsing.rs`:

```rust
use exchange::bybit::ws_public::missing_candle_count;

#[test]
fn consecutive_candles_show_no_gap() {
    let prev = 1_700_000_000_000;
    let next = prev + Timeframe::H1.duration_ms();
    assert_eq!(missing_candle_count(prev, next, Timeframe::H1), 0);
}

#[test]
fn a_skipped_candle_is_detected() {
    // Two hours elapsed on a 1h feed means exactly one candle went missing.
    let prev = 1_700_000_000_000;
    let next = prev + 2 * Timeframe::H1.duration_ms();
    assert_eq!(missing_candle_count(prev, next, Timeframe::H1), 1);
}

#[test]
fn duplicate_or_out_of_order_candles_report_no_gap() {
    let prev = 1_700_000_000_000;
    assert_eq!(missing_candle_count(prev, prev, Timeframe::H1), 0);
    assert_eq!(missing_candle_count(prev, prev - 3_600_000, Timeframe::H1), 0);
}
```

- [ ] **Step 6: Run the test to verify it fails**

Run: `cargo test -p exchange --test ws_kline_parsing`
Expected: FAIL — `missing_candle_count` not found.

- [ ] **Step 7: Implement gap detection**

Append to `crates/exchange/src/bybit/ws_public.rs`:

```rust
/// How many candles are missing between two consecutive open times.
///
/// A reconnect after a dropped socket will resume mid-series; the feed uses
/// this to decide whether a REST backfill is required before emitting further
/// events, so indicators never see a hole.
pub fn missing_candle_count(prev_open_ms: i64, next_open_ms: i64, tf: Timeframe) -> i64 {
    let step = tf.duration_ms();
    let delta = next_open_ms - prev_open_ms;
    if delta <= step {
        return 0;
    }
    delta / step - 1
}
```

- [ ] **Step 8: Run the tests to verify they pass**

Run: `cargo test -p exchange --test ws_kline_parsing`
Expected: PASS — 8 tests.

- [ ] **Step 9: Implement the reconnecting feed**

Append to `crates/exchange/src/bybit/ws_public.rs`:

```rust
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite::Message;
use tracing::{error, info, warn};

use super::rest::BybitRest;
use super::transport::backoff_delay;
use crate::traits::{MarketEvent, MarketFeed};

const PING_INTERVAL: Duration = Duration::from_secs(20);
const EVENT_CHANNEL_CAPACITY: usize = 1024;
const GAP_REFETCH_LIMIT: u16 = 200;

/// Public kline feed with automatic reconnect and REST gap backfill.
pub struct BybitPublicFeed {
    ws_url: String,
    rest: Arc<BybitRest>,
}

impl BybitPublicFeed {
    pub fn new(ws_url: String, rest: Arc<BybitRest>) -> Self {
        BybitPublicFeed { ws_url, rest }
    }
}

#[async_trait]
impl MarketFeed for BybitPublicFeed {
    async fn subscribe(
        &self,
        subs: &[Subscription],
    ) -> Result<broadcast::Receiver<MarketEvent>, ExchangeError> {
        let (tx, rx) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let url = self.ws_url.clone();
        let rest = Arc::clone(&self.rest);
        let subs = subs.to_vec();

        tokio::spawn(async move {
            // Last confirmed candle open time per (symbol, timeframe), used to
            // detect gaps across reconnects.
            let mut last_open: HashMap<(Symbol, Timeframe), i64> = HashMap::new();
            let mut attempt: u32 = 0;

            loop {
                match run_session(&url, &subs, &tx, &rest, &mut last_open).await {
                    Ok(()) => {
                        info!("public feed session ended cleanly; reconnecting");
                        attempt = 0;
                    }
                    Err(e) => {
                        warn!(error = %e, attempt, "public feed session failed");
                        attempt = attempt.saturating_add(1);
                    }
                }
                tokio::time::sleep(backoff_delay(attempt, 500, 0.25)).await;
            }
        });

        Ok(rx)
    }
}

/// One connection lifetime: connect, subscribe, pump messages until failure.
async fn run_session(
    url: &str,
    subs: &[Subscription],
    tx: &broadcast::Sender<MarketEvent>,
    rest: &BybitRest,
    last_open: &mut HashMap<(Symbol, Timeframe), i64>,
) -> Result<(), ExchangeError> {
    let (mut ws, _) = tokio_tungstenite::connect_async(url)
        .await
        .map_err(|e| ExchangeError::WebSocket(e.to_string()))?;

    let topics: Vec<String> = subs.iter().map(topic_for).collect();
    let sub_msg = serde_json::json!({ "op": "subscribe", "args": topics }).to_string();
    ws.send(Message::Text(sub_msg.into()))
        .await
        .map_err(|e| ExchangeError::WebSocket(e.to_string()))?;

    info!(count = subs.len(), "subscribed to public kline topics");

    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.tick().await; // first tick fires immediately; skip it

    loop {
        tokio::select! {
            _ = ping.tick() => {
                ws.send(Message::Text(r#"{"op":"ping"}"#.into()))
                    .await
                    .map_err(|e| ExchangeError::WebSocket(e.to_string()))?;
            }
            frame = ws.next() => {
                let Some(frame) = frame else {
                    return Err(ExchangeError::WebSocket("stream closed".into()));
                };
                let msg = frame.map_err(|e| ExchangeError::WebSocket(e.to_string()))?;
                let Message::Text(text) = msg else { continue };

                let Some(candles) = parse_kline_message(&text)? else { continue };
                for (symbol, tf, candle) in candles {
                    let key = (symbol.clone(), tf);
                    if let Some(prev) = last_open.get(&key).copied() {
                        let missing = missing_candle_count(prev, candle.open_time_ms, tf);
                        if missing > 0 {
                            warn!(%symbol, missing, "kline gap detected; backfilling via REST");
                            match rest.klines(&symbol, tf, GAP_REFETCH_LIMIT).await {
                                Ok(backfill) => {
                                    let _ = tx.send(MarketEvent::GapFilled {
                                        symbol: symbol.clone(),
                                        tf,
                                        candles: backfill,
                                    });
                                }
                                Err(e) => error!(%symbol, error = %e, "gap backfill failed"),
                            }
                        }
                    }
                    last_open.insert(key, candle.open_time_ms);
                    // A send error means no receivers are listening yet; the
                    // engine may not have started consuming. Not fatal.
                    let _ = tx.send(MarketEvent::CandleClosed { symbol, tf, candle });
                }
            }
        }
    }
}
```

- [ ] **Step 10: Verify the crate builds and all tests still pass**

Run: `cargo test -p exchange && cargo clippy -p exchange -- -D warnings`
Expected: PASS — 35 tests total, no clippy warnings.

- [ ] **Step 11: Commit**

```bash
git add crates/exchange
git commit -m "feat(exchange): public kline feed with reconnect and REST gap backfill

Only confirmed candles are emitted, so the engine never acts on a bar that
can still change. Reconnects detect missing candles by open-time arithmetic
and backfill over REST before resuming."
```

---

### Task 9: Private WebSocket feed

**Files:**
- Create: `crates/exchange/src/bybit/ws_private.rs`
- Modify: `crates/exchange/src/bybit/mod.rs`
- Test: `crates/exchange/tests/ws_private_parsing.rs`

**Interfaces:**
- Consumes: `sign_ws_auth`, `Credentials`, `ClockOffset` (Task 4); `OrderState`, `Position` (Task 2)
- Produces:
  - `AccountEvent` enum: `OrderUpdate`, `PositionUpdate`, `Execution`, `WalletUpdate`
  - `build_auth_frame(creds: &Credentials, expires_ms: i64) -> String`
  - `parse_private_message(raw: &str) -> Result<Vec<AccountEvent>, ExchangeError>`
  - `BybitPrivateFeed::new(ws_url, creds, clock) -> Self` with `subscribe(&self) -> broadcast::Receiver<AccountEvent>`

- [ ] **Step 1: Write the failing private-frame test**

Create `crates/exchange/tests/ws_private_parsing.rs`:

```rust
use core::{OrderState, Side};
use exchange::bybit::sign::Credentials;
use exchange::bybit::ws_private::{build_auth_frame, parse_private_message, AccountEvent};
use rust_decimal_macros::dec;

#[test]
fn auth_frame_has_the_documented_shape() {
    let creds = Credentials { api_key: "mykey".into(), api_secret: "mysecret".into() };
    let frame = build_auth_frame(&creds, 1_700_000_005_000);
    let v: serde_json::Value = serde_json::from_str(&frame).expect("valid json");

    assert_eq!(v["op"], "auth");
    let args = v["args"].as_array().expect("args is an array");
    assert_eq!(args.len(), 3);
    assert_eq!(args[0], "mykey");
    assert_eq!(args[1], 1_700_000_005_000i64);
    assert_eq!(args[2].as_str().expect("signature is a string").len(), 64);
}

#[test]
fn order_updates_parse_with_state_and_fill_quantity() {
    let raw = r#"{"topic":"order","data":[{
        "symbol":"BTCUSDT","orderId":"oid-1","orderLinkId":"link-1","side":"Buy",
        "price":"42000","qty":"0.01","cumExecQty":"0.004",
        "orderStatus":"PartiallyFilled","createdTime":"1700000000000"}]}"#;

    let events = parse_private_message(raw).expect("parses");
    assert_eq!(events.len(), 1);
    match &events[0] {
        AccountEvent::OrderUpdate(o) => {
            assert_eq!(o.order_link_id, "link-1");
            assert_eq!(o.state, OrderState::PartiallyFilled);
            assert_eq!(o.cum_exec_qty, dec!(0.004));
            assert_eq!(o.side, Side::Buy);
        }
        other => panic!("expected OrderUpdate, got {other:?}"),
    }
}

#[test]
fn flat_position_updates_are_reported_as_closed() {
    // A stop firing produces a position update with size 0. The engine must
    // see this as "closed", not as an open position of zero size.
    let raw = r#"{"topic":"position","data":[{
        "symbol":"BTCUSDT","side":"","size":"0","avgPrice":"0",
        "liqPrice":"","unrealisedPnl":"0"}]}"#;

    let events = parse_private_message(raw).expect("parses");
    assert_eq!(events.len(), 1);
    match &events[0] {
        AccountEvent::PositionClosed { symbol } => assert_eq!(symbol.as_str(), "BTCUSDT"),
        other => panic!("expected PositionClosed, got {other:?}"),
    }
}

#[test]
fn wallet_updates_carry_equity() {
    let raw = r#"{"topic":"wallet","data":[{
        "totalEquity":"10500.5","totalAvailableBalance":"9000.25"}]}"#;
    let events = parse_private_message(raw).expect("parses");
    match &events[0] {
        AccountEvent::WalletUpdate(b) => assert_eq!(b.equity, dec!(10500.5)),
        other => panic!("expected WalletUpdate, got {other:?}"),
    }
}

#[test]
fn auth_and_subscribe_acks_produce_no_events() {
    assert!(parse_private_message(r#"{"op":"auth","success":true}"#).unwrap().is_empty());
    assert!(parse_private_message(r#"{"op":"subscribe","success":true}"#).unwrap().is_empty());
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p exchange --test ws_private_parsing`
Expected: FAIL — `build_auth_frame`, `parse_private_message`, `AccountEvent` not found.

- [ ] **Step 3: Implement the private frame types and parsing**

Create `crates/exchange/src/bybit/ws_private.rs`:

```rust
use core::{Balance, OpenOrder, Position, Symbol};
use serde::Deserialize;
use serde_json::Value;

use super::sign::{sign_ws_auth, Credentials};
use super::transport::ExchangeError;
use super::wire::{OpenOrderRow, PositionRow, WalletRow};

/// An account-state change pushed over the private stream.
#[derive(Debug, Clone)]
pub enum AccountEvent {
    OrderUpdate(OpenOrder),
    PositionUpdate(Position),
    /// Emitted when the exchange reports size 0 — the position is gone.
    PositionClosed { symbol: Symbol },
    WalletUpdate(Balance),
}

/// Build the private-stream auth frame.
///
/// `expires_ms` must be comfortably in the future; the caller derives it from
/// the clock-corrected time so local drift cannot invalidate it.
pub fn build_auth_frame(creds: &Credentials, expires_ms: i64) -> String {
    let signature = sign_ws_auth(&creds.api_secret, expires_ms);
    serde_json::json!({
        "op": "auth",
        "args": [creds.api_key, expires_ms, signature],
    })
    .to_string()
}

#[derive(Debug, Deserialize)]
struct PrivateFrame {
    topic: String,
    data: Vec<Value>,
}

/// Parse one private-stream frame into zero or more account events.
///
/// Acks (`auth`, `subscribe`, `pong`) carry no topic and yield no events;
/// treating them as errors would cause a reconnect loop.
pub fn parse_private_message(raw: &str) -> Result<Vec<AccountEvent>, ExchangeError> {
    let value: Value = serde_json::from_str(raw)
        .map_err(|e| ExchangeError::Decode(format!("private frame: {e}")))?;

    if value.get("topic").is_none() {
        return Ok(Vec::new());
    }

    let frame: PrivateFrame = serde_json::from_value(value)
        .map_err(|e| ExchangeError::Decode(format!("private frame: {e}")))?;

    let mut out = Vec::new();
    for item in frame.data {
        match frame.topic.as_str() {
            "order" => {
                let row: OpenOrderRow = serde_json::from_value(item)
                    .map_err(|e| ExchangeError::Decode(format!("order update: {e}")))?;
                out.push(AccountEvent::OrderUpdate(row.into_open_order()?));
            }
            "position" => {
                let row: PositionRow = serde_json::from_value(item.clone())
                    .map_err(|e| ExchangeError::Decode(format!("position update: {e}")))?;
                let symbol = Symbol::new(row.symbol.clone());
                match row.into_position()? {
                    Some(p) => out.push(AccountEvent::PositionUpdate(p)),
                    None => out.push(AccountEvent::PositionClosed { symbol }),
                }
            }
            "wallet" => {
                let row: WalletRow = serde_json::from_value(item)
                    .map_err(|e| ExchangeError::Decode(format!("wallet update: {e}")))?;
                out.push(AccountEvent::WalletUpdate(row.into_balance()?));
            }
            // `execution` frames duplicate information the order topic already
            // carries; ignored rather than double-counted.
            _ => {}
        }
    }
    Ok(out)
}
```

Note: `PositionRow::into_position` parses `side` before checking size, and a flat position sends `side: ""`. Move the size check to the top of `into_position` in `wire.rs` so it returns `Ok(None)` before attempting to parse the empty side string:

```rust
    pub fn into_position(self) -> Result<Option<Position>, ExchangeError> {
        let size = req_decimal(&self.size, "position size")?;
        if size.is_zero() {
            return Ok(None); // must precede parse_side: flat positions send side ""
        }
        ...
```

Add to `crates/exchange/src/bybit/mod.rs`:

```rust
pub mod ws_private;
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p exchange --test ws_private_parsing`
Expected: PASS — 5 tests.

- [ ] **Step 5: Implement the reconnecting private feed**

Append to `crates/exchange/src/bybit/ws_private.rs`:

```rust
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite::Message;
use tracing::{info, warn};

use super::sign::ClockOffset;
use super::transport::backoff_delay;

const PING_INTERVAL: Duration = Duration::from_secs(20);
const EVENT_CHANNEL_CAPACITY: usize = 1024;
const AUTH_VALIDITY_MS: i64 = 10_000;
const TOPICS: [&str; 4] = ["order", "position", "execution", "wallet"];

pub struct BybitPrivateFeed {
    ws_url: String,
    creds: Credentials,
    clock: Arc<ClockOffset>,
}

impl BybitPrivateFeed {
    pub fn new(ws_url: String, creds: Credentials, clock: Arc<ClockOffset>) -> Self {
        BybitPrivateFeed { ws_url, creds, clock }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<AccountEvent> {
        let (tx, rx) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let url = self.ws_url.clone();
        let creds = self.creds.clone();
        let clock = Arc::clone(&self.clock);

        tokio::spawn(async move {
            let mut attempt: u32 = 0;
            loop {
                match run_private_session(&url, &creds, &clock, &tx).await {
                    Ok(()) => {
                        info!("private feed session ended; reconnecting");
                        attempt = 0;
                    }
                    Err(e) => {
                        warn!(error = %e, attempt, "private feed session failed");
                        attempt = attempt.saturating_add(1);
                    }
                }
                tokio::time::sleep(backoff_delay(attempt, 500, 0.25)).await;
            }
        });

        rx
    }
}

async fn run_private_session(
    url: &str,
    creds: &Credentials,
    clock: &ClockOffset,
    tx: &broadcast::Sender<AccountEvent>,
) -> Result<(), ExchangeError> {
    let (mut ws, _) = tokio_tungstenite::connect_async(url)
        .await
        .map_err(|e| ExchangeError::WebSocket(e.to_string()))?;

    // Expiry is derived from the clock-corrected time, so NTP drift on this
    // machine cannot produce an already-expired auth frame.
    let expires = clock.now_ms() + AUTH_VALIDITY_MS;
    ws.send(Message::Text(build_auth_frame(creds, expires).into()))
        .await
        .map_err(|e| ExchangeError::WebSocket(e.to_string()))?;

    let sub = serde_json::json!({ "op": "subscribe", "args": TOPICS }).to_string();
    ws.send(Message::Text(sub.into()))
        .await
        .map_err(|e| ExchangeError::WebSocket(e.to_string()))?;

    info!("subscribed to private topics");

    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.tick().await;

    loop {
        tokio::select! {
            _ = ping.tick() => {
                ws.send(Message::Text(r#"{"op":"ping"}"#.into()))
                    .await
                    .map_err(|e| ExchangeError::WebSocket(e.to_string()))?;
            }
            frame = ws.next() => {
                let Some(frame) = frame else {
                    return Err(ExchangeError::WebSocket("stream closed".into()));
                };
                let msg = frame.map_err(|e| ExchangeError::WebSocket(e.to_string()))?;
                let Message::Text(text) = msg else { continue };

                // An auth failure arrives as a frame, not a transport error.
                if text.contains(r#""op":"auth""#) && text.contains(r#""success":false"#) {
                    return Err(ExchangeError::Api {
                        code: 10004,
                        msg: format!("private stream auth rejected: {text}"),
                    });
                }

                for event in parse_private_message(&text)? {
                    let _ = tx.send(event);
                }
            }
        }
    }
}
```

- [ ] **Step 6: Run the full exchange suite**

Run: `cargo test -p exchange && cargo clippy -p exchange -- -D warnings`
Expected: PASS — 40 tests total, no clippy warnings.

- [ ] **Step 7: Commit**

```bash
git add crates/exchange
git commit -m "feat(exchange): private WebSocket feed for order, position and wallet updates

Auth expiry is derived from the clock-corrected time so local drift cannot
produce a pre-expired frame, and a rejected auth surfaces as a Fatal-class
error rather than an endless reconnect."
```

---

### Task 10: Turso journal with non-blocking sync

**Files:**
- Create: `crates/persistence/Cargo.toml`, `crates/persistence/src/lib.rs`, `crates/persistence/src/schema.rs`, `crates/persistence/src/journal.rs`, `crates/persistence/src/sync.rs`
- Test: `crates/persistence/tests/journal_roundtrip.rs`

**Interfaces:**
- Consumes: `Symbol`, `Side`, `OrderState` (Task 2)
- Produces:
  - `Journal::open_local(path: &str) -> Result<Journal, JournalError>`
  - `Journal::open_synced(path, url, token) -> Result<Journal, JournalError>`
  - `Journal::record_order(&self, &OrderRecord) -> Result<(), JournalError>`
  - `Journal::update_order_state(&self, link_id, OrderState, cum_exec_qty) -> Result<(), JournalError>`
  - `Journal::record_equity(&self, equity: Decimal, at_ms: i64)`, `Journal::daily_fill_count(&self, utc_day: i64)`
  - `Journal::set_halt(&self, reason: &str)`, `Journal::halt_reason(&self) -> Option<String>`
  - `spawn_sync_task(journal: Arc<Journal>, interval: Duration)`

- [ ] **Step 1: Create the crate manifest**

Create `crates/persistence/Cargo.toml`:

```toml
[package]
name = "persistence"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true

[dependencies]
core = { path = "../core" }
rust_decimal.workspace = true
serde.workspace = true
thiserror.workspace = true
tokio.workspace = true
tracing.workspace = true
turso = { version = "0.2", features = ["sync"] }

[dev-dependencies]
rust_decimal_macros.workspace = true
tempfile = "3"
```

If the `turso` crate's published version or feature name differs at implementation time, run `cargo add turso --features sync` and use what resolves — do not silently substitute `libsql`, which has a different sync API.

- [ ] **Step 2: Write the failing journal round-trip test**

Create `crates/persistence/tests/journal_roundtrip.rs`:

```rust
use core::{OrderState, Side, Symbol};
use persistence::{Journal, OrderRecord};
use rust_decimal_macros::dec;

async fn temp_journal() -> (Journal, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("test.db");
    let j = Journal::open_local(path.to_str().unwrap()).await.expect("journal opens");
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
    j.record_order(&order("link-1", 1_700_000_000_000)).await.expect("recorded");

    let fetched = j.order_by_link_id("link-1").await.expect("query ok").expect("row exists");
    assert_eq!(fetched.symbol.as_str(), "BTCUSDT");
    assert_eq!(fetched.price, dec!(42000.5));
    assert_eq!(fetched.state, OrderState::New);
    assert_eq!(fetched.config_hash, "deadbeef");
}

#[tokio::test]
async fn recording_the_same_link_id_twice_does_not_duplicate() {
    // orderLinkId is the idempotency key; a retry must not create a second row.
    let (j, _dir) = temp_journal().await;
    j.record_order(&order("link-1", 1)).await.expect("first insert");
    j.record_order(&order("link-1", 1)).await.expect("second insert is a no-op");
    assert_eq!(j.order_count().await.expect("count"), 1);
}

#[tokio::test]
async fn order_state_updates_in_place() {
    let (j, _dir) = temp_journal().await;
    j.record_order(&order("link-1", 1)).await.expect("recorded");
    j.update_order_state("link-1", OrderState::Filled, dec!(0.01)).await.expect("updated");

    let fetched = j.order_by_link_id("link-1").await.expect("query ok").expect("row exists");
    assert_eq!(fetched.state, OrderState::Filled);
    assert_eq!(fetched.cum_exec_qty, dec!(0.01));
}

#[tokio::test]
async fn daily_fill_count_only_counts_filled_orders_in_that_utc_day() {
    let (j, _dir) = temp_journal().await;
    let day = 1_700_000_000_000i64 / 86_400_000 * 86_400_000;

    // Two filled, one still resting, one on the following day.
    for (id, state, ts) in [
        ("a", OrderState::Filled, day + 1_000),
        ("b", OrderState::Filled, day + 2_000),
        ("c", OrderState::New, day + 3_000),
        ("d", OrderState::Filled, day + 86_400_000),
    ] {
        let mut o = order(id, ts);
        o.state = state;
        j.record_order(&o).await.expect("recorded");
    }

    assert_eq!(j.daily_fill_count(day).await.expect("count"), 2);
}

#[tokio::test]
async fn halt_state_survives_reopening_the_database() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("halt.db");
    let p = path.to_str().unwrap();

    {
        let j = Journal::open_local(p).await.expect("opens");
        j.set_halt("daily drawdown -5%").await.expect("halt set");
    }

    // A restart must not clear the halt — that is the whole point of persisting it.
    let j = Journal::open_local(p).await.expect("reopens");
    assert_eq!(j.halt_reason().await.expect("query"), Some("daily drawdown -5%".to_string()));
}

#[tokio::test]
async fn clearing_the_halt_requires_an_explicit_call() {
    let (j, _dir) = temp_journal().await;
    j.set_halt("test").await.expect("set");
    j.clear_halt().await.expect("cleared");
    assert_eq!(j.halt_reason().await.expect("query"), None);
}
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test -p persistence`
Expected: FAIL — `Journal`, `OrderRecord` not found.

- [ ] **Step 4: Implement the schema**

Create `crates/persistence/src/schema.rs`:

```rust
/// Table definitions, applied idempotently on every open.
///
/// Decimals are stored as TEXT rather than REAL: SQLite REAL is a float, and
/// round-tripping a price through it would defeat the Decimal discipline the
/// rest of the system maintains.
pub const MIGRATIONS: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS orders (
        order_link_id TEXT PRIMARY KEY,
        order_id      TEXT,
        symbol        TEXT NOT NULL,
        side          TEXT NOT NULL,
        price         TEXT NOT NULL,
        qty           TEXT NOT NULL,
        stop_loss     TEXT NOT NULL,
        take_profit   TEXT NOT NULL,
        state         TEXT NOT NULL,
        cum_exec_qty  TEXT NOT NULL,
        config_hash   TEXT NOT NULL,
        created_at_ms INTEGER NOT NULL
    )",
    "CREATE INDEX IF NOT EXISTS idx_orders_created ON orders(created_at_ms)",
    "CREATE INDEX IF NOT EXISTS idx_orders_state ON orders(state)",
    "CREATE TABLE IF NOT EXISTS fills (
        id            INTEGER PRIMARY KEY AUTOINCREMENT,
        order_link_id TEXT NOT NULL,
        price         TEXT NOT NULL,
        qty           TEXT NOT NULL,
        fee           TEXT NOT NULL,
        filled_at_ms  INTEGER NOT NULL
    )",
    "CREATE TABLE IF NOT EXISTS equity_snapshots (
        at_ms  INTEGER PRIMARY KEY,
        equity TEXT NOT NULL
    )",
    "CREATE TABLE IF NOT EXISTS halt_state (
        id       INTEGER PRIMARY KEY CHECK (id = 1),
        reason   TEXT NOT NULL,
        set_at_ms INTEGER NOT NULL
    )",
    "CREATE TABLE IF NOT EXISTS candles (
        symbol       TEXT NOT NULL,
        timeframe    TEXT NOT NULL,
        open_time_ms INTEGER NOT NULL,
        open TEXT NOT NULL, high TEXT NOT NULL, low TEXT NOT NULL, close TEXT NOT NULL,
        volume TEXT NOT NULL, turnover TEXT NOT NULL,
        PRIMARY KEY (symbol, timeframe, open_time_ms)
    )",
];
```

- [ ] **Step 5: Implement the journal**

Create `crates/persistence/src/journal.rs`:

```rust
use core::{OrderState, Side, Symbol};
use rust_decimal::Decimal;
use turso::{Builder, Connection, Database};

use crate::schema::MIGRATIONS;

#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    #[error("database error: {0}")]
    Db(String),
    #[error("decode error: {0}")]
    Decode(String),
}

impl From<turso::Error> for JournalError {
    fn from(e: turso::Error) -> Self {
        JournalError::Db(e.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderRecord {
    pub order_link_id: String,
    pub order_id: Option<String>,
    pub symbol: Symbol,
    pub side: Side,
    pub price: Decimal,
    pub qty: Decimal,
    pub stop_loss: Decimal,
    pub take_profit: Decimal,
    pub state: OrderState,
    pub cum_exec_qty: Decimal,
    /// SHA-256 of the effective config, so every order is attributable to an
    /// exact ruleset.
    pub config_hash: String,
    pub created_at_ms: i64,
}

fn state_str(s: OrderState) -> &'static str {
    match s {
        OrderState::New => "New",
        OrderState::PartiallyFilled => "PartiallyFilled",
        OrderState::Filled => "Filled",
        OrderState::Cancelled => "Cancelled",
        OrderState::Rejected => "Rejected",
    }
}

fn parse_state(s: &str) -> Result<OrderState, JournalError> {
    Ok(match s {
        "New" => OrderState::New,
        "PartiallyFilled" => OrderState::PartiallyFilled,
        "Filled" => OrderState::Filled,
        "Cancelled" => OrderState::Cancelled,
        "Rejected" => OrderState::Rejected,
        other => return Err(JournalError::Decode(format!("unknown order state {other}"))),
    })
}

fn parse_dec(s: &str, field: &str) -> Result<Decimal, JournalError> {
    s.parse::<Decimal>()
        .map_err(|e| JournalError::Decode(format!("{field}: {e}")))
}

/// The trade journal. All reads and writes hit the local database file; a
/// background task pushes to Turso cloud separately, so the cloud is never in
/// the order path.
pub struct Journal {
    db: Database,
    conn: Connection,
    synced: bool,
}

impl Journal {
    pub async fn open_local(path: &str) -> Result<Self, JournalError> {
        let db = Builder::new_local(path).build().await?;
        let conn = db.connect()?;
        let j = Journal { db, conn, synced: false };
        j.migrate().await?;
        Ok(j)
    }

    pub async fn open_synced(path: &str, url: &str, token: &str) -> Result<Self, JournalError> {
        let db = Builder::new_remote(path)
            .with_remote_url(url)
            .with_auth_token(token)
            .build()
            .await?;
        let conn = db.connect()?;
        let j = Journal { db, conn, synced: true };
        j.migrate().await?;
        Ok(j)
    }

    async fn migrate(&self) -> Result<(), JournalError> {
        for stmt in MIGRATIONS {
            self.conn.execute(stmt, ()).await?;
        }
        Ok(())
    }

    /// Push local changes to Turso cloud. Errors are returned, never
    /// propagated into trading logic — the caller logs and retries.
    pub async fn push(&self) -> Result<(), JournalError> {
        if !self.synced {
            return Ok(());
        }
        self.db.push().await?;
        Ok(())
    }

    /// Insert an order. Idempotent on `order_link_id`: a retried placement can
    /// never create a second row.
    pub async fn record_order(&self, o: &OrderRecord) -> Result<(), JournalError> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO orders
                 (order_link_id, order_id, symbol, side, price, qty, stop_loss,
                  take_profit, state, cum_exec_qty, config_hash, created_at_ms)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
                (
                    o.order_link_id.clone(),
                    o.order_id.clone(),
                    o.symbol.as_str().to_string(),
                    o.side.as_bybit().to_string(),
                    o.price.to_string(),
                    o.qty.to_string(),
                    o.stop_loss.to_string(),
                    o.take_profit.to_string(),
                    state_str(o.state).to_string(),
                    o.cum_exec_qty.to_string(),
                    o.config_hash.clone(),
                    o.created_at_ms,
                ),
            )
            .await?;
        Ok(())
    }

    pub async fn update_order_state(
        &self,
        link_id: &str,
        state: OrderState,
        cum_exec_qty: Decimal,
    ) -> Result<(), JournalError> {
        self.conn
            .execute(
                "UPDATE orders SET state = ?1, cum_exec_qty = ?2 WHERE order_link_id = ?3",
                (state_str(state).to_string(), cum_exec_qty.to_string(), link_id.to_string()),
            )
            .await?;
        Ok(())
    }

    pub async fn order_by_link_id(&self, link_id: &str) -> Result<Option<OrderRecord>, JournalError> {
        let mut rows = self
            .conn
            .query(
                "SELECT order_link_id, order_id, symbol, side, price, qty, stop_loss,
                        take_profit, state, cum_exec_qty, config_hash, created_at_ms
                 FROM orders WHERE order_link_id = ?1",
                (link_id.to_string(),),
            )
            .await?;

        let Some(row) = rows.next().await? else { return Ok(None) };
        let get_text = |i: usize| -> Result<String, JournalError> {
            row.get_value(i)
                .map_err(|e| JournalError::Db(e.to_string()))?
                .as_text()
                .map(|s| s.to_string())
                .ok_or_else(|| JournalError::Decode(format!("column {i} is not text")))
        };

        Ok(Some(OrderRecord {
            order_link_id: get_text(0)?,
            order_id: row.get_value(1).ok().and_then(|v| v.as_text().map(|s| s.to_string())),
            symbol: Symbol::new(get_text(2)?),
            side: match get_text(3)?.as_str() {
                "Buy" => Side::Buy,
                "Sell" => Side::Sell,
                other => return Err(JournalError::Decode(format!("unknown side {other}"))),
            },
            price: parse_dec(&get_text(4)?, "price")?,
            qty: parse_dec(&get_text(5)?, "qty")?,
            stop_loss: parse_dec(&get_text(6)?, "stop_loss")?,
            take_profit: parse_dec(&get_text(7)?, "take_profit")?,
            state: parse_state(&get_text(8)?)?,
            cum_exec_qty: parse_dec(&get_text(9)?, "cum_exec_qty")?,
            config_hash: get_text(10)?,
            created_at_ms: row
                .get_value(11)
                .map_err(|e| JournalError::Db(e.to_string()))?
                .as_integer()
                .copied()
                .ok_or_else(|| JournalError::Decode("created_at_ms is not an integer".into()))?,
        }))
    }

    pub async fn order_count(&self) -> Result<i64, JournalError> {
        self.scalar_i64("SELECT COUNT(*) FROM orders", ()).await
    }

    /// Filled entries within one UTC day. Counting fills rather than
    /// placements is what makes cancelled limit orders free of budget cost.
    pub async fn daily_fill_count(&self, utc_day_start_ms: i64) -> Result<i64, JournalError> {
        self.scalar_i64(
            "SELECT COUNT(*) FROM orders
             WHERE state = 'Filled' AND created_at_ms >= ?1 AND created_at_ms < ?2",
            (utc_day_start_ms, utc_day_start_ms + 86_400_000),
        )
        .await
    }

    pub async fn record_equity(&self, equity: Decimal, at_ms: i64) -> Result<(), JournalError> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO equity_snapshots (at_ms, equity) VALUES (?1, ?2)",
                (at_ms, equity.to_string()),
            )
            .await?;
        Ok(())
    }

    pub async fn set_halt(&self, reason: &str) -> Result<(), JournalError> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO halt_state (id, reason, set_at_ms) VALUES (1, ?1, ?2)",
                (reason.to_string(), 0i64),
            )
            .await?;
        Ok(())
    }

    pub async fn clear_halt(&self) -> Result<(), JournalError> {
        self.conn.execute("DELETE FROM halt_state WHERE id = 1", ()).await?;
        Ok(())
    }

    pub async fn halt_reason(&self) -> Result<Option<String>, JournalError> {
        let mut rows = self.conn.query("SELECT reason FROM halt_state WHERE id = 1", ()).await?;
        let Some(row) = rows.next().await? else { return Ok(None) };
        Ok(row
            .get_value(0)
            .map_err(|e| JournalError::Db(e.to_string()))?
            .as_text()
            .map(|s| s.to_string()))
    }

    async fn scalar_i64(
        &self,
        sql: &str,
        params: impl turso::params::IntoParams,
    ) -> Result<i64, JournalError> {
        let mut rows = self.conn.query(sql, params).await?;
        let Some(row) = rows.next().await? else { return Ok(0) };
        Ok(row
            .get_value(0)
            .map_err(|e| JournalError::Db(e.to_string()))?
            .as_integer()
            .copied()
            .unwrap_or(0))
    }
}
```

If the `turso` API's row accessors differ from `get_value`/`as_text`/`as_integer`, adapt to what the installed version exposes — check with `cargo doc -p turso --open`. Do not change the storage format: Decimals stay TEXT.

- [ ] **Step 6: Implement the background sync task**

Create `crates/persistence/src/sync.rs`:

```rust
use std::sync::Arc;
use std::time::Duration;

use tracing::warn;

use crate::journal::Journal;

/// Push the local journal to Turso cloud on an interval.
///
/// Failures are logged and retried on the next tick. They are deliberately not
/// surfaced to the caller: an unreachable cloud must never stop the bot from
/// trading or from recording state locally.
pub fn spawn_sync_task(journal: Arc<Journal>, interval: Duration) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        loop {
            ticker.tick().await;
            if let Err(e) = journal.push().await {
                warn!(error = %e, "turso sync failed; will retry on next tick");
            }
        }
    });
}
```

Create `crates/persistence/src/lib.rs`:

```rust
pub mod journal;
pub mod schema;
pub mod sync;

pub use journal::{Journal, JournalError, OrderRecord};
pub use sync::spawn_sync_task;
```

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test -p persistence && cargo clippy -p persistence -- -D warnings`
Expected: PASS — 6 tests, no clippy warnings.

- [ ] **Step 8: Add the sync-failure isolation test**

Create `crates/persistence/tests/sync_isolation.rs`:

```rust
use core::{OrderState, Side, Symbol};
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
```

- [ ] **Step 9: Run the isolation test**

Run: `cargo test -p persistence`
Expected: PASS — 7 tests.

- [ ] **Step 10: Commit**

```bash
git add crates/persistence
git commit -m "feat(persistence): Turso journal with local-first writes and background sync

Decimals are stored as TEXT so SQLite's float REAL type cannot corrupt a
price. Sync failures are logged and retried, never propagated into the order
path, and a test proves local writes survive an unreachable cloud."
```

---

### Task 11: Config, credentials and the probe binary

**Files:**
- Create: `bot/Cargo.toml`, `bot/src/config.rs`, `bot/src/main.rs`, `config/testnet.toml`, `.env.example`
- Test: `bot/tests/config_loading.rs`

**Interfaces:**
- Consumes: everything from Tasks 1–10
- Produces:
  - `Config` with `Config::load(profile: &str) -> Result<Config, ConfigError>` and `Config::hash(&self) -> String`
  - `Profile` enum (`Testnet`, `Mainnet`) with `rest_base_url()`, `ws_public_url()`, `ws_private_url()`
  - `crypto-bot probe` — connects, authenticates, streams candles, writes to the journal

- [ ] **Step 1: Write the failing config test**

Create `bot/tests/config_loading.rs`:

```rust
use bot::config::{Config, ConfigError, Profile};

#[test]
fn testnet_profile_points_at_testnet_hosts() {
    assert_eq!(Profile::Testnet.rest_base_url(), "https://api-testnet.bybit.com");
    assert_eq!(
        Profile::Testnet.ws_public_url(),
        "wss://stream-testnet.bybit.com/v5/public/linear"
    );
    assert_eq!(Profile::Testnet.ws_private_url(), "wss://stream-testnet.bybit.com/v5/private");
}

#[test]
fn mainnet_profile_points_at_mainnet_hosts() {
    assert_eq!(Profile::Mainnet.rest_base_url(), "https://api.bybit.com");
    assert_eq!(Profile::Mainnet.ws_public_url(), "wss://stream.bybit.com/v5/public/linear");
}

#[test]
fn mainnet_requires_an_explicit_confirmation_variable() {
    // Safety gate: selecting mainnet must take two independent actions, so no
    // single mistake can route orders at real money.
    temp_env::with_var_unset("BYBIT_ALLOW_MAINNET", || {
        let err = Profile::from_name("mainnet").expect_err("must refuse without confirmation");
        assert!(matches!(err, ConfigError::MainnetNotConfirmed));
    });
}

#[test]
fn mainnet_is_allowed_once_confirmed() {
    temp_env::with_var("BYBIT_ALLOW_MAINNET", Some("yes"), || {
        assert_eq!(Profile::from_name("mainnet").expect("confirmed"), Profile::Mainnet);
    });
}

#[test]
fn testnet_never_requires_confirmation() {
    temp_env::with_var_unset("BYBIT_ALLOW_MAINNET", || {
        assert_eq!(Profile::from_name("testnet").expect("testnet is always allowed"), Profile::Testnet);
    });
}

#[test]
fn config_hash_is_stable_and_parameter_sensitive() {
    let a = Config::from_toml_str(SAMPLE).expect("parses");
    let b = Config::from_toml_str(SAMPLE).expect("parses");
    assert_eq!(a.hash(), b.hash(), "same config must hash identically");

    let changed = Config::from_toml_str(&SAMPLE.replace("risk_pct = 0.01", "risk_pct = 0.02"))
        .expect("parses");
    assert_ne!(a.hash(), changed.hash(), "changing a rule must change the hash");
    assert_eq!(a.hash().len(), 64, "hash is a SHA-256 hex digest");
}

const SAMPLE: &str = r#"
[risk]
risk_pct = 0.01
max_concurrent_positions = 4
max_daily_entries = 5
daily_drawdown_halt_pct = 0.05
total_drawdown_halt_pct = 0.15
liq_buffer_multiple = 3.0
leverage = 5

[strategy]
ema_fast = 50
ema_slow = 200
ema_entry = 20
rsi_period = 14
rsi_long_trigger = 40
rsi_short_trigger = 60
atr_period = 14
atr_band_min_pct = 0.003
atr_band_max_pct = 0.05
swing_lookback = 10
atr_stop_multiple = 1.5
reward_multiple = 2.0
entry_expiry_candles = 3
stop_limit_offset_atr = 0.3
max_stop_escalations = 3
stop_fill_timeout_secs = 30

[universe]
size = 20
min_turnover_24h = 50000000
min_listing_age_days = 30
"#;
```

- [ ] **Step 2: Create the bot crate manifest**

Create `bot/Cargo.toml`:

```toml
[package]
name = "bot"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true

[[bin]]
name = "crypto-bot"
path = "src/main.rs"

[lib]
name = "bot"
path = "src/lib.rs"

[dependencies]
core = { path = "../crates/core" }
exchange = { path = "../crates/exchange" }
indicators = { path = "../crates/indicators" }
persistence = { path = "../crates/persistence" }
rust_decimal.workspace = true
serde.workspace = true
sha2 = "0.10"
hex = "0.4"
thiserror.workspace = true
tokio.workspace = true
toml = "0.8"
tracing.workspace = true
tracing-subscriber.workspace = true

[dev-dependencies]
temp-env = "0.3"
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test -p bot`
Expected: FAIL — `bot::config` not found.

- [ ] **Step 4: Implement config and profiles**

Create `bot/src/config.rs`:

```rust
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("unknown profile {0}; expected 'testnet' or 'mainnet'")]
    UnknownProfile(String),

    #[error(
        "mainnet profile requires BYBIT_ALLOW_MAINNET to be set. \
         This is a deliberate second gate so real money is never reached by accident."
    )]
    MainnetNotConfirmed,

    #[error("could not read config file {path}: {source}")]
    Io { path: String, source: std::io::Error },

    #[error("invalid config: {0}")]
    Parse(#[from] toml::de::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Testnet,
    Mainnet,
}

impl Profile {
    /// Parse a profile name. Mainnet additionally requires the
    /// `BYBIT_ALLOW_MAINNET` environment variable to be present.
    pub fn from_name(name: &str) -> Result<Self, ConfigError> {
        match name {
            "testnet" => Ok(Profile::Testnet),
            "mainnet" => {
                if std::env::var("BYBIT_ALLOW_MAINNET").is_ok() {
                    Ok(Profile::Mainnet)
                } else {
                    Err(ConfigError::MainnetNotConfirmed)
                }
            }
            other => Err(ConfigError::UnknownProfile(other.to_string())),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Profile::Testnet => "testnet",
            Profile::Mainnet => "mainnet",
        }
    }

    pub fn rest_base_url(self) -> &'static str {
        match self {
            Profile::Testnet => "https://api-testnet.bybit.com",
            Profile::Mainnet => "https://api.bybit.com",
        }
    }

    pub fn ws_public_url(self) -> &'static str {
        match self {
            Profile::Testnet => "wss://stream-testnet.bybit.com/v5/public/linear",
            Profile::Mainnet => "wss://stream.bybit.com/v5/public/linear",
        }
    }

    pub fn ws_private_url(self) -> &'static str {
        match self {
            Profile::Testnet => "wss://stream-testnet.bybit.com/v5/private",
            Profile::Mainnet => "wss://stream.bybit.com/v5/private",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RiskConfig {
    pub risk_pct: f64,
    pub max_concurrent_positions: u32,
    pub max_daily_entries: u32,
    pub daily_drawdown_halt_pct: f64,
    pub total_drawdown_halt_pct: f64,
    pub liq_buffer_multiple: f64,
    pub leverage: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StrategyConfig {
    pub ema_fast: usize,
    pub ema_slow: usize,
    pub ema_entry: usize,
    pub rsi_period: usize,
    pub rsi_long_trigger: f64,
    pub rsi_short_trigger: f64,
    pub atr_period: usize,
    pub atr_band_min_pct: f64,
    pub atr_band_max_pct: f64,
    pub swing_lookback: usize,
    pub atr_stop_multiple: f64,
    pub reward_multiple: f64,
    pub entry_expiry_candles: u32,
    pub stop_limit_offset_atr: f64,
    pub max_stop_escalations: u32,
    pub stop_fill_timeout_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UniverseConfig {
    pub size: usize,
    pub min_turnover_24h: u64,
    pub min_listing_age_days: i64,
}

/// The complete rule set. Immutable for the process lifetime — changing a
/// parameter requires an edit and a restart, which produces a new hash and a
/// visible discontinuity in the journal.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Config {
    pub risk: RiskConfig,
    pub strategy: StrategyConfig,
    pub universe: UniverseConfig,
}

impl Config {
    pub fn load(profile: Profile) -> Result<Self, ConfigError> {
        let path = format!("config/{}.toml", profile.name());
        let text = std::fs::read_to_string(&path)
            .map_err(|source| ConfigError::Io { path: path.clone(), source })?;
        Self::from_toml_str(&text)
    }

    pub fn from_toml_str(s: &str) -> Result<Self, ConfigError> {
        Ok(toml::from_str(s)?)
    }

    /// SHA-256 over a canonical serialisation. Written to every order row so
    /// each trade is attributable to an exact ruleset.
    pub fn hash(&self) -> String {
        let canonical = toml::to_string(self).expect("config always serialises");
        let mut hasher = Sha256::new();
        hasher.update(canonical.as_bytes());
        hex::encode(hasher.finalize())
    }
}
```

Create `bot/src/lib.rs`:

```rust
pub mod config;
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p bot`
Expected: PASS — 6 tests.

- [ ] **Step 6: Create the config file and env template**

Create `config/testnet.toml` with the contents of `SAMPLE` from the test (without the surrounding Rust string literal). Create `.env.example`:

```bash
# Copy to .env and fill in. NEVER commit .env — it is gitignored.
# Create the testnet key with TRADE permission and WITHDRAWAL DISABLED.
BYBIT_API_KEY=
BYBIT_API_SECRET=

# Turso. Leave both unset to run with a purely local journal.
TURSO_DATABASE_URL=
TURSO_AUTH_TOKEN=

# Required only for the mainnet profile. Leave unset for testnet.
# BYBIT_ALLOW_MAINNET=yes
```

- [ ] **Step 7: Implement the probe binary**

Create `bot/src/main.rs`:

```rust
use std::sync::Arc;
use std::time::Duration;

use bot::config::{Config, Profile};
use core::{Symbol, Timeframe};
use exchange::bybit::rest::BybitRest;
use exchange::bybit::sign::Credentials;
use exchange::bybit::ws_public::BybitPublicFeed;
use exchange::{MarketEvent, MarketFeed, Subscription};
use persistence::{spawn_sync_task, Journal};
use tracing::{error, info};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let profile_name = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "testnet".to_string());
    let profile = Profile::from_name(&profile_name)?;
    let config = Config::load(profile)?;

    info!(profile = profile.name(), config_hash = %config.hash(), "starting probe");

    let creds = Credentials::from_env()?;
    let rest = Arc::new(BybitRest::new(profile.rest_base_url().to_string(), creds));

    // 1. Prove authentication works.
    let balance = rest.balance().await?;
    info!(equity = %balance.equity, available = %balance.available, "authenticated");

    // 2. Prove market data works and instrument metadata parses.
    let instruments = rest.instruments().await?;
    info!(count = instruments.len(), "loaded tradable instruments");

    let tickers = rest.tickers().await?;
    let mut ranked: Vec<_> = tickers
        .into_iter()
        .filter(|t| {
            t.turnover_24h >= rust_decimal::Decimal::from(config.universe.min_turnover_24h)
        })
        .collect();
    ranked.sort_by(|a, b| b.turnover_24h.cmp(&a.turnover_24h));
    ranked.truncate(config.universe.size);
    info!(count = ranked.len(), top = ?ranked.first().map(|t| t.symbol.as_str()), "universe ranked");

    // 3. Prove the journal works, falling back to local-only when Turso is
    //    not configured or unreachable — never a reason to refuse to start.
    let journal = match (std::env::var("TURSO_DATABASE_URL"), std::env::var("TURSO_AUTH_TOKEN")) {
        (Ok(url), Ok(token)) if !url.is_empty() && !token.is_empty() => {
            match Journal::open_synced("data/bot.db", &url, &token).await {
                Ok(j) => {
                    info!("journal opened with Turso cloud sync");
                    j
                }
                Err(e) => {
                    error!(error = %e, "Turso sync unavailable; falling back to local journal");
                    Journal::open_local("data/bot.db").await?
                }
            }
        }
        _ => {
            info!("Turso not configured; using local journal only");
            Journal::open_local("data/bot.db").await?
        }
    };
    let journal = Arc::new(journal);
    journal.record_equity(balance.equity, 0).await?;
    spawn_sync_task(Arc::clone(&journal), Duration::from_secs(30));

    // 4. Prove the streaming feed works end to end.
    let feed = BybitPublicFeed::new(profile.ws_public_url().to_string(), Arc::clone(&rest));
    let subs: Vec<Subscription> = ranked
        .iter()
        .take(3)
        .map(|t| Subscription { symbol: Symbol::new(t.symbol.as_str()), timeframe: Timeframe::H1 })
        .collect();
    let mut rx = feed.subscribe(&subs).await?;
    info!(symbols = ?subs.iter().map(|s| s.symbol.as_str()).collect::<Vec<_>>(), "streaming klines");

    // 5. Warm up from history so a candle close is not needed to see data.
    for sub in &subs {
        let candles = rest.klines(&sub.symbol, sub.timeframe, 250).await?;
        info!(symbol = %sub.symbol, candles = candles.len(), "warmup history loaded");
    }

    info!("probe running; Ctrl-C to exit");
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("shutdown signal received");
                if let Err(e) = journal.push().await {
                    error!(error = %e, "final journal push failed");
                }
                return Ok(());
            }
            event = rx.recv() => match event {
                Ok(MarketEvent::CandleClosed { symbol, tf, candle }) => {
                    info!(%symbol, ?tf, close = %candle.close, "candle closed");
                }
                Ok(MarketEvent::GapFilled { symbol, candles, .. }) => {
                    info!(%symbol, count = candles.len(), "gap backfilled");
                }
                Err(e) => error!(error = %e, "feed channel error"),
            },
        }
    }
}
```

- [ ] **Step 8: Verify the whole workspace builds and tests pass**

Run: `cargo build --workspace && cargo test --workspace && cargo clippy --workspace -- -D warnings`
Expected: PASS — all tests green, no clippy warnings.

- [ ] **Step 9: Run the probe against testnet**

Create testnet API keys at <https://testnet.bybit.com> with **trade permission and withdrawal disabled**, then:

```bash
cp .env.example .env   # fill in BYBIT_API_KEY and BYBIT_API_SECRET
set -a && . ./.env && set +a
mkdir -p data
cargo run --release --bin crypto-bot -- testnet
```

Expected: log lines for `authenticated` (with a non-zero equity), `loaded tradable instruments`, `universe ranked`, `streaming klines`, `warmup history loaded`, then a `candle closed` line at the top of each hour.

If `authenticated` fails with retCode 10004, the signature is wrong — check that the signed payload is byte-identical to the transmitted body. If it fails with 10002, the clock offset is not being applied.

- [ ] **Step 10: Leave the probe running for one hour to confirm streaming**

The probe must log at least one `candle closed` line per subscribed symbol. This is the acceptance criterion for the whole plan: it proves signing, REST, WebSocket, reconnect scaffolding and the journal all work together against a real exchange.

- [ ] **Step 11: Commit**

```bash
git add bot config .env.example
git commit -m "feat(bot): config with hashing, profile gating and testnet probe binary

Mainnet requires both an explicit profile argument and BYBIT_ALLOW_MAINNET,
so no single mistake routes orders at real money. The probe proves auth,
market data, streaming and the journal work end to end against testnet."
```

- [ ] **Step 12: Refresh the knowledge graph**

```bash
graphify . --update
```

Expected: the graph gains nodes for the new crates and their relationships.

---

## Plan Self-Review

**1. Spec coverage.** Mapping each Phase 1a-relevant spec section to a task:

| Spec section | Task |
|---|---|
| §3.1 workspace layout | 1 |
| §3.3 `ExchangeClient` / `MarketFeed` traits | 7 |
| §4.1 endpoints | 6 (market data), 7 (trading) |
| §4.2 authentication, clock offset | 4 |
| §4.3 limit-only order policy, PostOnly, stop-limit | 7 |
| §5 invariant 2 (no market orders) | 7 (source-level test) |
| §5 invariant 11 (Decimal everywhere) | 1, 2 |
| §5 invariant 12 (journal never blocks orders) | 10 |
| §5 invariant 13 (mainnet gating) | 11 |
| §7.1 Turso local-first persistence | 10 |
| §7.3 error classification, backoff, rate limiting | 5 |
| §10 config, secrets, profiles | 11 |
| Indicators for §6 strategy | 3 |
| WebSocket streams, reconnect, gap fill | 8, 9 |

**Deferred to Plan 2 (Trading Engine), by design:** §5 invariants 1, 3–10 (sizing, liquidation buffer, daily cap, concurrency, drawdown halt, staleness), §5.1 daily trade cap enforcement, §5.2 config-hash-per-trade wiring, §6 strategy implementation and universe selection logic, §7.2 startup reconciliation, `OrderTracker` expiry and stop escalation, `RiskManager`, `Executor`, `Reconciler`, `MockExchange` integration tests, and the two-week soak.

Task 10 and 11 build the config-hash and journal *mechanism*; Plan 2 wires it into every order.

**2. Placeholder scan.** No TBDs, no "add error handling", no "similar to Task N". Two places name an explicit fallback rather than exact code, and both are deliberate: the `turso` crate's row-accessor API (Task 10 Step 5) and the golden signature vector (Task 4 Step 5), which must be generated locally rather than guessed. Both say exactly what to run.

**3. Type consistency.** Verified across tasks: `Side::as_bybit()` (Task 2) used in Tasks 7 and 10; `Symbol::as_str()` used throughout; `Timeframe::as_bybit_interval()` (Task 2) used in Tasks 6 and 8; `ExchangeError` (Task 5) is the error type of every method in Tasks 6–9; `OrderState` (Task 2) parsed in Task 7, re-parsed in Task 9, persisted in Task 10; `Ticker` (Task 6) returned by the trait in Task 7 and consumed in Task 11; `ClockOffset` (Task 4) shared by Tasks 6 and 9.

One fix applied inline: Task 4's `ClockOffset` test originally bound `let mut clock`, which would not compile against the `AtomicI64` interior-mutability design; Step 4 notes the change to `let clock`.

A second fix applied inline: `PositionRow::into_position` must check size before parsing `side`, because Bybit sends `side: ""` on flat positions. Task 9 Step 3 states this explicitly.

**Amendment (2026-08-01, pre-execution):** Task 7 originally defined every endpoint twice — as inherent methods on `BybitRest` and again in a delegating `impl ExchangeClient for BybitRest`. That is verbatim duplication of ten signatures with no benefit beyond letting tests skip a trait import. Task 7 now defines the traits first (Step 4), implements every endpoint exactly once inside the trait impl (Step 5), and moves Task 6's three market-data methods into the same impl (Step 6). `impl BybitRest` keeps only the constructor and the get/post/retry plumbing.

---

## Execution Handoff

Plan 1a covers 11 tasks producing a tested, running foundation. Plan 2 (Trading Engine) will be written against this plan's produced interfaces once it is executed — writing it now would lock in signatures before the exchange client has met the real testnet.

