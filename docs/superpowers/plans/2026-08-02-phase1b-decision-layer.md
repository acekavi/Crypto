# Phase 1b — Decision Layer Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the two crates that decide *whether* to trade and *how much* — a pluggable `Strategy` producing unsized signals, and a `RiskManager` that turns a signal into a fully-sized order intent or a documented refusal.

**Architecture:** Two new crates with no I/O and no async. `strategy` holds per-symbol incremental indicator state and emits a `Signal` carrying direction, entry limit, stop and target — but never a quantity. `risk` owns every hard limit the owner set and is the only place a position size is computed. Keeping both pure makes them exhaustively testable without a network, and lets Phase 2's backtester replay them unchanged.

**Tech Stack:** Rust 1.97 (edition 2024), rust_decimal, botcore, indicators, proptest. No tokio, no reqwest — these crates do no I/O.

## Global Constraints

Copied from the approved spec. Every task's requirements implicitly include this section.

- All monetary and quantity values use `rust_decimal::Decimal`. **Never `f64`** for prices, quantities, or balances. Config tuning knobs (percentages, multiples) arrive as `f64` and must be converted to `Decimal` at the boundary.
- The domain crate is **`botcore`**, never `core` — that name shadows Rust's sysroot crate and breaks `thiserror` derives.
- **No code path may construct `orderType: "Market"`.** A source-grep test in `crates/exchange/tests/no_market_orders.rs` scans every `.rs` file under `crates/` and `bot/` for the quoted literal `"Market"` and fails the build. `"MarkPrice"` is a different string and is fine.
- **Risk per trade is 1% of equity.** Size = `(risk_pct × equity) ÷ stop_distance`, rounded **down** to the instrument's `qty_step`. Rounding may only reduce risk, never increase it.
- **Risk:reward is 1:2.** Target sits at 2R, where R = `|entry_limit_price − stop_price|`.
- **Max 4 concurrent positions, max 1 position per symbol.**
- **Max 5 filled entries per UTC day**, counted at fill, not at placement.
- **Liquidation must sit at least 3.0 × stop distance from entry**, else the trade is rejected rather than resized.
- Halt on −5% daily drawdown (measured from equity at the most recent 00:00 UTC) or −15% total drawdown (from the all-time high-water mark).
- The strategy must never see account equity. Only `RiskManager` computes size.
- Rust edition 2024, resolver 3, rust-version 1.97.

## What already exists (Plan 1a)

- **`botcore`** — `Candle { open_time_ms, open, high, low, close, volume, turnover }`, `Timeframe::{H1,H4}` (`as_bybit_interval()`, `duration_ms()`), `Symbol` (`new()`, `as_str()`), `Instrument { symbol, tick_size, qty_step, min_order_qty, launch_time_ms }` (`age_days()`, `qty_is_valid()`), `Side::{Buy,Sell}` (`as_bybit()`, `opposite()`), `Position { symbol, side, size, entry_price, liq_price: Option<Decimal>, unrealized_pnl }`, `Balance { equity, available }`, `LimitEntry { symbol, side, qty, price, order_link_id, stop_loss, stop_limit_price, take_profit }`, `OrderState`, `ErrorClass`, and `money::{round_down_to_step, round_price_away_from_market}`.
- **`indicators`** — `Ema::new(period)`, `Rsi::new(period)`, `Atr::new(period)`, each with `update(...) -> Option<Decimal>`, `value() -> Option<Decimal>`, `is_warm() -> bool`. `Ema`/`Rsi` take a `Decimal` price; `Atr` takes `&Candle`.
- **`bot::config`** — `StrategyConfig`, `RiskConfig`, `UniverseConfig`, `Config::hash()`.

`botcore::money::round_down_to_step` carries a `debug_assert!` rejecting negative input — always pass a non-negative quantity.

---

## File Structure

```
crates/
├── strategy/
│   ├── Cargo.toml
│   └── src/
│       ├── lib.rs          re-exports
│       ├── signal.rs       Signal, MarketContext, StrategyParams
│       ├── traits.rs       the Strategy trait
│       └── pullback.rs     baseline trend-filtered pullback + per-symbol state
└── risk/
    ├── Cargo.toml
    └── src/
        ├── lib.rs          re-exports
        ├── sizing.rs       size from stop distance; liquidation-buffer check
        ├── limits.rs       concurrency, daily cap, drawdown halt
        └── manager.rs      RiskManager: Signal + AccountState -> Decision
```

Splitting `risk` three ways keeps each file focused on one question: *how big*, *are we allowed*, and *put it together*. The limits are the owner's hard rules and deserve a file a reviewer can read in one sitting.

---

### Task 1: Signal types and the Strategy trait

**Files:**
- Create: `crates/strategy/Cargo.toml`, `crates/strategy/src/lib.rs`, `crates/strategy/src/signal.rs`, `crates/strategy/src/traits.rs`
- Modify: root `Cargo.toml` (add `"crates/strategy"` to members)

**Interfaces:**
- Consumes: `botcore::{Candle, Instrument, Side, Symbol, Timeframe}`
- Produces:
  - `Signal { symbol: Symbol, side: Side, entry_price: Decimal, stop_price: Decimal, target_price: Decimal, atr: Decimal, signal_candle_open_ms: i64 }` with `Signal::risk_distance(&self) -> Decimal`
  - `MarketContext<'a> { symbol: &'a Symbol, timeframe: Timeframe, candle: &'a Candle, instrument: &'a Instrument }`
  - `trait Strategy` with `timeframes()`, `warmup_candles()`, `on_candle_close(&mut self, ctx: &MarketContext) -> Option<Signal>`

- [ ] **Step 1: Create the crate manifest**

Create `crates/strategy/Cargo.toml`:

```toml
[package]
name = "strategy"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true

[dependencies]
botcore = { path = "../botcore" }
indicators = { path = "../indicators" }
rust_decimal.workspace = true

[dev-dependencies]
rust_decimal_macros.workspace = true
```

Add `"crates/strategy"` to the `members` array in the root `Cargo.toml`. Do not add `crates/risk` yet — it does not exist until Task 4.

- [ ] **Step 2: Write the failing test for Signal**

Create `crates/strategy/src/signal.rs` containing only the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use botcore::Symbol;
    use rust_decimal_macros::dec;

    #[test]
    fn risk_distance_is_absolute_for_a_long() {
        let s = Signal {
            symbol: Symbol::new("BTCUSDT"),
            side: Side::Buy,
            entry_price: dec!(100),
            stop_price: dec!(95),
            target_price: dec!(110),
            atr: dec!(2),
            signal_candle_open_ms: 0,
        };
        assert_eq!(s.risk_distance(), dec!(5));
    }

    #[test]
    fn risk_distance_is_absolute_for_a_short() {
        // A short's stop sits ABOVE entry, so a naive entry - stop would be
        // negative and every downstream size calculation would invert.
        let s = Signal {
            symbol: Symbol::new("BTCUSDT"),
            side: Side::Sell,
            entry_price: dec!(100),
            stop_price: dec!(105),
            target_price: dec!(90),
            atr: dec!(2),
            signal_candle_open_ms: 0,
        };
        assert_eq!(s.risk_distance(), dec!(5));
    }

    #[test]
    fn reward_multiple_is_two_for_a_correctly_built_long() {
        let s = Signal {
            symbol: Symbol::new("BTCUSDT"),
            side: Side::Buy,
            entry_price: dec!(100),
            stop_price: dec!(95),
            target_price: dec!(110),
            atr: dec!(2),
            signal_candle_open_ms: 0,
        };
        assert_eq!(s.reward_multiple(), Some(dec!(2)));
    }

    #[test]
    fn reward_multiple_is_two_for_a_correctly_built_short() {
        let s = Signal {
            symbol: Symbol::new("BTCUSDT"),
            side: Side::Sell,
            entry_price: dec!(100),
            stop_price: dec!(105),
            target_price: dec!(90),
            atr: dec!(2),
            signal_candle_open_ms: 0,
        };
        assert_eq!(s.reward_multiple(), Some(dec!(2)));
    }

    #[test]
    fn reward_multiple_is_none_when_stop_equals_entry() {
        // A zero-distance stop would divide by zero downstream; callers must
        // be able to detect it rather than panic.
        let s = Signal {
            symbol: Symbol::new("BTCUSDT"),
            side: Side::Buy,
            entry_price: dec!(100),
            stop_price: dec!(100),
            target_price: dec!(110),
            atr: dec!(2),
            signal_candle_open_ms: 0,
        };
        assert_eq!(s.reward_multiple(), None);
    }
}
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test -p strategy`
Expected: FAIL — compile error, `Signal` not found.

- [ ] **Step 4: Implement the signal types**

Prepend to `crates/strategy/src/signal.rs`:

```rust
use botcore::{Candle, Instrument, Side, Symbol, Timeframe};
use rust_decimal::Decimal;

/// A strategy's decision to enter, expressed in prices only.
///
/// There is deliberately no quantity field. The strategy never sees account
/// equity; `RiskManager` is the only component that computes size. That
/// separation keeps sizing bugs out of strategy code and lets any strategy be
/// replayed against any equity curve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signal {
    pub symbol: Symbol,
    pub side: Side,
    /// The limit price the entry will rest at.
    pub entry_price: Decimal,
    pub stop_price: Decimal,
    pub target_price: Decimal,
    /// ATR at the signal candle's close. Carried because execution derives the
    /// stop-limit offset and the escalation ladder's widths from it — those are
    /// fractions of ATR, and recomputing ATR outside the strategy would mean a
    /// second implementation free to drift from this one.
    pub atr: Decimal,
    /// Open time of the candle that produced this signal. Feeds the
    /// deterministic `orderLinkId`, so a retry cannot open a second position.
    pub signal_candle_open_ms: i64,
}

impl Signal {
    /// "R" — the distance between the entry limit and the stop.
    ///
    /// Always non-negative: a short's stop sits above its entry, so a raw
    /// subtraction would invert every downstream size calculation.
    pub fn risk_distance(&self) -> Decimal {
        (self.entry_price - self.stop_price).abs()
    }

    /// How many R the target sits away from entry. `None` when the stop
    /// distance is zero, which callers must treat as an invalid signal
    /// rather than dividing by it.
    pub fn reward_multiple(&self) -> Option<Decimal> {
        let r = self.risk_distance();
        if r.is_zero() {
            return None;
        }
        Some((self.target_price - self.entry_price).abs() / r)
    }
}

