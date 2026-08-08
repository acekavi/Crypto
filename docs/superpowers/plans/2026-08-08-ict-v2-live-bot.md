# ICT liquidity_sweep_v2 — Live Bot Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `liquidity_sweep_v2` (ICT, 1:5 R:R, breakeven stop at 2R) the strategy the live bot actually runs, with the breakeven rule implemented identically in the backtester and the live engine.

**Architecture:** The breakeven threshold moves out of `BacktestConfig` and onto `Signal`, so the strategy is the single source of truth and the live engine and simulator consume the same value. The live engine gains a breakeven pass that amends the stop to entry via the existing `ExchangeClient::amend_stop`, reusing the escalation ladder's amend path. `bot/src/main.rs` switches from `PullbackStrategy` to `IctStrategy`.

**Tech Stack:** Rust 2024, `rust_decimal::Decimal` (never `f64` for money), `async_trait`, tokio, libSQL/Turso, existing `MockExchange` test double.

## Global Constraints

- **Never `f64` for money.** All prices, quantities and PnL are `rust_decimal::Decimal`. A grep test enforces this.
- **Limit orders only, never market.** Enforced by `crates/exchange/tests/no_market_orders.rs`. The stop-escalation ladder must continue to exhaust without ever sending a market order.
- **Decimals persist as TEXT, never REAL.** Never order or compare a Decimal column lexicographically in SQL — `"9" > "10000"` as text. Sort in Rust.
- **`max_daily_entries` stays 5.** Owner's standing rule is 3–5 trades/day. Measured non-binding, so it costs nothing.
- **Mainnet stays double-gated:** the `mainnet` CLI arg AND `BYBIT_ALLOW_MAINNET`. Do not weaken, and do not add a third path.
- **Credentials from env only.** Never log `Credentials`, never log a Turso error's `Display` (it can embed `TURSO_DATABASE_URL`) — log the error kind only.
- Run `cargo fmt --all`, the full `cargo test --workspace`, and `cargo clippy --workspace --all-targets -- -D warnings` before every commit. Clippy must finish clean *before* committing, not after.

## The frozen configuration

```
structure_tf        H4          execution_tf       M15
require_mss         false       use_pdh_pdl        true
use_session_levels  false       use_order_block    true
ob_lookback         5           fvg_entry_fraction 0.50
stop_buffer_atr     0.00        stop_widen_multiple 1.0
session_filter      false       reward_multiple    5
breakeven_at_r      2           entry_expiry       12 execution candles
risk_pct            1%          max_concurrent     8
max_daily_entries   5           halts              -5% daily / -20% total
universe            the 8 validated symbols, fixed
```

Research-window result being reproduced: **n=298, win 24.2%, PF 1.457, maxDD 17.1%, net 18008** on `data/history.db`, 8 symbols, maker fee 0.0002, starting equity 10000.

**Provenance, stated plainly:** this configuration was selected by searching a 12-cell grid on the research window. The 330-day holdout was spent on `liquidity_sweep_v1` and cannot be reused. This config has **no out-of-sample validation**. It goes to testnet paper trading. It does not go to mainnet on the strength of this plan.

## File Structure

| File | Responsibility |
|---|---|
| `crates/strategy/src/ict.rs` | Add `breakeven_at_r` to `IctParams`; add `liquidity_sweep_v2()`; emit it on `Signal` |
| `crates/strategy/src/signal.rs` | `Signal` gains `breakeven_at_r: Option<Decimal>` |
| `crates/strategy/src/pullback.rs` | Set `breakeven_at_r: None` on emitted signals (compile fix, no behaviour change) |
| `crates/strategy/src/ict_params_from_config.rs` | **New.** Validated `IctParams` from config primitives |
| `crates/backtest/src/sim_exchange.rs` | Read breakeven from the position (sourced from the signal), not from `BacktestConfig` |
| `crates/backtest/src/replay.rs` | Drop `breakeven_at_r` plumbing from `BacktestConfig` |
| `bot/src/engine_loop.rs` | **New** `drive_breakeven_stops`; track `initial_risk` and `entry_price` per open position |
| `bot/src/config.rs` | `[strategy]` schema for ICT; `[universe] symbols` fixed list |
| `bot/src/main.rs` | Build `IctStrategy` instead of `PullbackStrategy` |
| `config/testnet.toml`, `config/mainnet.toml` | ICT parameters, risk envelope, fixed universe |
| `docs/strategies/ict-liquidity-sweep-v2.md` | **New.** The strategy, its numbers, and its unvalidated status |

---

### Task 1: Freeze `liquidity_sweep_v2` and put breakeven on the strategy

**Files:**
- Modify: `crates/strategy/src/ict.rs`
- Modify: `crates/strategy/src/signal.rs`
- Modify: `crates/strategy/src/pullback.rs`
- Test: `crates/strategy/src/ict.rs` (inline `#[cfg(test)]` module, matching the file's existing convention)

**Interfaces:**
- Consumes: existing `IctParams`, `Signal`.
- Produces: `IctParams::liquidity_sweep_v2() -> IctParams`; `IctParams.breakeven_at_r: Option<Decimal>`; `Signal.breakeven_at_r: Option<Decimal>`.

- [ ] **Step 1: Write the failing tripwire test**

In the `#[cfg(test)]` module of `crates/strategy/src/ict.rs`:

```rust
#[test]
fn liquidity_sweep_v2_is_frozen() {
    // Pins the configuration the live bot trades. A deliberate change means
    // updating this test AND re-running the research-window backtest; an
    // accidental one fails here.
    let p = IctParams::liquidity_sweep_v2();
    assert_eq!(p.reward_multiple, Decimal::from(5));
    assert_eq!(p.breakeven_at_r, Some(Decimal::TWO));
    assert_eq!(p.stop_widen_multiple, Decimal::ONE);
    assert_eq!(p.structure_tf, Timeframe::H4);
    assert_eq!(p.execution_tf, Timeframe::M15);
    assert!(!p.require_mss);
    assert!(p.use_pdh_pdl);
    assert!(!p.use_session_levels);
    assert!(p.use_order_block);
    assert_eq!(p.ob_lookback, 5);
    assert_eq!(p.stop_buffer_atr, Decimal::ZERO);
    assert!(!p.session_filter);
    assert!(p.allow_long);
    assert!(p.allow_short);
}

#[test]
fn v2_differs_from_v1_only_in_reward_and_breakeven() {
    let v1 = IctParams::liquidity_sweep_v1();
    let v2 = IctParams::liquidity_sweep_v2();
    let v1_relabelled = IctParams {
        reward_multiple: v2.reward_multiple,
        breakeven_at_r: v2.breakeven_at_r,
        ..v1
    };
    assert_eq!(v1_relabelled, v2);
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p strategy liquidity_sweep_v2 -- --nocapture`
Expected: FAIL — no function `liquidity_sweep_v2`, no field `breakeven_at_r`.

- [ ] **Step 3: Add the field and the constructor**

In `IctParams`, after `stop_widen_multiple`:

```rust
    /// Pull the stop to entry once the trade has travelled this many multiples
    /// of its initial risk in favour. `None` leaves the stop where it started.
    ///
    /// Carried on every `Signal` so the live engine and the simulator read one
    /// value. Holding it only in `BacktestConfig` is how the two drift.
    pub breakeven_at_r: Option<Decimal>,
```

Set `breakeven_at_r: None` in `variant_a()` (all other constructors inherit via `..Self::variant_a()`).

Add, next to `liquidity_sweep_v1`:

```rust
    /// The configuration the live bot trades.
    ///
    /// `liquidity_sweep_v1` with the target extended to 5R and a stop that
    /// moves to entry at 2R. On the research window this raised profit factor
    /// 1.291 -> 1.457 and lifted profitable quarters from 7/10 to 9/10, with
    /// the gain spread across symbols rather than concentrated.
    ///
    /// NOT VALIDATED OUT OF SAMPLE. It was chosen by searching a 12-cell grid
    /// on the research window, and the 330-day holdout was spent on v1. The
    /// research numbers are evidence about the research window and nothing
    /// else. Testnet paper trading is the only clean evidence available.
    pub fn liquidity_sweep_v2() -> Self {
        IctParams {
            reward_multiple: Decimal::from(5),
            breakeven_at_r: Some(Decimal::TWO),
            ..Self::liquidity_sweep_v1()
        }
    }
```

- [ ] **Step 4: Add `breakeven_at_r` to `Signal` and populate it**

In `crates/strategy/src/signal.rs`, add to `Signal`:

```rust
    /// Multiple of initial risk at which the stop moves to entry. `None`
    /// leaves the stop fixed for the life of the trade.
    pub breakeven_at_r: Option<Decimal>,
```

In `ict.rs` where the `Signal` is constructed, add `breakeven_at_r: p.breakeven_at_r,`.
In `pullback.rs` where its `Signal` is constructed, add `breakeven_at_r: None,`.

Fix every other `Signal { .. }` literal the compiler flags (tests included) by adding `breakeven_at_r: None`.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p strategy`
Expected: PASS, including both new tests.

- [ ] **Step 6: Commit**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
git add -A
git commit -m "feat(strategy): freeze liquidity_sweep_v2 and carry breakeven on the signal"
```

---

### Task 2: Make the simulator read breakeven from the signal

**Files:**
- Modify: `crates/backtest/src/sim_exchange.rs`
- Modify: `crates/backtest/src/replay.rs`
- Test: `crates/backtest/tests/breakeven_source.rs` (create)

**Interfaces:**
- Consumes: `Signal.breakeven_at_r` from Task 1.
- Produces: `SimPosition.breakeven_at_r: Option<Decimal>`; `BacktestConfig` no longer has `breakeven_at_r`.

**Why:** `BacktestConfig::breakeven_at_r` and the live engine would be two independent settings for one rule. This project has already shipped that class of bug — `warm()` fed the CandleStore but not the strategy's indicators, and the live bot was silent for five weeks after a restart. One source of truth, enforced by a test.

- [ ] **Step 1: Write the failing test**

Create `crates/backtest/tests/breakeven_source.rs`:

```rust
// A signal carrying no breakeven must not get one, and a signal carrying 2R
// must get exactly that — regardless of what any config says.
use backtest::sim_exchange::SimExchange;
use botcore::{Candle, Side, Symbol};
use rust_decimal_macros::dec;

#[test]
fn breakeven_threshold_comes_from_the_signal() {
    let mut sim = SimExchange::new(dec!(10000), dec!(0.0002));
    sim.open_for_test(
        Symbol::new("BTCUSDT"),
        Side::Buy,
        dec!(100),          // entry
        dec!(90),           // stop -> initial risk 10
        dec!(1),            // qty
        Some(dec!(2)),      // breakeven at 2R -> 120
    );
    // Travels to 119: one tick short of 2R. Stop must not move.
    sim.on_candle_for_test(&Symbol::new("BTCUSDT"), &candle(dec!(100), dec!(119), dec!(99)));
    assert_eq!(sim.stop_for_test(&Symbol::new("BTCUSDT")), dec!(90));

    // Travels to 120: exactly 2R. Stop moves to entry, not entry-plus-a-tick.
    sim.on_candle_for_test(&Symbol::new("BTCUSDT"), &candle(dec!(119), dec!(120), dec!(118)));
    assert_eq!(sim.stop_for_test(&Symbol::new("BTCUSDT")), dec!(100));
}

#[test]
fn a_signal_without_breakeven_never_moves_its_stop() {
    let mut sim = SimExchange::new(dec!(10000), dec!(0.0002));
    sim.open_for_test(
        Symbol::new("BTCUSDT"), Side::Buy,
        dec!(100), dec!(90), dec!(1),
        None,
    );
    sim.on_candle_for_test(&Symbol::new("BTCUSDT"), &candle(dec!(100), dec!(200), dec!(99)));
    assert_eq!(sim.stop_for_test(&Symbol::new("BTCUSDT")), dec!(90));
}

fn candle(open: rust_decimal::Decimal, high: rust_decimal::Decimal, low: rust_decimal::Decimal) -> Candle {
    Candle { open_time_ms: 0, open, high, low, close: open, volume: dec!(1), turnover: dec!(1) }
}
```

If `SimExchange` has no `open_for_test` / `on_candle_for_test` / `stop_for_test`, add them as `#[cfg(any(test, feature = "test-util"))]` helpers rather than making internals public. Match whatever convention the crate already uses for test seams; if none exists, `pub(crate)` plus an integration test moved to a unit test in the same file is acceptable.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p backtest --test breakeven_source`
Expected: FAIL to compile — the helpers and the per-position field do not exist.

- [ ] **Step 3: Move the threshold onto the position**

- Add `breakeven_at_r: Option<Decimal>` to the simulator's open-position struct.
- Populate it from the `Signal` when the entry fills.
- Change the breakeven block (currently reading `self.breakeven_at_r`) to read `pos.breakeven_at_r`.
- Delete the `breakeven_at_r` field from `SimExchange` and from `BacktestConfig`, and remove it from `replay.rs`'s construction.
- Keep the existing ordering exactly: the breakeven check runs **after** exit resolution for the candle, so a trade that already stopped out cannot be retroactively rescued.

- [ ] **Step 4: Fix the diagnostics that set `BacktestConfig::breakeven_at_r`**

`diag_sides.rs`, `diag_widen.rs`, `diag_rr_breakeven.rs`, `diag_rr5_breakdown.rs` and `diag_concurrency.rs` set that field. Change each to set `IctParams::breakeven_at_r` instead. These are `#[ignore]` diagnostics — they must compile, and they do not need to be run.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p backtest`
Expected: PASS.

- [ ] **Step 6: Reproduce the frozen result**

Add `crates/backtest/tests/diag_v2_baseline.rs`, an `#[ignore]` diagnostic that runs `liquidity_sweep_v2` over the full research window with `RiskParams { max_concurrent_positions: 8, max_daily_entries: 5, total_drawdown_halt_pct: dec!(0.20), ..defaults() }` and prints n / win% / PF / maxDD / net.

Run: `cargo test -p backtest --release --test diag_v2_baseline -- --ignored --nocapture`
Expected: **n=298, win 24.2%, PF 1.457, maxDD 17.1%, net 18008.**

If the numbers differ, STOP and report — it means moving the threshold changed behaviour, which it must not.

- [ ] **Step 7: Commit**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
git add -A
git commit -m "refactor(backtest): source the breakeven threshold from the signal"
```

---

### Task 3: Breakeven stops in the live engine

**Files:**
- Modify: `bot/src/engine_loop.rs`
- Test: `bot/src/engine_loop.rs` (inline `#[cfg(test)]`, matching the file's existing tests)

**Interfaces:**
- Consumes: `Signal.breakeven_at_r`; `ExchangeClient::amend_stop(&Symbol, trigger, limit_price)`.
- Produces: `EngineLoop::drive_breakeven_stops(&mut self) -> Result<(), ExchangeError>`.

**Why this is the load-bearing task:** without it the live bot trades 1:5 with no breakeven — the `1:5 none` row: PF 1.312 and **21.3% drawdown**, which breaches even the raised 20% halt. The bot would halt itself.

**Design notes for the implementer:**

- `RecordedStop` already tracks the stop the engine placed. Extend it with `entry_price`, `initial_risk` and `moved_to_breakeven: bool`, populated when the entry fill is observed.
- Detect the 2R trigger from the **ticker last price**, the same source `drive_stop_escalation` uses. Do not add a new price source.
- Long: `last_price - entry >= initial_risk * threshold`. Short: `entry - last_price >= initial_risk * threshold`.
- On trigger, call `amend_stop(symbol, entry_price, limit_price)` where `limit_price` is offset **beyond** the trigger by `stop_limit_offset_atr × atr`, matching how the initial stop is placed. Getting this backwards is the sizing bug fixed in `2d19d2d` — the stop-limit sits beyond the trigger, so that is where a stopped trade actually fills.
- Set `moved_to_breakeven = true` only after `amend_stop` returns `Ok`. A failed amend must retry next tick, never be silently skipped.
- One symbol failing must not stop the others — mirror `drive_stop_escalation`'s per-symbol error handling, and log at `warn`.
- A position already past its breakeven point when the engine restarts must still get its stop amended. Reconstruct from the recorded stop, not from an in-memory flag that a restart clears.
- **Never send a market order.** This is an amend of an existing stop-limit, nothing else.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn stop_moves_to_entry_once_price_reaches_the_breakeven_multiple() {
    let mock = MockExchange::new();
    mock.set_position("BTCUSDT", Side::Buy, dec!(1), dec!(100));
    mock.set_last_price("BTCUSDT", dec!(120)); // entry 100, risk 10 -> 2R
    let mut engine = engine_with_recorded_stop("BTCUSDT", dec!(100), dec!(90), Some(dec!(2)));

    engine.drive_breakeven_stops().await.expect("breakeven pass");

    let amends = mock.amend_stop_calls();
    assert_eq!(amends.len(), 1);
    assert_eq!(amends[0].trigger, dec!(100), "trigger must be entry exactly");
    assert!(amends[0].limit_price < dec!(100), "a long's stop-limit sits below its trigger");
}

#[tokio::test]
async fn stop_does_not_move_before_the_breakeven_multiple() {
    let mock = MockExchange::new();
    mock.set_position("BTCUSDT", Side::Buy, dec!(1), dec!(100));
    mock.set_last_price("BTCUSDT", dec!(119)); // one short of 2R
    let mut engine = engine_with_recorded_stop("BTCUSDT", dec!(100), dec!(90), Some(dec!(2)));

    engine.drive_breakeven_stops().await.expect("breakeven pass");

    assert!(mock.amend_stop_calls().is_empty());
}

#[tokio::test]
async fn a_short_moves_its_stop_down_to_entry() {
    let mock = MockExchange::new();
    mock.set_position("BTCUSDT", Side::Sell, dec!(1), dec!(100));
    mock.set_last_price("BTCUSDT", dec!(80)); // entry 100, risk 10 -> 2R in favour
    let mut engine = engine_with_recorded_stop("BTCUSDT", dec!(100), dec!(110), Some(dec!(2)));

    engine.drive_breakeven_stops().await.expect("breakeven pass");

    let amends = mock.amend_stop_calls();
    assert_eq!(amends.len(), 1);
    assert_eq!(amends[0].trigger, dec!(100));
    assert!(amends[0].limit_price > dec!(100), "a short's stop-limit sits above its trigger");
}

#[tokio::test]
async fn the_stop_is_amended_only_once() {
    let mock = MockExchange::new();
    mock.set_position("BTCUSDT", Side::Buy, dec!(1), dec!(100));
    mock.set_last_price("BTCUSDT", dec!(130));
    let mut engine = engine_with_recorded_stop("BTCUSDT", dec!(100), dec!(90), Some(dec!(2)));

    engine.drive_breakeven_stops().await.expect("first pass");
    engine.drive_breakeven_stops().await.expect("second pass");

    assert_eq!(mock.amend_stop_calls().len(), 1);
}

#[tokio::test]
async fn a_failed_amend_is_retried_on_the_next_tick() {
    let mock = MockExchange::new();
    mock.set_position("BTCUSDT", Side::Buy, dec!(1), dec!(100));
    mock.set_last_price("BTCUSDT", dec!(130));
    mock.fail_next_amend_stop();
    let mut engine = engine_with_recorded_stop("BTCUSDT", dec!(100), dec!(90), Some(dec!(2)));

    engine.drive_breakeven_stops().await.expect("a failed amend must not abort the pass");
    engine.drive_breakeven_stops().await.expect("second pass");

    assert_eq!(mock.amend_stop_calls().len(), 2, "the failure must be retried, not skipped");
}

#[tokio::test]
async fn a_position_with_no_breakeven_threshold_is_left_alone() {
    let mock = MockExchange::new();
    mock.set_position("BTCUSDT", Side::Buy, dec!(1), dec!(100));
    mock.set_last_price("BTCUSDT", dec!(500));
    let mut engine = engine_with_recorded_stop("BTCUSDT", dec!(100), dec!(90), None);

    engine.drive_breakeven_stops().await.expect("breakeven pass");

    assert!(mock.amend_stop_calls().is_empty());
}
```

Add `MockExchange::amend_stop_calls()`, `set_last_price` and `fail_next_amend_stop` if absent, following the double's existing recording convention. Write `engine_with_recorded_stop` as a local test helper.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p bot breakeven`
Expected: FAIL — `drive_breakeven_stops` does not exist.

- [ ] **Step 3: Implement `drive_breakeven_stops`**

Follow the design notes above. Keep it a separate pass from `drive_stop_escalation`; do not entangle the two ladders.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p bot`
Expected: PASS, all six.

- [ ] **Step 5: Call it from the engine tick**

Wire `drive_breakeven_stops` into the same tick that calls `drive_stop_escalation`, **before** escalation: a stop that has just moved to entry and triggered should escalate from its new trigger, not its old one.

Add a test asserting the tick invokes it:

```rust
#[tokio::test]
async fn the_engine_tick_runs_the_breakeven_pass() {
    // Guards against the class of bug where a pass is implemented, tested in
    // isolation, and never actually called — as happened with the escalation
    // ladder (299bf40) and the drawdown halt (3a725d5).
    let mock = MockExchange::new();
    mock.set_position("BTCUSDT", Side::Buy, dec!(1), dec!(100));
    mock.set_last_price("BTCUSDT", dec!(130));
    let mut engine = engine_with_recorded_stop("BTCUSDT", dec!(100), dec!(90), Some(dec!(2)));

    engine.tick(0).await.expect("tick");

    assert_eq!(mock.amend_stop_calls().len(), 1);
}
```

Use whatever the real per-iteration entry point is named; if the loop body is inline in `run`, extract the smallest testable seam rather than restructuring the loop.

- [ ] **Step 6: Commit**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
git add -A
git commit -m "feat(bot): move the stop to entry once a trade reaches its breakeven multiple"
```

---

### Task 4: Config schema and ICT parameter validation

**Files:**
- Create: `crates/strategy/src/ict_params_from_config.rs`
- Modify: `crates/strategy/src/lib.rs` (export it)
- Modify: `bot/src/config.rs`
- Test: `crates/strategy/src/ict_params_from_config.rs` (inline `#[cfg(test)]`)

**Interfaces:**
- Produces: `pub fn ict_params_from_config(...) -> Result<IctParams, ParamError>`.

Mirror `params_from_f64_config`: take primitives, not the config struct — `bot` depends on `strategy`, and accepting the struct would invert that.

Parameters to accept: `bias_ema`, `swing_lookback`, `atr_period`, `ob_lookback` (usize); `fvg_entry_fraction`, `stop_buffer_atr`, `stop_widen_multiple`, `reward_multiple` (f64); `breakeven_at_r` (`Option<f64>`); `use_pdh_pdl`, `use_session_levels`, `use_order_block`, `require_mss`, `session_filter`, `allow_long`, `allow_short` (bool). Timeframes stay hard-coded to H4/M15 — they are not safely config-driven and nothing asks for them to be.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn a_valid_config_produces_the_frozen_v2_parameters() {
    let p = ict_params_from_config(
        50, 5, 14, 5,
        0.5, 0.0, 1.0, 5.0, Some(2.0),
        true, false, true, false, false, true, true,
    ).expect("valid config");
    assert_eq!(p, IctParams::liquidity_sweep_v2());
}

#[test]
fn a_zero_period_is_rejected_rather_than_panicking_inside_an_indicator() {
    // Ema::new asserts period > 0; without this the failure is a panic deep
    // in indicator construction instead of a clear startup error.
    let e = ict_params_from_config(
        0, 5, 14, 5, 0.5, 0.0, 1.0, 5.0, Some(2.0),
        true, false, true, false, false, true, true,
    ).unwrap_err();
    assert_eq!(e, ParamError::ZeroPeriod("bias_ema"));
}

#[test]
fn a_non_finite_value_is_rejected() {
    let e = ict_params_from_config(
        50, 5, 14, 5, f64::NAN, 0.0, 1.0, 5.0, Some(2.0),
        true, false, true, false, false, true, true,
    ).unwrap_err();
    assert_eq!(e, ParamError::NotFinite("fvg_entry_fraction"));
}

#[test]
fn a_config_with_no_breakeven_yields_none_not_zero() {
    // Zero would mean "move the stop to entry immediately", which is a very
    // different strategy from "never move it".
    let p = ict_params_from_config(
        50, 5, 14, 5, 0.5, 0.0, 1.0, 5.0, None,
        true, false, true, false, false, true, true,
    ).expect("valid config");
    assert_eq!(p.breakeven_at_r, None);
}

#[test]
fn a_non_positive_reward_multiple_is_rejected() {
    let e = ict_params_from_config(
        50, 5, 14, 5, 0.5, 0.0, 1.0, 0.0, Some(2.0),
        true, false, true, false, false, true, true,
    ).unwrap_err();
    assert_eq!(e, ParamError::ZeroPeriod("reward_multiple"));
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p strategy ict_params`
Expected: FAIL — module does not exist.

- [ ] **Step 3: Implement it**

Reuse the existing `ParamError` and the `dec()` helper from `params_from_config.rs` rather than defining new ones. Add a variant to `ParamError` only if genuinely needed.

- [ ] **Step 4: Update the config schema**

In `bot/src/config.rs`, replace the pullback `[strategy]` fields with the ICT set above, keeping `entry_expiry_candles`, `stop_limit_offset_atr`, `max_stop_escalations` and `stop_fill_timeout_secs` — those are engine settings, not strategy ones.

Add to `[universe]` an optional explicit `symbols: Option<Vec<String>>`. When present it is used verbatim and turnover/age filters are skipped; the point is to pin the live universe to the 8 symbols the strategy was measured on.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test -p strategy && cargo test -p bot`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
git add -A
git commit -m "feat(strategy): validated ICT parameters from config"
```

---

### Task 5: Wire the bot to ICT and update the config files

**Files:**
- Modify: `bot/src/main.rs`
- Modify: `config/testnet.toml`
- Modify: `config/mainnet.toml`
- Test: `bot/tests/config_matches_frozen_strategy.rs` (create)

- [ ] **Step 1: Write the failing test**

```rust
// The config files and the frozen constructor must not drift. They already
// had: config/*.toml drove PullbackStrategy at 1:2 while the measured
// strategy was ICT at 1:3.
#[test]
fn testnet_config_reproduces_the_frozen_strategy() {
    let cfg = bot::config::load("config/testnet.toml").expect("testnet config parses");
    let p = strategy::ict_params_from_config(
        cfg.strategy.bias_ema, cfg.strategy.swing_lookback,
        cfg.strategy.atr_period, cfg.strategy.ob_lookback,
        cfg.strategy.fvg_entry_fraction, cfg.strategy.stop_buffer_atr,
        cfg.strategy.stop_widen_multiple, cfg.strategy.reward_multiple,
        cfg.strategy.breakeven_at_r,
        cfg.strategy.use_pdh_pdl, cfg.strategy.use_session_levels,
        cfg.strategy.use_order_block, cfg.strategy.require_mss,
        cfg.strategy.session_filter, cfg.strategy.allow_long, cfg.strategy.allow_short,
    ).expect("config produces valid params");
    assert_eq!(p, strategy::ict::IctParams::liquidity_sweep_v2());
}

#[test]
fn testnet_risk_envelope_matches_what_was_measured() {
    let cfg = bot::config::load("config/testnet.toml").expect("testnet config parses");
    assert_eq!(cfg.risk.risk_pct, 0.01);
    assert_eq!(cfg.risk.max_concurrent_positions, 8);
    assert_eq!(cfg.risk.max_daily_entries, 5, "owner's 3-5 trades/day rule");
    assert_eq!(cfg.risk.total_drawdown_halt_pct, 0.20);
}

#[test]
fn the_live_universe_is_pinned_to_the_measured_symbols() {
    // 8 concurrent positions across a 20-symbol universe was never measured,
    // and 20 correlated positions at 1% each is 20% at risk at once.
    let cfg = bot::config::load("config/testnet.toml").expect("testnet config parses");
    let mut symbols = cfg.universe.symbols.expect("universe must be pinned");
    symbols.sort();
    assert_eq!(symbols, vec![
        "ADAUSDT", "BNBUSDT", "BTCUSDT", "DOGEUSDT",
        "ETHUSDT", "LINKUSDT", "SOLUSDT", "XRPUSDT",
    ]);
}

#[test]
fn mainnet_config_reproduces_the_frozen_strategy_too() {
    let cfg = bot::config::load("config/mainnet.toml").expect("mainnet config parses");
    assert_eq!(cfg.risk.max_daily_entries, 5);
    assert_eq!(cfg.risk.total_drawdown_halt_pct, 0.20);
    assert!(cfg.universe.symbols.is_some(), "mainnet universe must be pinned too");
}
```

Adjust the module paths to whatever `bot` actually exports; if `config::load` is private, make the minimal change to expose it for testing.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p bot --test config_matches_frozen_strategy`
Expected: FAIL — config still carries pullback fields.

- [ ] **Step 3: Rewrite `config/testnet.toml`**

```toml
[risk]
risk_pct = 0.01
# One position per symbol across the pinned 8-symbol universe. Measured: the
# cap saturates at 6 and lifting it past 8 changes nothing.
max_concurrent_positions = 8
# Owner's standing rule, 3-5 trades/day. Measured non-binding: results are
# identical at 5, 8 and 100, so the rule costs nothing.
max_daily_entries = 5
daily_drawdown_halt_pct = 0.05
# Raised from 0.15 on the owner's instruction. The strategy's measured
# research-window drawdown is 17.1%, so a 15% halt would stop the bot on a
# path the backtest treats as normal.
total_drawdown_halt_pct = 0.20
liq_buffer_multiple = 3.0
leverage = 5

[strategy]
bias_ema = 50
swing_lookback = 5
atr_period = 14
ob_lookback = 5
fvg_entry_fraction = 0.5
stop_buffer_atr = 0.0
stop_widen_multiple = 1.0
reward_multiple = 5.0
breakeven_at_r = 2.0
use_pdh_pdl = true
use_session_levels = false
use_order_block = true
require_mss = false
session_filter = false
allow_long = true
allow_short = true

entry_expiry_candles = 12
stop_limit_offset_atr = 0.3
max_stop_escalations = 3
stop_fill_timeout_secs = 30

[universe]
# Pinned, not screened. These are the 8 symbols the strategy was measured on;
# trading anything else would be running an unmeasured strategy.
symbols = [
  "ADAUSDT", "BNBUSDT", "BTCUSDT", "DOGEUSDT",
  "ETHUSDT", "LINKUSDT", "SOLUSDT", "XRPUSDT",
]
```

Apply the same `[risk]`, `[strategy]` and `[universe]` blocks to `config/mainnet.toml`, preserving whatever mainnet-specific settings it already carries.

- [ ] **Step 4: Switch `main.rs` to `IctStrategy`**

Replace the `params_from_f64_config` / `PullbackStrategy` block at `bot/src/main.rs:140-157` with `ict_params_from_config` and `IctStrategy::new`. `timeframes()` and `warmup_candles()` are trait methods — read them from the ICT strategy exactly as the pullback code did, so H4/M15 subscriptions follow automatically.

Confirm `warm()` still feeds the strategy's indicators and not just the CandleStore — that was fixed in `4ce2f44` and must not regress with a different strategy whose warm-up needs differ.

- [ ] **Step 5: Run tests to verify they pass**

Run: `cargo test --workspace`
Expected: PASS.

- [ ] **Step 6: Verify against a live testnet connection**

```bash
BYBIT_API_KEY=... BYBIT_API_SECRET=... cargo run -p bot -- testnet --dry-run
```

Confirm from the logs: the 8 pinned symbols are subscribed, H4 and M15 klines warm up, no order is placed in dry-run, and no market order appears anywhere.

If `--dry-run` does not exist, run against testnet normally and confirm the first tick's decisions in the logs rather than adding a new flag.

- [ ] **Step 7: Commit**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
git add -A
git commit -m "feat(bot): run liquidity_sweep_v2 with a pinned universe and a 20% halt"
```

---

### Task 6: Document the strategy and its status

**Files:**
- Create: `docs/strategies/ict-liquidity-sweep-v2.md`
- Modify: `docs/strategies/ict-h4-sweep-m15-fvg.md`

- [ ] **Step 1: Write the v2 strategy document**

Cover: the frozen parameters; the research-window result (n=298, win 24.2%, PF 1.457, maxDD 17.1%, net 18008); the per-symbol and per-quarter breakdown (7/8 symbols profitable, 9/10 quarters profitable, best quarter 35% of net, net excluding best quarter 11180); the mechanism (breakeven at 2R converts reached-2R-then-reversed from −1R to 0R, and the 5R target collects moves previously capped at 3R); and the MFE evidence behind it.

State the provenance without softening it: selected from a 12-cell grid on the research window, holdout spent on v1, **no out-of-sample validation exists**. Cell noise is roughly ±20% — the 1:4-no-breakeven cell scoring below 1:3-no-breakeven shows it — so the 1:6 row is not meaningfully better than 1:5.

Record what would falsify it: testnet forward results materially below a 24% win rate over 100+ trades, or drawdown past 20%.

- [ ] **Step 2: Mark v1 superseded**

Add a note at the top of `ict-h4-sweep-m15-fvg.md`: still REJECTED on its holdout, superseded by v2, and the holdout it consumed is spent and cannot be reused for v2.

- [ ] **Step 3: Commit**

```bash
git add -A
git commit -m "docs: liquidity_sweep_v2, its evidence, and its unvalidated status"
```

---

## Out of scope

- Mainnet deployment. The double gate stays; nothing here authorises it.
- Re-running or reusing the 330-day holdout. It is spent.
- Time-series momentum. Tracked separately.
- The graphify knowledge base from the original hard rules — not built, and not resolved by this plan.

## Definition of done

- [ ] `cargo test --workspace` green, `cargo clippy --workspace --all-targets -- -D warnings` clean
- [ ] `liquidity_sweep_v2` reproduces n=298 / PF 1.457 / maxDD 17.1% / net 18008
- [ ] The live engine moves stops to entry at 2R, proven by tests, and the tick actually calls it
- [ ] `config/testnet.toml` provably equals `liquidity_sweep_v2`, enforced by a test
- [ ] The bot connects to testnet and warms up on the 8 pinned symbols
- [ ] `crates/exchange/tests/no_market_orders.rs` still passes