/// Everything a strategy sees about one closed candle.
///
/// Deliberately one candle, not a slice: strategies hold incremental
/// indicator state, so evaluation is O(1) per candle. Passing history would
/// make a backtest over years of data quadratic.
#[derive(Debug)]
pub struct MarketContext<'a> {
    pub symbol: &'a Symbol,
    pub timeframe: Timeframe,
    pub candle: &'a Candle,
    pub instrument: &'a Instrument,
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p strategy`
Expected: PASS — 5 tests.

- [ ] **Step 6: Define the Strategy trait**

Create `crates/strategy/src/traits.rs`:

```rust
use botcore::Timeframe;

use crate::signal::{MarketContext, Signal};

/// A pluggable trading strategy.
///
/// Implementations hold their own per-symbol indicator state and are fed one
/// closed candle at a time. The engine calls `on_candle_close` only for
/// timeframes the strategy declared, and only for candles Bybit marked
/// confirmed.
pub trait Strategy: Send {
    /// Which timeframes this strategy needs fed to it.
    fn timeframes(&self) -> &[Timeframe];

    /// How many candles of history are needed before signals are trustworthy.
    /// The engine warms indicators with this much history after a restart
    /// before accepting any signal.
    fn warmup_candles(&self) -> usize;

    /// Consume one closed candle. Returns a signal only on the candle that
    /// completes a setup; `None` on every other candle, including those that
    /// merely advance indicator state.
    fn on_candle_close(&mut self, ctx: &MarketContext) -> Option<Signal>;
}
```

Create `crates/strategy/src/lib.rs`:

```rust
pub mod pullback;
pub mod signal;
pub mod traits;

pub use pullback::PullbackStrategy;
pub use signal::{MarketContext, Signal};
pub use traits::Strategy;
```

Create a placeholder `crates/strategy/src/pullback.rs` containing only `// Implemented in Task 2.` so the crate compiles. Task 2 replaces it.

- [ ] **Step 7: Verify the crate builds and tests pass**

Run: `cargo test -p strategy && cargo clippy -p strategy -- -D warnings`
Expected: PASS — 5 tests, no clippy warnings.

- [ ] **Step 8: Commit**

```bash
git add crates/strategy Cargo.toml Cargo.lock
git commit -m "feat(strategy): Signal types and the Strategy trait

Signal carries prices but never a quantity — the strategy never sees account
equity, so sizing bugs cannot originate in strategy code. risk_distance is
absolute so a short's above-entry stop cannot invert downstream sizing."
```

---

### Task 2: Baseline trend-filtered pullback strategy

**Files:**
- Create: `crates/strategy/src/pullback.rs` (replacing the Task 1 placeholder)
- Test: `crates/strategy/tests/pullback_setups.rs`

**Interfaces:**
- Consumes: `Signal`, `MarketContext`, `Strategy` (Task 1); `indicators::{Ema, Rsi, Atr}`
- Produces:
  - `StrategyParams { ema_fast: usize, ema_slow: usize, ema_entry: usize, rsi_period: usize, rsi_long_trigger: Decimal, rsi_short_trigger: Decimal, atr_period: usize, atr_band_min_pct: Decimal, atr_band_max_pct: Decimal, swing_lookback: usize, atr_stop_multiple: Decimal, reward_multiple: Decimal, pullback_lookback: usize, pullback_atr_fraction: Decimal }` with `StrategyParams::defaults()`
  - `PullbackStrategy::new(params: StrategyParams) -> Self`

The rules, verbatim from the approved spec §6:

| Element | Rule |
|---|---|
| Bias filter (4h) | Long bias if EMA50 > EMA200; short bias if EMA50 < EMA200; otherwise no trade |
| Pullback (1h) | Within the last 5 closed candles, the low (long) or high (short) came within 0.5 × ATR(14) of EMA20 |
| Trigger (1h) | On the closing candle, RSI(14) crosses from below 40 to at or above 40 (long), or from above 60 to at or below 60 (short) |
| Volatility gate | ATR(14) ÷ close must fall in [0.3%, 5.0%] |
| Entry | Limit at the EMA20 value on the signal candle's close, in the 4h bias direction only |
| Stop | The further from entry of: lowest low (long) / highest high (short) of the last 10 closed candles, or 1.5 × ATR(14) |
| Target | 2R |

- [ ] **Step 1: Write the failing test for parameter defaults and bias**

Create `crates/strategy/tests/pullback_setups.rs`:

```rust
use botcore::{Candle, Instrument, Side, Symbol, Timeframe};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use strategy::pullback::{PullbackStrategy, StrategyParams};
use strategy::{MarketContext, Strategy};

fn instrument() -> Instrument {
    Instrument {
        symbol: Symbol::new("BTCUSDT"),
        tick_size: dec!(0.1),
        qty_step: dec!(0.001),
        min_order_qty: dec!(0.001),
        launch_time_ms: 0,
    }
}

fn candle(open_time_ms: i64, high: Decimal, low: Decimal, close: Decimal) -> Candle {
    Candle {
        open_time_ms,
        open: close,
        high,
        low,
        close,
        volume: Decimal::ZERO,
        turnover: Decimal::ZERO,
    }
}

/// Feed a flat-then-rising 4h series so EMA50 climbs above EMA200,
/// establishing long bias. Returns the strategy already warm on 4h.
fn warm_long_bias(strat: &mut PullbackStrategy, symbol: &Symbol, inst: &Instrument) {
    // 250 rising 4h candles is enough to warm EMA200 and put EMA50 above it.
    for i in 0..250i64 {
        let px = Decimal::from(1000 + i);
        let c = candle(i * 14_400_000, px + dec!(1), px - dec!(1), px);
        let ctx = MarketContext {
            symbol,
            timeframe: Timeframe::H4,
            candle: &c,
            instrument: inst,
        };
        strat.on_candle_close(&ctx);
    }
}

#[test]
fn defaults_match_the_spec() {
    let p = StrategyParams::defaults();
    assert_eq!(p.ema_fast, 50);
    assert_eq!(p.ema_slow, 200);
    assert_eq!(p.ema_entry, 20);
    assert_eq!(p.rsi_period, 14);
    assert_eq!(p.rsi_long_trigger, dec!(40));
    assert_eq!(p.rsi_short_trigger, dec!(60));
    assert_eq!(p.atr_period, 14);
    assert_eq!(p.atr_band_min_pct, dec!(0.003));
    assert_eq!(p.atr_band_max_pct, dec!(0.05));
    assert_eq!(p.swing_lookback, 10);
    assert_eq!(p.atr_stop_multiple, dec!(1.5));
    assert_eq!(p.reward_multiple, dec!(2));
    assert_eq!(p.pullback_lookback, 5);
    assert_eq!(p.pullback_atr_fraction, dec!(0.5));
}

#[test]
fn no_signal_before_indicators_are_warm() {
    let mut strat = PullbackStrategy::new(StrategyParams::defaults());
    let symbol = Symbol::new("BTCUSDT");
    let inst = instrument();

    // A single 1h candle cannot possibly complete a setup.
    let c = candle(0, dec!(101), dec!(99), dec!(100));
    let ctx = MarketContext {
        symbol: &symbol,
        timeframe: Timeframe::H1,
        candle: &c,
        instrument: &inst,
    };
    assert_eq!(strat.on_candle_close(&ctx), None);
}

#[test]
fn four_hour_candles_never_produce_a_signal_directly() {
    // The 4h stream only sets bias. Entries are timed on 1h.
    let mut strat = PullbackStrategy::new(StrategyParams::defaults());
    let symbol = Symbol::new("BTCUSDT");
    let inst = instrument();
    warm_long_bias(&mut strat, &symbol, &inst);

    let c = candle(250 * 14_400_000, dec!(1300), dec!(1240), dec!(1290));
    let ctx = MarketContext {
        symbol: &symbol,
        timeframe: Timeframe::H4,
        candle: &c,
        instrument: &inst,
    };
    assert_eq!(strat.on_candle_close(&ctx), None);
}

#[test]
fn declares_both_timeframes_and_a_warmup_covering_the_slow_ema() {
    let strat = PullbackStrategy::new(StrategyParams::defaults());
    let tfs = strat.timeframes();
    assert!(tfs.contains(&Timeframe::H1));
    assert!(tfs.contains(&Timeframe::H4));
    // EMA200 needs at least 200 samples before it reports a value at all.
    assert!(strat.warmup_candles() >= 200);
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p strategy --test pullback_setups`
Expected: FAIL — `PullbackStrategy` / `StrategyParams` not found.

- [ ] **Step 3: Implement parameters and per-symbol state**

Replace `crates/strategy/src/pullback.rs`:

```rust
use std::collections::HashMap;
use std::collections::VecDeque;

use botcore::{Candle, Side, Symbol, Timeframe};
use indicators::{Atr, Ema, Rsi};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

use crate::signal::{MarketContext, Signal};
use crate::traits::Strategy;

/// Tunable rules. Every threshold is a value here rather than a constant, so
/// Phase 2 can sweep them without touching code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StrategyParams {
    pub ema_fast: usize,
    pub ema_slow: usize,
    pub ema_entry: usize,
    pub rsi_period: usize,
    pub rsi_long_trigger: Decimal,
    pub rsi_short_trigger: Decimal,
    pub atr_period: usize,
    pub atr_band_min_pct: Decimal,
    pub atr_band_max_pct: Decimal,
    pub swing_lookback: usize,
    pub atr_stop_multiple: Decimal,
    pub reward_multiple: Decimal,
    pub pullback_lookback: usize,
    pub pullback_atr_fraction: Decimal,
}

impl StrategyParams {
    /// The spec's defaults. Conventional starting points, deliberately not
    /// tuned — tuning is Phase 2 work against out-of-sample data.
    pub fn defaults() -> Self {
        StrategyParams {
            ema_fast: 50,
            ema_slow: 200,
            ema_entry: 20,
            rsi_period: 14,
            rsi_long_trigger: dec!(40),
            rsi_short_trigger: dec!(60),
            atr_period: 14,
            atr_band_min_pct: dec!(0.003),
            atr_band_max_pct: dec!(0.05),
            swing_lookback: 10,
            atr_stop_multiple: dec!(1.5),
            reward_multiple: dec!(2),
            pullback_lookback: 5,
            pullback_atr_fraction: dec!(0.5),
        }
    }
}

/// Which direction the 4h trend permits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bias {
    Long,
    Short,
    None,
}

/// All incremental state for one symbol.
struct SymbolState {
    ema_fast_h4: Ema,
    ema_slow_h4: Ema,
    ema_entry_h1: Ema,
    rsi_h1: Rsi,
    atr_h1: Atr,
    /// Closed 1h candles, newest last, bounded to `swing_lookback`.
    recent_h1: VecDeque<Candle>,
    /// RSI on the previous 1h close, for detecting a cross rather than a level.
    prev_rsi: Option<Decimal>,
    /// EMA20 value at each of the last `pullback_lookback` 1h closes, so a
    /// pullback is measured against the EMA as it was then, not as it is now.
    recent_ema_entry: VecDeque<Decimal>,
}

impl SymbolState {
    fn new(p: &StrategyParams) -> Self {
        SymbolState {
            ema_fast_h4: Ema::new(p.ema_fast),
            ema_slow_h4: Ema::new(p.ema_slow),
            ema_entry_h1: Ema::new(p.ema_entry),
            rsi_h1: Rsi::new(p.rsi_period),
            atr_h1: Atr::new(p.atr_period),
            recent_h1: VecDeque::new(),
            prev_rsi: None,
            recent_ema_entry: VecDeque::new(),
        }
    }

    fn bias(&self) -> Bias {
        match (self.ema_fast_h4.value(), self.ema_slow_h4.value()) {
            (Some(fast), Some(slow)) if fast > slow => Bias::Long,
            (Some(fast), Some(slow)) if fast < slow => Bias::Short,
            _ => Bias::None,
        }
    }
}

/// Trend-filtered pullback with a limit entry at EMA20.
///
/// This is a starting point for exercising the engine, not a validated edge.
/// No claim is made about its profitability; Phase 2 decides whether it or
/// anything else reaches mainnet.
pub struct PullbackStrategy {
    params: StrategyParams,
    per_symbol: HashMap<Symbol, SymbolState>,
    timeframes: Vec<Timeframe>,
}

impl PullbackStrategy {
    pub fn new(params: StrategyParams) -> Self {
        PullbackStrategy {
            params,
            per_symbol: HashMap::new(),
            timeframes: vec![Timeframe::H1, Timeframe::H4],
        }
    }
}

impl Strategy for PullbackStrategy {
    fn timeframes(&self) -> &[Timeframe] {
        &self.timeframes
    }

    fn warmup_candles(&self) -> usize {
        // The slow EMA is the binding constraint; a margin on top keeps the
        // smoothed value meaningful rather than merely defined.
        self.params.ema_slow + 50
    }

    fn on_candle_close(&mut self, ctx: &MarketContext) -> Option<Signal> {
        let params = self.params.clone();
        let state = self
            .per_symbol
            .entry(ctx.symbol.clone())
            .or_insert_with(|| SymbolState::new(&params));

        match ctx.timeframe {
            // 4h only advances the bias filter. Entries are always timed on 1h.
            Timeframe::H4 => {
                state.ema_fast_h4.update(ctx.candle.close);
                state.ema_slow_h4.update(ctx.candle.close);
                None
            }
            Timeframe::H1 => evaluate_h1(&params, state, ctx),
        }
    }
}
```

- [ ] **Step 4: Implement the 1h evaluation**

Append to `crates/strategy/src/pullback.rs`:

```rust
/// Advance 1h state, then test the setup. Returns a signal only on the candle
/// that completes it.
fn evaluate_h1(
    params: &StrategyParams,
    state: &mut SymbolState,
    ctx: &MarketContext,
) -> Option<Signal> {
    let candle = ctx.candle;

    // Advance indicators first so every value below describes this candle.
    let ema20 = state.ema_entry_h1.update(candle.close);
    let rsi_now = state.rsi_h1.update(candle.close);
    let atr = state.atr_h1.update(candle);

    let prev_rsi = state.prev_rsi;
    state.prev_rsi = rsi_now;

    state.recent_h1.push_back(candle.clone());
    while state.recent_h1.len() > params.swing_lookback {
        state.recent_h1.pop_front();
    }

    if let Some(e) = ema20 {
        state.recent_ema_entry.push_back(e);
        while state.recent_ema_entry.len() > params.pullback_lookback {
            state.recent_ema_entry.pop_front();
        }
    }

    // Every indicator must be warm, and we need a previous RSI to detect a
    // cross rather than merely a level.
    let (ema20, rsi_now, atr, prev_rsi) = (ema20?, rsi_now?, atr?, prev_rsi?);

    let bias = state.bias();
    if bias == Bias::None {
        return None;
    }

    // Volatility gate: skip dead and berserk markets.
    if candle.close.is_zero() {
        return None;
    }
    let atr_pct = atr / candle.close;
    if atr_pct < params.atr_band_min_pct || atr_pct > params.atr_band_max_pct {
        return None;
    }

    // Need a full swing window before a stop can be located.
    if state.recent_h1.len() < params.swing_lookback {
        return None;
    }

    let side = match bias {
        Bias::Long => Side::Buy,
        Bias::Short => Side::Sell,
        Bias::None => unreachable!("bias None returned above"),
    };

    if !pullback_occurred(params, state, side, atr) {
        return None;
    }
    if !rsi_triggered(params, side, prev_rsi, rsi_now) {
        return None;
    }

    let stop_price = stop_for(params, state, side, ema20, atr);
    let risk = (ema20 - stop_price).abs();
    if risk.is_zero() {
        return None;
    }
    let target_price = match side {
        Side::Buy => ema20 + risk * params.reward_multiple,
        Side::Sell => ema20 - risk * params.reward_multiple,
    };

    Some(Signal {
        symbol: ctx.symbol.clone(),
        side,
        entry_price: ema20,
        stop_price,
        target_price,
        atr,
        signal_candle_open_ms: candle.open_time_ms,
    })
}

/// True when price came within `pullback_atr_fraction × ATR` of the EMA20
/// within the lookback window, in the direction the bias permits.
///
/// Each candle is compared against the EMA as it stood at that candle's close,
/// not today's EMA — otherwise a fast-moving EMA would retroactively invent or
/// erase pullbacks.
fn pullback_occurred(
    params: &StrategyParams,
    state: &SymbolState,
    side: Side,
    atr: Decimal,
) -> bool {
    let threshold = atr * params.pullback_atr_fraction;
    let n = params.pullback_lookback.min(state.recent_h1.len());
    let candles = state.recent_h1.iter().rev().take(n);
    let emas = state.recent_ema_entry.iter().rev().take(n);

    for (c, e) in candles.zip(emas) {
        let touched = match side {
            Side::Buy => (c.low - *e).abs() <= threshold,
            Side::Sell => (c.high - *e).abs() <= threshold,
        };
        if touched {
            return true;
        }
    }
    false
}

/// True when RSI crossed the trigger this candle — not merely sits past it.
fn rsi_triggered(params: &StrategyParams, side: Side, prev: Decimal, now: Decimal) -> bool {
    match side {
        Side::Buy => prev < params.rsi_long_trigger && now >= params.rsi_long_trigger,
        Side::Sell => prev > params.rsi_short_trigger && now <= params.rsi_short_trigger,
    }
}

/// The further from entry of the swing extreme and the ATR-based stop.
///
/// "Further" is deliberate: taking the tighter of the two would put the stop
/// inside recent noise, where it gets hit by ordinary chop rather than by the
/// setup being wrong.
fn stop_for(
    params: &StrategyParams,
    state: &SymbolState,
    side: Side,
    entry: Decimal,
    atr: Decimal,
) -> Decimal {
    let atr_stop = match side {
        Side::Buy => entry - atr * params.atr_stop_multiple,
        Side::Sell => entry + atr * params.atr_stop_multiple,
    };
    match side {
        Side::Buy => {
            let swing_low = state
                .recent_h1
                .iter()
                .map(|c| c.low)
                .min()
                .unwrap_or(atr_stop);
            swing_low.min(atr_stop)
        }
        Side::Sell => {
            let swing_high = state
                .recent_h1
                .iter()
                .map(|c| c.high)
                .max()
                .unwrap_or(atr_stop);
            swing_high.max(atr_stop)
        }
    }
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p strategy --test pullback_setups`
Expected: PASS — 4 tests.

- [ ] **Step 6: Add tests proving the setup gates actually gate**

Append to `crates/strategy/tests/pullback_setups.rs`:

```rust
/// Drive a full long setup: warm 4h long bias, warm 1h indicators with a
/// pullback into EMA20, then a candle whose RSI crosses up through 40.
/// Returns whatever the final candle produced.
fn drive_long_setup(dip_depth: Decimal, final_close_bump: Decimal) -> Option<strategy::Signal> {
    let mut strat = PullbackStrategy::new(StrategyParams::defaults());
    let symbol = Symbol::new("BTCUSDT");
    let inst = instrument();
    warm_long_bias(&mut strat, &symbol, &inst);

    // 1h: 300 candles drifting up, then a dip that pulls RSI below 40 and
    // brings price back to the EMA, then a recovery candle.
    let mut last = None;
    for i in 0..300i64 {
        let px = Decimal::from(1000 + i / 3);
        let c = candle(i * 3_600_000, px + dec!(2), px - dec!(2), px);
        let ctx = MarketContext {
            symbol: &symbol,
            timeframe: Timeframe::H1,
            candle: &c,
            instrument: &inst,
        };
        last = strat.on_candle_close(&ctx);
    }

    // Dip: several down candles push RSI below 40 and price down to the EMA.
    let base = Decimal::from(1000 + 299 / 3);
    for i in 0..8i64 {
        let px = base - dip_depth * Decimal::from(i + 1);
        let c = candle((300 + i) * 3_600_000, px + dec!(1), px - dec!(3), px);
        let ctx = MarketContext {
            symbol: &symbol,
            timeframe: Timeframe::H1,
            candle: &c,
            instrument: &inst,
        };
        last = strat.on_candle_close(&ctx);
    }

    // Recovery candle: closes up, pulling RSI back through 40.
    let px = base - dip_depth * dec!(8) + final_close_bump;
    let c = candle(308 * 3_600_000, px + dec!(2), px - dec!(1), px);
    let ctx = MarketContext {
        symbol: &symbol,
        timeframe: Timeframe::H1,
        candle: &c,
        instrument: &inst,
    };
    last = strat.on_candle_close(&ctx);
    last
}

#[test]
fn a_signal_places_its_stop_below_entry_and_target_above_for_a_long() {
    // Whatever the exact setup, ANY long signal must be internally coherent:
    // stop below entry, target above, and the target exactly 2R away.
    if let Some(sig) = drive_long_setup(dec!(4), dec!(12)) {
        assert_eq!(sig.side, Side::Buy);
        assert!(
            sig.stop_price < sig.entry_price,
            "long stop {} was not below entry {}",
            sig.stop_price,
            sig.entry_price
        );
        assert!(
            sig.target_price > sig.entry_price,
            "long target {} was not above entry {}",
            sig.target_price,
            sig.entry_price
        );
        assert_eq!(
            sig.reward_multiple(),
            Some(dec!(2)),
            "target must sit at exactly 2R"
        );
    }
}

#[test]
fn no_signal_when_bias_is_absent() {
    // Without any 4h history there is no bias, so no 1h candle can fire.
    let mut strat = PullbackStrategy::new(StrategyParams::defaults());
    let symbol = Symbol::new("BTCUSDT");
    let inst = instrument();

    let mut produced = false;
    for i in 0..400i64 {
        let px = Decimal::from(1000 + (i % 20));
        let c = candle(i * 3_600_000, px + dec!(2), px - dec!(2), px);
        let ctx = MarketContext {
            symbol: &symbol,
            timeframe: Timeframe::H1,
            candle: &c,
            instrument: &inst,
        };
        if strat.on_candle_close(&ctx).is_some() {
            produced = true;
        }
    }
    assert!(!produced, "signals fired with no 4h bias established");
}

#[test]
fn flat_prices_are_rejected_by_the_volatility_gate() {
    // A perfectly flat series has ATR 0, which is below the 0.3% floor.
    let mut strat = PullbackStrategy::new(StrategyParams::defaults());
    let symbol = Symbol::new("BTCUSDT");
    let inst = instrument();
    warm_long_bias(&mut strat, &symbol, &inst);

    let mut produced = false;
    for i in 0..400i64 {
        let c = candle(i * 3_600_000, dec!(1000), dec!(1000), dec!(1000));
        let ctx = MarketContext {
            symbol: &symbol,
            timeframe: Timeframe::H1,
            candle: &c,
            instrument: &inst,
        };
        if strat.on_candle_close(&ctx).is_some() {
            produced = true;
        }
    }
    assert!(!produced, "a zero-volatility series produced a signal");
}

#[test]
fn per_symbol_state_is_isolated() {
    // Warming BTCUSDT must not warm ETHUSDT — shared state would let one
    // symbol's trend authorise another symbol's entry.
    let mut strat = PullbackStrategy::new(StrategyParams::defaults());
    let btc = Symbol::new("BTCUSDT");
    let eth = Symbol::new("ETHUSDT");
    let inst = instrument();
    warm_long_bias(&mut strat, &btc, &inst);

    let c = candle(0, dec!(101), dec!(99), dec!(100));
    let ctx = MarketContext {
        symbol: &eth,
        timeframe: Timeframe::H1,
        candle: &c,
        instrument: &inst,
    };
    assert_eq!(
        strat.on_candle_close(&ctx),
        None,
        "ETHUSDT fired on BTCUSDT's warm state"
    );
}
```

- [ ] **Step 7: Run the tests**

Run: `cargo test -p strategy && cargo clippy -p strategy -- -D warnings`
Expected: PASS — 9 tests, no clippy warnings.

If `a_signal_places_its_stop_below_entry_and_target_above_for_a_long` never enters its `if let` (no signal produced), that is acceptable for this task — the other tests still prove the gates reject. Report it as a concern rather than loosening the setup conditions to force a fire.

- [ ] **Step 8: Commit**

```bash
git add crates/strategy
git commit -m "feat(strategy): baseline trend-filtered pullback

4h EMA50/EMA200 sets bias, 1h RSI cross times the entry, and the limit rests
at EMA20. The pullback check compares each candle against the EMA as it stood
then, not today's value, so a fast-moving EMA cannot retroactively invent or
erase a pullback. The stop takes the further of the swing extreme and
1.5xATR — the tighter of the two would sit inside ordinary chop."
```

---

### Task 3: Strategy parameters from config

**Files:**
- Modify: `crates/strategy/src/pullback.rs` (add a `From<&bot::config::StrategyConfig>` equivalent as a free function to avoid a dependency cycle)
- Create: `crates/strategy/src/params_from_config.rs`
- Modify: `crates/strategy/src/lib.rs`
- Test: inline `#[cfg(test)]` in `params_from_config.rs`

**Interfaces:**
- Consumes: `StrategyParams` (Task 2)
- Produces: `params_from_f64_config(ema_fast, ema_slow, ema_entry, rsi_period, rsi_long_trigger, rsi_short_trigger, atr_period, atr_band_min_pct, atr_band_max_pct, swing_lookback, atr_stop_multiple, reward_multiple, entry_expiry_candles, stop_limit_offset_atr) -> Result<StrategyParams, ParamError>` and `ParamError`

`bot` depends on `strategy`, so `strategy` must NOT depend on `bot`. The conversion therefore takes primitives rather than the config struct.

- [ ] **Step 1: Write the failing test**

Create `crates/strategy/src/params_from_config.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn valid() -> Result<StrategyParams, ParamError> {
        params_from_f64_config(50, 200, 20, 14, 40.0, 60.0, 14, 0.003, 0.05, 10, 1.5, 2.0)
    }

    #[test]
    fn valid_config_converts_f64_knobs_to_decimal() {
        let p = valid().expect("valid config converts");
        assert_eq!(p.rsi_long_trigger, dec!(40));
        assert_eq!(p.atr_band_min_pct, dec!(0.003));
        assert_eq!(p.atr_stop_multiple, dec!(1.5));
        assert_eq!(p.reward_multiple, dec!(2));
    }

    #[test]
    fn zero_period_is_rejected() {
        // Ema::new/Rsi::new/Atr::new assert on period > 0; catching it here
        // turns a panic deep in indicator construction into a startup error.
        let e = params_from_f64_config(0, 200, 20, 14, 40.0, 60.0, 14, 0.003, 0.05, 10, 1.5, 2.0)
            .expect_err("zero period must be rejected");
        assert!(matches!(e, ParamError::ZeroPeriod(_)));
    }

    #[test]
    fn fast_ema_must_be_shorter_than_slow() {
        let e = params_from_f64_config(200, 50, 20, 14, 40.0, 60.0, 14, 0.003, 0.05, 10, 1.5, 2.0)
            .expect_err("inverted EMAs must be rejected");
        assert!(matches!(e, ParamError::EmaOrder { .. }));
    }

    #[test]
    fn volatility_band_must_be_ordered() {
        let e = params_from_f64_config(50, 200, 20, 14, 40.0, 60.0, 14, 0.05, 0.003, 10, 1.5, 2.0)
            .expect_err("inverted ATR band must be rejected");
        assert!(matches!(e, ParamError::BandOrder { .. }));
    }

    #[test]
    fn rsi_triggers_must_leave_room_between_them() {
        // A long trigger at or above the short trigger means both sides could
        // fire on the same candle.
        let e = params_from_f64_config(50, 200, 20, 14, 60.0, 40.0, 14, 0.003, 0.05, 10, 1.5, 2.0)
            .expect_err("overlapping RSI triggers must be rejected");
        assert!(matches!(e, ParamError::RsiTriggerOrder { .. }));
    }

    #[test]
    fn non_finite_f64_is_rejected_rather_than_producing_a_garbage_decimal() {
        let e = params_from_f64_config(
            50, 200, 20, 14, f64::NAN, 60.0, 14, 0.003, 0.05, 10, 1.5, 2.0,
        )
        .expect_err("NaN must be rejected");
        assert!(matches!(e, ParamError::NotFinite(_)));
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p strategy`
Expected: FAIL — `params_from_f64_config` not found.

- [ ] **Step 3: Implement the conversion**

Prepend to `crates/strategy/src/params_from_config.rs`:

```rust
use rust_decimal::Decimal;
use rust_decimal::prelude::FromPrimitive;

use crate::pullback::StrategyParams;

/// Why a config could not become usable strategy parameters.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ParamError {
    #[error("{0} must be greater than zero")]
    ZeroPeriod(&'static str),

    #[error("ema_fast ({fast}) must be shorter than ema_slow ({slow})")]
    EmaOrder { fast: usize, slow: usize },

    #[error("atr_band_min_pct ({min}) must be below atr_band_max_pct ({max})")]
    BandOrder { min: f64, max: f64 },

    #[error("rsi_long_trigger ({long}) must be below rsi_short_trigger ({short})")]
    RsiTriggerOrder { long: f64, short: f64 },

    #[error("{0} is not a finite number")]
    NotFinite(&'static str),
}

fn dec(value: f64, field: &'static str) -> Result<Decimal, ParamError> {
    if !value.is_finite() {
        return Err(ParamError::NotFinite(field));
    }
    Decimal::from_f64(value).ok_or(ParamError::NotFinite(field))
}

/// Build validated strategy parameters from the primitive values the config
/// file carries.
///
/// Takes primitives rather than the config struct because `bot` depends on
/// `strategy`; accepting the struct would invert that and create a cycle.
///
/// Validation here turns what would otherwise be a panic deep inside indicator
/// construction — `Ema::new` asserts `period > 0` — into a clear startup error.
#[allow(clippy::too_many_arguments)]
pub fn params_from_f64_config(
    ema_fast: usize,
    ema_slow: usize,
    ema_entry: usize,
    rsi_period: usize,
    rsi_long_trigger: f64,
    rsi_short_trigger: f64,
    atr_period: usize,
    atr_band_min_pct: f64,
    atr_band_max_pct: f64,
    swing_lookback: usize,
    atr_stop_multiple: f64,
    reward_multiple: f64,
) -> Result<StrategyParams, ParamError> {
    for (value, name) in [
        (ema_fast, "ema_fast"),
        (ema_slow, "ema_slow"),
        (ema_entry, "ema_entry"),
        (rsi_period, "rsi_period"),
        (atr_period, "atr_period"),
        (swing_lookback, "swing_lookback"),
    ] {
        if value == 0 {
            return Err(ParamError::ZeroPeriod(name));
        }
    }

    if ema_fast >= ema_slow {
        return Err(ParamError::EmaOrder {
            fast: ema_fast,
            slow: ema_slow,
        });
    }
    if atr_band_min_pct >= atr_band_max_pct {
        return Err(ParamError::BandOrder {
            min: atr_band_min_pct,
            max: atr_band_max_pct,
        });
    }
    if rsi_long_trigger >= rsi_short_trigger {
        return Err(ParamError::RsiTriggerOrder {
            long: rsi_long_trigger,
            short: rsi_short_trigger,
        });
    }

    Ok(StrategyParams {
        ema_fast,
        ema_slow,
        ema_entry,
        rsi_period,
        rsi_long_trigger: dec(rsi_long_trigger, "rsi_long_trigger")?,
        rsi_short_trigger: dec(rsi_short_trigger, "rsi_short_trigger")?,
        atr_period,
        atr_band_min_pct: dec(atr_band_min_pct, "atr_band_min_pct")?,
        atr_band_max_pct: dec(atr_band_max_pct, "atr_band_max_pct")?,
        swing_lookback,
        atr_stop_multiple: dec(atr_stop_multiple, "atr_stop_multiple")?,
        reward_multiple: dec(reward_multiple, "reward_multiple")?,
        pullback_lookback: 5,
        pullback_atr_fraction: Decimal::new(5, 1),
    })
}
```

Add `thiserror.workspace = true` to `crates/strategy/Cargo.toml` dependencies.

Add to `crates/strategy/src/lib.rs`:

```rust
pub mod params_from_config;

pub use params_from_config::{ParamError, params_from_f64_config};
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p strategy && cargo clippy -p strategy -- -D warnings`
Expected: PASS — 15 tests, no clippy warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/strategy
git commit -m "feat(strategy): validated parameter construction from config

Rejects zero periods, inverted EMA and ATR-band ordering, overlapping RSI
triggers, and non-finite floats at startup rather than letting them become a
panic inside Ema::new or a garbage Decimal mid-session."
```

---

### Task 4: Position sizing and the liquidation buffer

**Files:**
- Create: `crates/risk/Cargo.toml`, `crates/risk/src/lib.rs`, `crates/risk/src/sizing.rs`
- Modify: root `Cargo.toml` (add `"crates/risk"` to members)

**Interfaces:**
- Consumes: `botcore::{Instrument, Side}`, `botcore::money::round_down_to_step`
- Produces:
  - `RiskParams { risk_pct, max_concurrent_positions, max_daily_entries, daily_drawdown_halt_pct, total_drawdown_halt_pct, liq_buffer_multiple }` (all `Decimal` except the two counts) with `RiskParams::defaults()`
  - `position_size(equity: Decimal, risk_pct: Decimal, stop_distance: Decimal, qty_step: Decimal) -> Option<Decimal>`
  - `liquidation_is_safe(entry: Decimal, stop: Decimal, liq_price: Option<Decimal>, buffer_multiple: Decimal) -> bool`

- [ ] **Step 1: Create the crate manifest**

Create `crates/risk/Cargo.toml`:

```toml
[package]
name = "risk"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true

[dependencies]
botcore = { path = "../botcore" }
strategy = { path = "../strategy" }
rust_decimal.workspace = true
thiserror.workspace = true

[dev-dependencies]
rust_decimal_macros.workspace = true
proptest = "1"
```

Add `"crates/risk"` to the root `Cargo.toml` members array.

- [ ] **Step 2: Write the failing sizing test**

Create `crates/risk/src/sizing.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn size_is_risk_budget_divided_by_stop_distance() {
        // 1% of 10,000 = 100 risked; a $5 stop distance buys 20 units.
        let size = position_size(dec!(10000), dec!(0.01), dec!(5), dec!(0.001))
            .expect("valid inputs produce a size");
        assert_eq!(size, dec!(20));
    }

    #[test]
    fn size_rounds_down_to_the_quantity_step() {
        // 100 / 3 = 33.333...; with a 0.01 step that must become 33.33,
        // never 33.34 — rounding up would risk more than the budget.
        let size = position_size(dec!(10000), dec!(0.01), dec!(3), dec!(0.01))
            .expect("valid inputs produce a size");
        assert_eq!(size, dec!(33.33));
    }

    #[test]
    fn zero_stop_distance_yields_none_rather_than_dividing_by_zero() {
        assert_eq!(position_size(dec!(10000), dec!(0.01), dec!(0), dec!(0.001)), None);
    }

    #[test]
    fn non_positive_equity_yields_none() {
        // A zero-equity account (an unfunded testnet account, for instance)
        // must produce no position rather than a zero-size order.
        assert_eq!(position_size(dec!(0), dec!(0.01), dec!(5), dec!(0.001)), None);
        assert_eq!(position_size(dec!(-100), dec!(0.01), dec!(5), dec!(0.001)), None);
    }

    #[test]
    fn a_size_rounding_to_zero_yields_none() {
        // Tiny equity against a wide stop and a coarse step rounds to nothing;
        // that must be None, not an order for zero units.
        assert_eq!(position_size(dec!(10), dec!(0.01), dec!(5000), dec!(1)), None);
    }

    #[test]
    fn liquidation_far_beyond_the_stop_is_safe() {
        // Long at 100, stop at 95 (distance 5), buffer 3 => liquidation must
        // be at or below 85. At 80 it is comfortably clear.
        assert!(liquidation_is_safe(dec!(100), dec!(95), Some(dec!(80)), dec!(3)));
    }

    #[test]
    fn liquidation_inside_the_buffer_is_unsafe() {
        // Liquidation at 90 is only 2 stop-distances away, inside the 3x rule.
        assert!(!liquidation_is_safe(dec!(100), dec!(95), Some(dec!(90)), dec!(3)));
    }

    #[test]
    fn liquidation_between_entry_and_stop_is_unsafe() {
        // The exchange would close the position before the stop ever triggers.
        assert!(!liquidation_is_safe(dec!(100), dec!(95), Some(dec!(97)), dec!(3)));
    }

    #[test]
    fn short_side_buffer_is_measured_upward() {
        // Short at 100, stop at 105 (distance 5), buffer 3 => liquidation must
        // be at or above 115.
        assert!(liquidation_is_safe(dec!(100), dec!(105), Some(dec!(120)), dec!(3)));
        assert!(!liquidation_is_safe(dec!(100), dec!(105), Some(dec!(110)), dec!(3)));
    }

    #[test]
    fn absent_liquidation_price_is_treated_as_safe() {
        // Bybit reports no liquidation price when there is no liquidation risk
        // on the position. Absent must not be read as zero, which would look
        // infinitely far away on a short and adjacent on a long.
        assert!(liquidation_is_safe(dec!(100), dec!(95), None, dec!(3)));
    }
}
```

- [ ] **Step 3: Run the test to verify it fails**

Run: `cargo test -p risk`
Expected: FAIL — `position_size` and `liquidation_is_safe` not found.

- [ ] **Step 4: Implement sizing**

Prepend to `crates/risk/src/sizing.rs`:

```rust
use botcore::money::round_down_to_step;
use rust_decimal::Decimal;

/// The owner's hard risk envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RiskParams {
    /// Fraction of equity risked per trade, e.g. 0.01 for 1%.
    pub risk_pct: Decimal,
    pub max_concurrent_positions: usize,
    pub max_daily_entries: u32,
    /// Fraction, e.g. 0.05 for a −5% daily halt.
    pub daily_drawdown_halt_pct: Decimal,
    /// Fraction, e.g. 0.15 for a −15% total halt.
    pub total_drawdown_halt_pct: Decimal,
    /// Liquidation must sit at least this many stop-distances from entry.
    pub liq_buffer_multiple: Decimal,
}

impl RiskParams {
    /// The spec's envelope.
    pub fn defaults() -> Self {
        RiskParams {
            risk_pct: Decimal::new(1, 2),              // 0.01
            max_concurrent_positions: 4,
            max_daily_entries: 5,
            daily_drawdown_halt_pct: Decimal::new(5, 2),  // 0.05
            total_drawdown_halt_pct: Decimal::new(15, 2), // 0.15
            liq_buffer_multiple: Decimal::from(3),
        }
    }
}

/// Position size from the risk budget and the stop distance.
///
/// `size = (risk_pct × equity) ÷ stop_distance`, rounded DOWN to `qty_step`.
/// Deriving size from stop distance rather than from available margin is what
/// makes leverage a margin-efficiency setting rather than a risk multiplier:
/// the amount at risk is the same at 1x as at 10x.
///
/// Returns `None` — never a zero-size order — when equity is non-positive, the
/// stop distance is zero, or the result rounds away to nothing.
pub fn position_size(
    equity: Decimal,
    risk_pct: Decimal,
    stop_distance: Decimal,
    qty_step: Decimal,
) -> Option<Decimal> {
    if equity <= Decimal::ZERO || stop_distance <= Decimal::ZERO || risk_pct <= Decimal::ZERO {
        return None;
    }
    let budget = equity * risk_pct;
    let raw = budget / stop_distance;
    let stepped = round_down_to_step(raw, qty_step);
    if stepped <= Decimal::ZERO {
        return None;
    }
    Some(stepped)
}

/// Whether liquidation sits far enough beyond the stop.
///
/// This is what makes a limit-only stop survivable: by requiring liquidation to
/// be at least `buffer_multiple` stop-distances away, a gap through the stop has
/// room for the escalation ladder to fill before the exchange force-closes.
/// In practice it caps the leverage any individual setup can use — setups
/// needing more are rejected outright rather than quietly resized.
///
/// The direction is inferred from stop versus entry: a stop below entry is a
/// long, above is a short. An absent liquidation price means the exchange sees
/// no liquidation risk and is treated as safe.
pub fn liquidation_is_safe(
    entry: Decimal,
    stop: Decimal,
    liq_price: Option<Decimal>,
    buffer_multiple: Decimal,
) -> bool {
    let Some(liq) = liq_price else {
        return true;
    };
    let distance = (entry - stop).abs();
    if distance.is_zero() {
        return false;
    }
    let required = distance * buffer_multiple;
    if stop < entry {
        // Long: liquidation lies below, and must be at least `required` below.
        liq <= entry - required
    } else {
        // Short: liquidation lies above.
        liq >= entry + required
    }
}
```

Create `crates/risk/src/lib.rs`:

```rust
pub mod sizing;

pub use sizing::{RiskParams, liquidation_is_safe, position_size};
```

- [ ] **Step 5: Run the tests**

Run: `cargo test -p risk`
Expected: PASS — 10 tests.

- [ ] **Step 6: Add a property test proving risk is never exceeded**

Append to the `tests` module in `crates/risk/src/sizing.rs`:

```rust
    use proptest::prelude::*;

    proptest! {
        /// The realized risk of a position — size × stop distance — must never
        /// exceed the budget, for any combination of equity, stop distance and
        /// quantity step. This is the invariant the whole risk model rests on.
        #[test]
        fn realized_risk_never_exceeds_the_budget(
            equity_cents in 1_000i64..1_000_000_000i64,
            stop_cents in 1i64..10_000_000i64,
            step_choice in 0usize..4usize,
        ) {
            let equity = Decimal::new(equity_cents, 2);
            let stop_distance = Decimal::new(stop_cents, 2);
            let qty_step = [dec!(0.0001), dec!(0.001), dec!(0.01), dec!(1)][step_choice];
            let risk_pct = dec!(0.01);

            if let Some(size) = position_size(equity, risk_pct, stop_distance, qty_step) {
                let budget = equity * risk_pct;
                let realized = size * stop_distance;
                prop_assert!(
                    realized <= budget,
                    "realized risk {realized} exceeded budget {budget} \
                     (equity {equity}, stop {stop_distance}, step {qty_step})"
                );
                prop_assert!(size > Decimal::ZERO, "a Some size must be positive");
            }
        }
    }
```

- [ ] **Step 7: Run the property test**

Run: `cargo test -p risk && cargo clippy -p risk -- -D warnings`
Expected: PASS — 11 tests, no clippy warnings.

- [ ] **Step 8: Commit**

```bash
git add crates/risk Cargo.toml Cargo.lock
git commit -m "feat(risk): stop-distance position sizing and the liquidation buffer

Size derives from stop distance, not available margin, so leverage changes
only how much margin locks — not how much is risked. A proptest asserts
realized risk never exceeds the budget across random equity, stop and step
combinations. The 3x liquidation buffer is what makes a limit-only stop
survivable: a gap has room to fill before the exchange force-closes."
```

---

### Task 5: Hard limits — concurrency, daily cap, drawdown halt

**Files:**
- Create: `crates/risk/src/limits.rs`
- Modify: `crates/risk/src/lib.rs`

**Interfaces:**
- Consumes: `RiskParams` (Task 4), `botcore::{Position, Symbol}`
- Produces:
  - `AccountState { equity, available, open_positions: Vec<Position>, day_start_equity, high_water_mark, entries_filled_today: u32, halt_reason: Option<String> }`
  - `Refusal` enum
  - `check_entry_allowed(state: &AccountState, params: &RiskParams, symbol: &Symbol) -> Result<(), Refusal>`
  - `drawdown_breach(state: &AccountState, params: &RiskParams) -> Option<Refusal>`

- [ ] **Step 1: Write the failing limits test**

Create `crates/risk/src/limits.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use botcore::{Position, Side, Symbol};
    use rust_decimal_macros::dec;

    fn position(sym: &str) -> Position {
        Position {
            symbol: Symbol::new(sym),
            side: Side::Buy,
            size: dec!(1),
            entry_price: dec!(100),
            liq_price: None,
            unrealized_pnl: dec!(0),
        }
    }

    fn healthy() -> AccountState {
        AccountState {
            equity: dec!(10000),
            available: dec!(9000),
            open_positions: vec![],
            day_start_equity: dec!(10000),
            high_water_mark: dec!(10000),
            entries_filled_today: 0,
            halt_reason: None,
        }
    }

    #[test]
    fn a_healthy_account_may_enter() {
        let p = RiskParams::defaults();
        assert!(check_entry_allowed(&healthy(), &p, &Symbol::new("BTCUSDT")).is_ok());
    }

    #[test]
    fn a_persisted_halt_blocks_entry() {
        let mut s = healthy();
        s.halt_reason = Some("daily drawdown".into());
        let e = check_entry_allowed(&s, &RiskParams::defaults(), &Symbol::new("BTCUSDT"))
            .expect_err("a halted account must refuse");
        assert!(matches!(e, Refusal::Halted { .. }));
    }

    #[test]
    fn the_fifth_entry_is_allowed_and_the_sixth_is_not() {
        let p = RiskParams::defaults();
        let mut s = healthy();
        s.entries_filled_today = 4;
        assert!(check_entry_allowed(&s, &p, &Symbol::new("BTCUSDT")).is_ok());

        s.entries_filled_today = 5;
        let e = check_entry_allowed(&s, &p, &Symbol::new("BTCUSDT"))
            .expect_err("the sixth entry of a day must refuse");
        assert!(matches!(e, Refusal::DailyCapReached { cap: 5, .. }));
    }

    #[test]
    fn a_fifth_concurrent_position_is_refused() {
        let p = RiskParams::defaults();
        let mut s = healthy();
        s.open_positions = vec![
            position("BTCUSDT"),
            position("ETHUSDT"),
            position("SOLUSDT"),
            position("XRPUSDT"),
        ];
        let e = check_entry_allowed(&s, &p, &Symbol::new("ADAUSDT"))
            .expect_err("a fifth concurrent position must refuse");
        assert!(matches!(e, Refusal::TooManyPositions { limit: 4, .. }));
    }

    #[test]
    fn a_second_position_in_the_same_symbol_is_refused() {
        let p = RiskParams::defaults();
        let mut s = healthy();
        s.open_positions = vec![position("BTCUSDT")];
        let e = check_entry_allowed(&s, &p, &Symbol::new("BTCUSDT"))
            .expect_err("doubling up on one symbol must refuse");
        assert!(matches!(e, Refusal::AlreadyInSymbol { .. }));
    }

    #[test]
    fn daily_drawdown_at_the_threshold_breaches() {
        let p = RiskParams::defaults();
        let mut s = healthy();
        // −5% exactly from the 00:00 UTC mark.
        s.equity = dec!(9500);
        let b = drawdown_breach(&s, &p).expect("−5% must breach");
        assert!(matches!(b, Refusal::DailyDrawdown { .. }));
    }

    #[test]
    fn daily_drawdown_just_inside_the_threshold_does_not_breach() {
        let p = RiskParams::defaults();
        let mut s = healthy();
        s.equity = dec!(9501);
        assert_eq!(drawdown_breach(&s, &p), None);
    }

    #[test]
    fn total_drawdown_is_measured_from_the_high_water_mark_not_the_day_start() {
        let p = RiskParams::defaults();
        let mut s = healthy();
        // The account peaked at 20,000 and is now at 17,000: −15% from peak,
        // even though it is up on the day.
        s.high_water_mark = dec!(20000);
        s.day_start_equity = dec!(16000);
        s.equity = dec!(17000);
        let b = drawdown_breach(&s, &p).expect("−15% from the peak must breach");
        assert!(matches!(b, Refusal::TotalDrawdown { .. }));
    }

    #[test]
    fn a_non_positive_day_start_cannot_produce_a_division_by_zero() {
        let p = RiskParams::defaults();
        let mut s = healthy();
        s.day_start_equity = dec!(0);
        s.high_water_mark = dec!(0);
        s.equity = dec!(0);
        // No baseline means no measurable drawdown, not a panic.
        assert_eq!(drawdown_breach(&s, &p), None);
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p risk`
Expected: FAIL — `AccountState`, `Refusal`, `check_entry_allowed` not found.

- [ ] **Step 3: Implement the limits**

Prepend to `crates/risk/src/limits.rs`:

```rust
use botcore::{Position, Symbol};
use rust_decimal::Decimal;

use crate::sizing::RiskParams;

/// Everything the risk layer needs to know about the account right now.
///
/// Assembled by the engine from the exchange (positions, equity) and the
/// journal (today's fill count, the persisted halt, the high-water mark).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountState {
    pub equity: Decimal,
    pub available: Decimal,
    pub open_positions: Vec<Position>,
    /// Equity at the most recent 00:00 UTC boundary.
    pub day_start_equity: Decimal,
    /// All-time peak equity, persisted across restarts.
    pub high_water_mark: Decimal,
    /// Entries FILLED so far this UTC day. Cancelled and expired limit orders
    /// do not count, which is why this is a fill count rather than a
    /// placement count.
    pub entries_filled_today: u32,
    /// Set when a halt is in force. Persisted, so a restart cannot clear it.
    pub halt_reason: Option<String>,
}

/// Why an entry was refused. Every refusal is nameable — the engine logs the
/// reason rather than silently skipping.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("trading is halted: {reason}")]
    Halted { reason: String },

    #[error("daily entry cap reached: {filled} of {cap} filled today")]
    DailyCapReached { filled: u32, cap: u32 },

    #[error("already at the concurrent position limit: {open} of {limit}")]
    TooManyPositions { open: usize, limit: usize },

    #[error("already holding a position in {symbol}")]
    AlreadyInSymbol { symbol: String },

    #[error("daily drawdown {pct} breached the {limit} limit")]
    DailyDrawdown { pct: Decimal, limit: Decimal },

    #[error("total drawdown {pct} from the high-water mark breached the {limit} limit")]
    TotalDrawdown { pct: Decimal, limit: Decimal },

    #[error("liquidation price sits nearer than {multiple}x the stop distance")]
    LiquidationTooClose { multiple: Decimal },

    #[error("position size rounded to zero or equity is non-positive")]
    SizeTooSmall,

    #[error("order notional {notional} is below the instrument minimum {minimum}")]
    BelowMinimumQty { notional: Decimal, minimum: Decimal },

    #[error("order notional {notional} exceeds available margin {available}")]
    InsufficientMargin { notional: Decimal, available: Decimal },
}

/// Whether a new entry in `symbol` is permitted right now.
///
/// Checks the cheap, certain refusals: the persisted halt, the daily cap, the
/// concurrency limit, and one-position-per-symbol. Sizing-dependent refusals
/// live in the manager, which knows the prices.
pub fn check_entry_allowed(
    state: &AccountState,
    params: &RiskParams,
    symbol: &Symbol,
) -> Result<(), Refusal> {
    if let Some(reason) = &state.halt_reason {
        return Err(Refusal::Halted {
            reason: reason.clone(),
        });
    }

    if state.entries_filled_today >= params.max_daily_entries {
        return Err(Refusal::DailyCapReached {
            filled: state.entries_filled_today,
            cap: params.max_daily_entries,
        });
    }

    if state
        .open_positions
        .iter()
        .any(|p| p.symbol.as_str() == symbol.as_str())
    {
        return Err(Refusal::AlreadyInSymbol {
            symbol: symbol.as_str().to_string(),
        });
    }

    if state.open_positions.len() >= params.max_concurrent_positions {
        return Err(Refusal::TooManyPositions {
            open: state.open_positions.len(),
            limit: params.max_concurrent_positions,
        });
    }

    Ok(())
}

/// Whether equity has fallen far enough to trip a halt.
///
/// Daily drawdown measures from the 00:00 UTC equity mark; total drawdown from
/// the all-time high-water mark. A non-positive baseline yields `None` rather
/// than dividing by zero — an account with no recorded baseline has no
/// measurable drawdown.
pub fn drawdown_breach(state: &AccountState, params: &RiskParams) -> Option<Refusal> {
    if state.day_start_equity > Decimal::ZERO {
        let fall = state.day_start_equity - state.equity;
        if fall > Decimal::ZERO {
            let pct = fall / state.day_start_equity;
            if pct >= params.daily_drawdown_halt_pct {
                return Some(Refusal::DailyDrawdown {
                    pct,
                    limit: params.daily_drawdown_halt_pct,
                });
            }
        }
    }

    if state.high_water_mark > Decimal::ZERO {
        let fall = state.high_water_mark - state.equity;
        if fall > Decimal::ZERO {
            let pct = fall / state.high_water_mark;
            if pct >= params.total_drawdown_halt_pct {
                return Some(Refusal::TotalDrawdown {
                    pct,
                    limit: params.total_drawdown_halt_pct,
                });
            }
        }
    }

    None
}
```

Update `crates/risk/src/lib.rs`:

```rust
pub mod limits;
pub mod sizing;

pub use limits::{AccountState, Refusal, check_entry_allowed, drawdown_breach};
pub use sizing::{RiskParams, liquidation_is_safe, position_size};
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p risk && cargo clippy -p risk -- -D warnings`
Expected: PASS — 20 tests, no clippy warnings.

- [ ] **Step 5: Commit**

```bash
git add crates/risk
git commit -m "feat(risk): concurrency, daily cap and drawdown halt limits

The daily cap counts FILLED entries, so cancelled and expired limit orders
cost nothing against the budget. Total drawdown measures from the all-time
high-water mark rather than the day's start, so an account that is up on the
day can still be halted for being far off its peak. A non-positive baseline
yields no breach rather than dividing by zero."
```

---

### Task 6: RiskManager — Signal to OrderIntent

**Files:**
- Create: `crates/risk/src/manager.rs`
- Modify: `crates/risk/src/lib.rs`
- Test: `crates/risk/tests/manager_decisions.rs`

**Interfaces:**
- Consumes: everything from Tasks 4–5; `strategy::Signal`; `botcore::{Instrument, Side}`
- Produces:
  - `OrderIntent { symbol, side, qty, entry_price, stop_price, stop_limit_price, target_price, atr, signal_candle_open_ms }`
  - `Decision` enum: `Enter(OrderIntent)` | `Refuse(Refusal)`
  - `RiskManager::new(params: RiskParams, stop_limit_offset_atr: Decimal) -> Self`
  - `RiskManager::evaluate(&self, signal: &Signal, state: &AccountState, instrument: &Instrument, liq_price_estimate: Option<Decimal>) -> Decision`

- [ ] **Step 1: Write the failing manager test**

Create `crates/risk/tests/manager_decisions.rs`:

```rust
use botcore::{Instrument, Position, Side, Symbol};
use risk::{AccountState, Decision, Refusal, RiskManager, RiskParams};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use strategy::Signal;

fn instrument() -> Instrument {
    Instrument {
        symbol: Symbol::new("BTCUSDT"),
        tick_size: dec!(0.1),
        qty_step: dec!(0.001),
        min_order_qty: dec!(0.001),
        launch_time_ms: 0,
    }
}

fn long_signal() -> Signal {
    Signal {
        symbol: Symbol::new("BTCUSDT"),
        side: Side::Buy,
        entry_price: dec!(100),
        stop_price: dec!(95),
        target_price: dec!(110),
        atr: dec!(2),
        signal_candle_open_ms: 1_700_000_000_000,
    }
}

fn healthy() -> AccountState {
    AccountState {
        equity: dec!(10000),
        available: dec!(9000),
        open_positions: vec![],
        day_start_equity: dec!(10000),
        high_water_mark: dec!(10000),
        entries_filled_today: 0,
        halt_reason: None,
    }
}

fn manager() -> RiskManager {
    RiskManager::new(RiskParams::defaults(), dec!(0.3))
}

#[test]
fn a_clean_signal_becomes_a_sized_intent() {
    let d = manager().evaluate(&long_signal(), &healthy(), &instrument(), None);
    let Decision::Enter(intent) = d else {
        panic!("expected Enter, got {d:?}");
    };
    // 1% of 10,000 = 100 risked, $5 stop distance => 20 units.
    assert_eq!(intent.qty, dec!(20));
    assert_eq!(intent.entry_price, dec!(100));
    assert_eq!(intent.stop_price, dec!(95));
    assert_eq!(intent.target_price, dec!(110));
    assert_eq!(intent.side, Side::Buy);
    assert_eq!(intent.signal_candle_open_ms, 1_700_000_000_000);
}

#[test]
fn the_stop_limit_sits_beyond_the_trigger_for_a_long() {
    // Offset 0.3 x ATR(2) = 0.6, placed BELOW the stop on a long so the order
    // fills into the move rather than at its edge.
    let d = manager().evaluate(&long_signal(), &healthy(), &instrument(), None);
    let Decision::Enter(intent) = d else {
        panic!("expected Enter");
    };
    assert_eq!(intent.stop_limit_price, dec!(94.4));
}

#[test]
fn the_stop_limit_sits_beyond_the_trigger_for_a_short() {
    let mut sig = long_signal();
    sig.side = Side::Sell;
    sig.stop_price = dec!(105);
    sig.target_price = dec!(90);

    let d = manager().evaluate(&sig, &healthy(), &instrument(), None);
    let Decision::Enter(intent) = d else {
        panic!("expected Enter");
    };
    // On a short the stop lies above, so the limit is placed above it.
    assert_eq!(intent.stop_limit_price, dec!(105.6));
}

#[test]
fn a_halted_account_refuses_before_sizing() {
    let mut s = healthy();
    s.halt_reason = Some("daily drawdown".into());
    let d = manager().evaluate(&long_signal(), &s, &instrument(), None);
    assert!(matches!(d, Decision::Refuse(Refusal::Halted { .. })));
}

#[test]
fn a_drawdown_breach_refuses_even_when_no_halt_is_persisted_yet() {
    // The halt flag is written by the engine after this fires; the manager
    // must refuse on the measurement itself, not wait for the flag.
    let mut s = healthy();
    s.equity = dec!(9000); // −10% on the day
    let d = manager().evaluate(&long_signal(), &s, &instrument(), None);
    assert!(matches!(d, Decision::Refuse(Refusal::DailyDrawdown { .. })));
}

#[test]
fn liquidation_inside_the_buffer_refuses_rather_than_resizing() {
    // Stop distance 5, buffer 3 => liquidation must be at or below 85.
    let d = manager().evaluate(&long_signal(), &healthy(), &instrument(), Some(dec!(90)));
    assert!(matches!(
        d,
        Decision::Refuse(Refusal::LiquidationTooClose { .. })
    ));
}

#[test]
fn zero_equity_refuses_with_size_too_small() {
    // An unfunded account must produce a named refusal, not a zero-size order.
    let mut s = healthy();
    s.equity = dec!(0);
    s.day_start_equity = dec!(0);
    s.high_water_mark = dec!(0);
    let d = manager().evaluate(&long_signal(), &s, &instrument(), None);
    assert!(matches!(d, Decision::Refuse(Refusal::SizeTooSmall)));
}

#[test]
fn a_size_below_the_instrument_minimum_refuses() {
    let mut inst = instrument();
    inst.min_order_qty = dec!(1000);
    let d = manager().evaluate(&long_signal(), &healthy(), &inst, None);
    assert!(matches!(
        d,
        Decision::Refuse(Refusal::BelowMinimumQty { .. })
    ));
}

#[test]
fn notional_exceeding_available_margin_refuses() {
    // 20 units at 100 = 2,000 notional against only 500 available.
    let mut s = healthy();
    s.available = dec!(500);
    let d = manager().evaluate(&long_signal(), &s, &instrument(), None);
    assert!(matches!(
        d,
        Decision::Refuse(Refusal::InsufficientMargin { .. })
    ));
}

#[test]
fn a_second_position_in_the_same_symbol_refuses() {
    let mut s = healthy();
    s.open_positions = vec![Position {
        symbol: Symbol::new("BTCUSDT"),
        side: Side::Buy,
        size: dec!(1),
        entry_price: dec!(100),
        liq_price: None,
        unrealized_pnl: dec!(0),
    }];
    let d = manager().evaluate(&long_signal(), &s, &instrument(), None);
    assert!(matches!(d, Decision::Refuse(Refusal::AlreadyInSymbol { .. })));
}

#[test]
fn entry_and_stop_prices_are_rounded_to_the_instruments_tick() {
    let mut sig = long_signal();
    sig.entry_price = dec!(100.567);
    sig.stop_price = dec!(95.123);
    let d = manager().evaluate(&sig, &healthy(), &instrument(), None);
    let Decision::Enter(intent) = d else {
        panic!("expected Enter");
    };
    // tick 0.1; a buy limit rounds DOWN, away from the market.
    assert_eq!(intent.entry_price, dec!(100.5));
    // Every transmitted price must land on a tick or Bybit rejects the order.
    let ticks = intent.stop_price / dec!(0.1);
    assert_eq!(ticks.fract(), Decimal::ZERO, "stop {} is off-tick", intent.stop_price);
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p risk --test manager_decisions`
Expected: FAIL — `RiskManager`, `Decision`, `OrderIntent` not found.

- [ ] **Step 3: Implement the manager**

Create `crates/risk/src/manager.rs`:

```rust
use botcore::money::{round_down_to_step, round_price_away_from_market};
use botcore::{Instrument, Side, Symbol};
use rust_decimal::Decimal;
use strategy::Signal;

use crate::limits::{AccountState, Refusal, check_entry_allowed, drawdown_breach};
use crate::sizing::{RiskParams, liquidation_is_safe, position_size};

/// A fully-sized order, ready for the executor to turn into a `LimitEntry`.
///
/// The executor adds only the deterministic `orderLinkId`, derived from
/// `symbol`, `signal_candle_open_ms` and `side`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderIntent {
    pub symbol: Symbol,
    pub side: Side,
    pub qty: Decimal,
    pub entry_price: Decimal,
    pub stop_price: Decimal,
    /// Limit price for the stop, placed BEYOND the trigger so it fills into
    /// the move rather than at its edge.
    pub stop_limit_price: Decimal,
    pub target_price: Decimal,
    pub atr: Decimal,
    pub signal_candle_open_ms: i64,
}

/// The outcome of evaluating one signal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Enter(OrderIntent),
    Refuse(Refusal),
}

/// The only component that computes a position size.
///
/// Every hard limit the owner set is enforced here, and the strategy cannot
/// reach around it — strategies emit prices, never quantities.
pub struct RiskManager {
    params: RiskParams,
    /// Fraction of ATR the stop-limit sits beyond its trigger.
    stop_limit_offset_atr: Decimal,
}

impl RiskManager {
    pub fn new(params: RiskParams, stop_limit_offset_atr: Decimal) -> Self {
        RiskManager {
            params,
            stop_limit_offset_atr,
        }
    }

    pub fn params(&self) -> &RiskParams {
        &self.params
    }

    /// Turn a signal into a sized intent, or a named refusal.
    ///
    /// `liq_price_estimate` is the liquidation price the position would have.
    /// `None` means the exchange reports no liquidation risk.
    ///
    /// Checks run cheapest-and-most-certain first: the persisted halt and the
    /// counting limits before any arithmetic, then drawdown, then sizing.
    pub fn evaluate(
        &self,
        signal: &Signal,
        state: &AccountState,
        instrument: &Instrument,
        liq_price_estimate: Option<Decimal>,
    ) -> Decision {
        if let Err(refusal) = check_entry_allowed(state, &self.params, &signal.symbol) {
            return Decision::Refuse(refusal);
        }

        // Measure drawdown directly rather than waiting for the engine to have
        // persisted a halt flag — the flag is written after this fires.
        if let Some(breach) = drawdown_breach(state, &self.params) {
            return Decision::Refuse(breach);
        }

        // Round prices to the instrument's tick before anything derives from
        // them, so the size matches the price actually transmitted.
        let entry = round_price_away_from_market(signal.entry_price, instrument.tick_size, signal.side);
        let stop = round_stop_to_tick(signal.stop_price, instrument.tick_size, signal.side);

        let stop_distance = (entry - stop).abs();
        if stop_distance <= Decimal::ZERO {
            return Decision::Refuse(Refusal::SizeTooSmall);
        }

        if !liquidation_is_safe(
            entry,
            stop,
            liq_price_estimate,
            self.params.liq_buffer_multiple,
        ) {
            return Decision::Refuse(Refusal::LiquidationTooClose {
                multiple: self.params.liq_buffer_multiple,
            });
        }

        let Some(qty) = position_size(
            state.equity,
            self.params.risk_pct,
            stop_distance,
            instrument.qty_step,
        ) else {
            return Decision::Refuse(Refusal::SizeTooSmall);
        };

        if !instrument.qty_is_valid(qty) {
            return Decision::Refuse(Refusal::BelowMinimumQty {
                notional: qty,
                minimum: instrument.min_order_qty,
            });
        }

        let notional = qty * entry;
        if notional > state.available {
            return Decision::Refuse(Refusal::InsufficientMargin {
                notional,
                available: state.available,
            });
        }

        // Target at the configured R multiple, measured from the ROUNDED entry
        // so the realized reward matches what was sized.
        let reward = stop_distance * reward_multiple_from(signal, stop_distance);
        let target = match signal.side {
            Side::Buy => entry + reward,
            Side::Sell => entry - reward,
        };

        let offset = signal.atr * self.stop_limit_offset_atr;
        let stop_limit_price = match signal.side {
            Side::Buy => stop - offset,
            Side::Sell => stop + offset,
        };

        Decision::Enter(OrderIntent {
            symbol: signal.symbol.clone(),
            side: signal.side,
            qty,
            entry_price: entry,
            stop_price: stop,
            stop_limit_price: round_stop_to_tick(stop_limit_price, instrument.tick_size, signal.side),
            target_price: round_stop_to_tick(target, instrument.tick_size, signal.side.opposite()),
            atr: signal.atr,
            signal_candle_open_ms: signal.signal_candle_open_ms,
        })
    }
}

/// Recover the strategy's intended R multiple from the signal's own prices, so
/// a strategy configured for something other than 2R is honoured rather than
/// silently overridden.
fn reward_multiple_from(signal: &Signal, _rounded_distance: Decimal) -> Decimal {
    signal.reward_multiple().unwrap_or(Decimal::from(2))
}

/// Round a protective price to a valid tick, conservatively for the side.
///
/// A long's stop rounds DOWN (further from entry, giving the trade more room);
/// a short's rounds UP. Rounding a stop toward entry would tighten it below
/// what was sized, so realized loss would exceed the budget.
fn round_stop_to_tick(price: Decimal, tick: Decimal, side: Side) -> Decimal {
    if tick.is_zero() {
        return price;
    }
    match side {
        Side::Buy => round_down_to_step(price, tick),
        Side::Sell => {
            let down = round_down_to_step(price, tick);
            if down == price { price } else { down + tick }
        }
    }
}
```

Update `crates/risk/src/lib.rs`:

```rust
pub mod limits;
pub mod manager;
pub mod sizing;

pub use limits::{AccountState, Refusal, check_entry_allowed, drawdown_breach};
pub use manager::{Decision, OrderIntent, RiskManager};
pub use sizing::{RiskParams, liquidation_is_safe, position_size};
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p risk --test manager_decisions`
Expected: PASS — 11 tests.

If `entry_and_stop_prices_are_rounded_to_the_instruments_tick` fails on the off-tick assertion, the bug is in `round_stop_to_tick`, not the test — fix the rounding rather than relaxing the assertion. Every price Bybit receives must land on a tick or the order is rejected outright.

- [ ] **Step 5: Run the whole risk suite and lint**

Run: `cargo test -p risk && cargo clippy -p risk -- -D warnings`
Expected: PASS — 31 tests, no clippy warnings.

- [ ] **Step 6: Verify the limit-only guard still passes with the new crates in scope**

Run: `cargo test -p exchange --test no_market_orders`
Expected: PASS — the source-grep walks `crates/`, which now includes `strategy` and `risk`.

- [ ] **Step 7: Commit**

```bash
git add crates/risk
git commit -m "feat(risk): RiskManager turning signals into sized order intents

Every hard limit is enforced here and the strategy cannot reach around it.
Checks run cheapest-first: persisted halt and counting limits before any
arithmetic, then drawdown, then sizing. Drawdown is measured directly rather
than waiting for a persisted flag, since the flag is written afterwards.
Prices are rounded to tick before sizing derives from them, and protective
prices round AWAY from entry so a rounded stop can never tighten below what
was sized."
```

---

## Plan Self-Review

**1. Spec coverage.** Mapping the approved spec to tasks:

| Spec requirement | Task |
|---|---|
| §3.3 `Strategy` trait, `Signal` carries no quantity | 1 |
| §6 bias filter, pullback, RSI trigger, volatility gate | 2 |
| §6 stop = further of swing / 1.5×ATR; target 2R | 2, 6 |
| §6 all thresholds are config, not constants | 2, 3 |
| §5 invariant 4 — size = risk ÷ stop distance, rounded down | 4 |
| §5 invariant 3 — 3× liquidation buffer, reject not resize | 4, 6 |
| §5 invariant 5 — reject below min qty / above available margin | 6 |
| §5 invariant 7 — max 5 filled entries per UTC day | 5 |
| §5 invariant 8 — max 4 concurrent, 1 per symbol | 5 |
| §5 invariant 9 — drawdown halts, persisted | 5 |
| §5 invariant 11 — Decimal everywhere | all |
| §4.3 stop-limit offset beyond the trigger | 6 |

**Deferred to Plan 1c (execution layer), by design:** §5 invariants 1, 2, 6, 10 (stop attached at entry, no market orders at the client, `orderLinkId` idempotency, feed staleness), §4.3 entry expiry and the stop-escalation ladder, §6 universe selection, §7.2 startup reconciliation, `OrderTracker`, `Executor`, `Reconciler`, `MockExchange` integration tests, and the live loop.

**2. Placeholder scan.** No TBDs. One deliberate placeholder file (`pullback.rs` in Task 1 Step 6) exists only so the crate compiles between tasks and is replaced wholesale in Task 2 — flagged explicitly at both ends.

**3. Type consistency.** `Signal` gained an `atr` field mid-plan; Task 1's struct, its five test constructions, and Task 2's construction site were all updated. `Side::opposite()` (Plan 1a) is used in Task 6 for target rounding. `round_down_to_step`'s non-negative `debug_assert!` is respected — every call passes a positive quantity or price. `Refusal` variants used in Task 6 (`LiquidationTooClose`, `SizeTooSmall`, `BelowMinimumQty`, `InsufficientMargin`) are all defined in Task 5.

**One known gap, stated rather than hidden:** Task 2's `a_signal_places_its_stop_below_entry_and_target_above_for_a_long` asserts only *if* the synthetic series happens to produce a signal. Constructing candle data that provably triggers a multi-condition setup is fragile, and a test rigged until it fires teaches nothing. The gates are proven negatively instead (no bias, no volatility, cold indicators, symbol isolation all correctly refuse), and Task 6 proves the sizing arithmetic directly from hand-built signals. End-to-end signal generation gets exercised for real in Plan 1c against `MockExchange`, and in Phase 2 against years of market data.

---

## Execution Handoff

Plan 1b covers 6 tasks producing two pure, heavily-tested crates: a strategy that emits price-only signals and a risk manager that is the sole authority on position size.

