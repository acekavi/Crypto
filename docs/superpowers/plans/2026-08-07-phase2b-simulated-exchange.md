# Phase 2b: SimulatedExchange and Replay Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replay historical candles through the *existing* live pipeline and produce a trade list with realistic fills, fees, and funding.

**Architecture:** A new `crates/backtest` supplies `SimulatedExchange`, which implements the existing `ExchangeClient` trait. `EngineLoop` already takes `Arc<dyn ExchangeClient>`, so the strategy, sizing, gating, and execution logic under test are **the same code that trades live** — not a reimplementation.

**Tech Stack:** Rust edition 2024, `rust_decimal::Decimal`, `tokio`, `async_trait`, `crates/history` for stored candles and funding rates.

## Why this plan is the risky one

A crash announces itself. **A subtly wrong fill model does not** — it produces confident, plausible, wrong numbers, and every later result inherits the error. The whole plan is ordered around that: the fill model and the cost model are built **first, as pure functions with no I/O**, and are pinned by oracle tests whose expected values are computed by hand and written into the plan. Nothing is asserted as "whatever the code produces".

## Global Constraints

- Money, prices, quantities, rates, fees: `rust_decimal::Decimal`. **Never `f64`.** `grep -rn "f64" crates/backtest/src/` must return nothing.
- The domain crate is `botcore`, never `core`.
- **No market orders.** No quoted `"Market"` literal anywhere. `cargo test -p exchange --test no_market_orders` must keep passing. `SimulatedExchange` has no market-order path — a limit that does not fill simply does not fill.
- **No wall-clock time in replay.** No `SystemTime::now()`, no `Instant::now()`, no `chrono::Utc::now()` in `crates/backtest/src/`. Simulated time only, or results are not reproducible.
- Decimals persist as TEXT, never SQLite REAL.
- `dec!` (`rust_decimal_macros`) in **test code only**.
- Rust edition 2024, rust-version 1.97.
- Comments explain **why**, not what.
- **Wrap every cargo command in `timeout 600`** (`timeout 900` for clippy and full-workspace runs).
- Per-crate formatting: `cargo fmt -p <crate>`, never `--all`.
- No AI-attribution trailers. Commit with:
  `git -c user.email=avishkakavinda@proton.me -c user.name=acekavi commit`
- **Commit after every task**, before writing any report.

## File Structure

| File | Responsibility |
|---|---|
| `crates/backtest/Cargo.toml` | New crate manifest |
| `crates/backtest/src/lib.rs` | Re-exports |
| `crates/backtest/src/fills.rs` | Trade-through fill model and pessimistic exit resolution — **pure, no I/O** |
| `crates/backtest/src/costs.rs` | Maker fee and funding accounting — **pure, no I/O** |
| `crates/backtest/src/sim_exchange.rs` | `SimulatedExchange`: `ExchangeClient` over historical data |
| `crates/backtest/src/replay.rs` | Chronological driver over `EngineLoop` |
| `crates/backtest/src/report.rs` | Closed-trade list and run summary |
| `Cargo.toml` | **Modify** — add `crates/backtest` to workspace members |

## Facts already established — do not re-derive

- `botcore::Candle { open_time_ms, open, high, low, close, volume, turnover }`
- `botcore::Instrument { symbol, tick_size, qty_step, min_order_qty, launch_time_ms }`
- `botcore::LimitEntry { symbol, side, qty, price, order_link_id, stop_loss, stop_limit_price, take_profit }` — carries the whole trade lifecycle, so the simulator needs no extra bookkeeping to know where the stop and target sit.
- `botcore::Position { symbol, side, size, entry_price, liq_price, unrealized_pnl }`
- `botcore::Balance { equity, available }`
- `botcore::Timeframe` is `H1 | H4`, with `duration_ms()` and `as_bybit_interval()`.
- `EngineLoop::on_candle_closed(&mut self, symbol, tf, candle) -> Result<CandleOutcome, ExchangeError>`
- `EngineLoop::warm(&mut self, symbol, tf, candles)`, `drive_stop_escalation(&mut self, now_ms)`, `on_account_event(&mut self, &AccountEvent)`, `load_baselines(&mut self, now_ms)`
- `EngineLoop::new(strategy, risk, client: Arc<dyn ExchangeClient>, journal: Arc<Journal>, instruments, config_hash, entry_expiry_candles, warmup_candles)`
- `crates/history`: `HistoryDb::candles_in_range`, `funding_in_range`, `rolling_turnover_24h`, `reconstruct_universe`.

---

### Task 1: The fill model — trade-through and pessimistic exit ordering

**Files:**
- Create: `crates/backtest/Cargo.toml`, `crates/backtest/src/lib.rs`, `crates/backtest/src/fills.rs`
- Modify: root `Cargo.toml` (workspace members)
- Test: `crates/backtest/tests/fills.rs`

**Interfaces:**
- Consumes: nothing (first task)
- Produces:
  - `pub enum FillOutcome { NoFill, Filled { price: Decimal } }`
  - `pub fn limit_fill(side: Side, limit_price: Decimal, candle: &Candle) -> FillOutcome`
  - `pub enum ExitOutcome { StillOpen, Stopped { price: Decimal }, TargetHit { price: Decimal } }`
  - `pub fn resolve_exit(position_side: Side, stop_limit: Decimal, target: Decimal, candle: &Candle) -> ExitOutcome`
  - `pub fn exit_was_ambiguous(position_side: Side, stop_limit: Decimal, target: Decimal, candle: &Candle) -> bool`

**The two rules this task encodes.** Get these wrong and every number downstream is wrong:

1. **Trade-through required.** A resting limit fills only if price traded *strictly* past it — never on touch. The live bot posts limit orders and sits behind the queue at its own price level; when price merely touches a level, the orders already there absorb the volume and a latecomer does not fill. **Touch-fill is the single most common way a backtest manufactures edge that does not exist.** Fill price is always the limit price, never better.

2. **Pessimistic intra-candle ordering.** OHLC gives no path within a candle. When both the stop and the target are reachable in the same candle, the true order is unknowable, so **always assume the stop filled first**. Any other choice inflates results, and the inflation is largest on exactly the volatile candles that dominate returns.

**Direction logic — the easiest thing to get backwards.** A long position closes by *selling*: its stop sits below and its target above. A short closes by *buying*: its stop sits above and its target below.

- [ ] **Step 1: Add the crate to the workspace**

Append `"crates/backtest"` to `members` in the root `Cargo.toml`.

- [ ] **Step 2: Write `crates/backtest/Cargo.toml`**

```toml
[package]
name = "backtest"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true

[dependencies]
botcore = { path = "../botcore" }
exchange = { path = "../exchange" }
history = { path = "../history" }
persistence = { path = "../persistence" }
risk = { path = "../risk" }
strategy = { path = "../strategy" }
engine = { path = "../engine" }
async-trait = "0.1"
rust_decimal.workspace = true
thiserror.workspace = true
tokio.workspace = true
tracing.workspace = true

[dev-dependencies]
rust_decimal_macros.workspace = true
tempfile = "3"
```

Check `async-trait`'s version against `crates/exchange/Cargo.toml` and match it.

- [ ] **Step 3: Write the failing tests**

`crates/backtest/tests/fills.rs`:

```rust
use backtest::{ExitOutcome, FillOutcome, exit_was_ambiguous, limit_fill, resolve_exit};
use botcore::{Candle, Side};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

fn candle(high: Decimal, low: Decimal) -> Candle {
    Candle {
        open_time_ms: 0,
        open: dec!(100),
        high,
        low,
        close: dec!(100),
        volume: dec!(1),
        turnover: dec!(1),
    }
}

#[test]
fn a_buy_limit_fills_only_when_price_trades_strictly_below_it() {
    // Traded through: low is under the limit.
    assert_eq!(
        limit_fill(Side::Buy, dec!(100), &candle(dec!(105), dec!(99))),
        FillOutcome::Filled { price: dec!(100) }
    );
}

#[test]
fn a_buy_limit_merely_touched_does_not_fill() {
    // THE central rule. Price reached exactly 100 and reversed. The orders
    // already queued at 100 absorbed the volume; a latecomer did not fill.
    // Treating this as a fill is how backtests invent edge.
    assert_eq!(
        limit_fill(Side::Buy, dec!(100), &candle(dec!(105), dec!(100))),
        FillOutcome::NoFill
    );
}

#[test]
fn a_sell_limit_fills_only_when_price_trades_strictly_above_it() {
    assert_eq!(
        limit_fill(Side::Sell, dec!(100), &candle(dec!(101), dec!(95))),
        FillOutcome::Filled { price: dec!(100) }
    );
    assert_eq!(
        limit_fill(Side::Sell, dec!(100), &candle(dec!(100), dec!(95))),
        FillOutcome::NoFill
    );
}

#[test]
fn a_fill_is_always_at_the_limit_price_never_better() {
    // Price traded far through the limit, but the order was resting AT the
    // limit — it does not fill at the extreme of the candle.
    assert_eq!(
        limit_fill(Side::Buy, dec!(100), &candle(dec!(105), dec!(80))),
        FillOutcome::Filled { price: dec!(100) }
    );
}

#[test]
fn a_long_position_stops_out_when_price_trades_below_the_stop_limit() {
    // Long closes by SELLING; its stop sits below.
    assert_eq!(
        resolve_exit(Side::Buy, dec!(98), dec!(104), &candle(dec!(100), dec!(97))),
        ExitOutcome::Stopped { price: dec!(98) }
    );
}

#[test]
fn a_long_position_takes_profit_when_price_trades_above_the_target() {
    assert_eq!(
        resolve_exit(Side::Buy, dec!(98), dec!(104), &candle(dec!(105), dec!(99))),
        ExitOutcome::TargetHit { price: dec!(104) }
    );
}

#[test]
fn a_candle_reaching_both_stop_and_target_resolves_as_stopped() {
    // THE PESSIMISTIC RULE. Both were reachable; the true order within the
    // candle is unknowable, so assume the worst. Resolving this as a target
    // hit would inflate results most on exactly the volatile candles that
    // dominate returns.
    assert_eq!(
        resolve_exit(Side::Buy, dec!(98), dec!(104), &candle(dec!(105), dec!(97))),
        ExitOutcome::Stopped { price: dec!(98) }
    );
    assert!(exit_was_ambiguous(Side::Buy, dec!(98), dec!(104), &candle(dec!(105), dec!(97))));
}

#[test]
fn a_short_position_stops_out_when_price_trades_above_the_stop_limit() {
    // Short closes by BUYING; its stop sits ABOVE and its target BELOW.
    // Getting this backwards silently inverts every short trade.
    assert_eq!(
        resolve_exit(Side::Sell, dec!(102), dec!(96), &candle(dec!(103), dec!(100))),
        ExitOutcome::Stopped { price: dec!(102) }
    );
}

#[test]
fn a_short_position_takes_profit_when_price_trades_below_the_target() {
    assert_eq!(
        resolve_exit(Side::Sell, dec!(102), dec!(96), &candle(dec!(101), dec!(95))),
        ExitOutcome::TargetHit { price: dec!(96) }
    );
}

#[test]
fn a_short_reaching_both_also_resolves_as_stopped() {
    assert_eq!(
        resolve_exit(Side::Sell, dec!(102), dec!(96), &candle(dec!(103), dec!(95))),
        ExitOutcome::Stopped { price: dec!(102) }
    );
}

#[test]
fn a_candle_touching_neither_leaves_the_position_open() {
    assert_eq!(
        resolve_exit(Side::Buy, dec!(98), dec!(104), &candle(dec!(103), dec!(99))),
        ExitOutcome::StillOpen
    );
    assert!(!exit_was_ambiguous(Side::Buy, dec!(98), dec!(104), &candle(dec!(103), dec!(99))));
}

#[test]
fn exits_also_require_trade_through_not_touch() {
    // A candle whose low is exactly the stop-limit did not trade through it.
    assert_eq!(
        resolve_exit(Side::Buy, dec!(98), dec!(104), &candle(dec!(103), dec!(98))),
        ExitOutcome::StillOpen
    );
}
```

- [ ] **Step 4: Run to verify failure**

`timeout 600 cargo test -p backtest` — expected: crate/items not found.

- [ ] **Step 5: Implement `fills.rs`**

`limit_fill`: `Side::Buy` fills when `candle.low < limit_price`; `Side::Sell` fills when `candle.high > limit_price`. Always `Filled { price: limit_price }`.

`resolve_exit`: check the **stop first**, then the target. For `Side::Buy` (long position) the stop is a sell below (`candle.low < stop_limit`) and the target a sell above (`candle.high > target`). For `Side::Sell` (short) the stop is a buy above (`candle.high > stop_limit`) and the target a buy below (`candle.low < target`).

`exit_was_ambiguous`: true when **both** the stop and the target were traded through in the same candle. This is reported so a run that depends heavily on the assumption says so.

- [ ] **Step 6: Write `lib.rs`**

```rust
pub mod fills;

pub use fills::{ExitOutcome, FillOutcome, exit_was_ambiguous, limit_fill, resolve_exit};
```

- [ ] **Step 7: Run tests to verify they pass**

`timeout 600 cargo test -p backtest`

- [ ] **Step 8: Commit**

```bash
cargo fmt -p backtest
git add Cargo.toml Cargo.lock crates/backtest
git -c user.email=avishkakavinda@proton.me -c user.name=acekavi commit -m "feat(backtest): trade-through fill model with pessimistic exit ordering"
```

---

### Task 2: The cost model — maker fees and funding

**Files:**
- Create: `crates/backtest/src/costs.rs`, `config/backtest.toml`
- Modify: `crates/backtest/src/lib.rs`
- Test: `crates/backtest/tests/costs.rs`

**Interfaces:**
- Consumes: nothing from Task 1
- Produces:
  - `pub struct CostModel { pub maker_fee_rate: Decimal }`
  - `pub fn maker_fee(&self, qty: Decimal, price: Decimal) -> Decimal`
  - `pub fn funding_charge(side: Side, qty: Decimal, mark_price: Decimal, rate: Decimal) -> Decimal` — **positive means the position PAYS**
  - `pub fn funding_timestamps_in(from_ms: i64, to_ms: i64, rates: &[FundingRate]) -> Vec<&FundingRate>` — timestamps strictly inside the holding period

**The sign rule.** A **positive** funding rate means **longs pay shorts**. A negative rate means **shorts pay longs**. Returning "positive = pays" for both sides keeps every call site from having to re-derive it — and a sign error here silently inverts the cost of every short in every backtest.

**Which notional?** Bybit charges funding on position value at the funding timestamp. Replay has no intra-candle mark, so use the **close of the candle containing the funding timestamp**. State that approximation in a comment; it is honest and stable, and the alternative (entry notional) drifts further from reality the longer a position is held.

- [ ] **Step 1: Determine the real maker fee rate — do not invent one**

Look up Bybit's current USDT-perpetual **maker** fee. Put it in `config/backtest.toml` with the date checked, e.g.:

```toml
# Bybit USDT-perpetual maker fee, standard (non-VIP) tier.
# Checked against Bybit's published fee schedule on <DATE>.
# A stale value here silently scales every backtest result.
maker_fee_rate = "0.0002"
```

Store it as a **string** and parse to `Decimal`, matching how the rest of the workspace avoids float config. If you cannot verify the current rate, **stop and report** rather than guessing — the plan's Global Constraints forbid inventing it.

- [ ] **Step 2: Write the failing tests**

`crates/backtest/tests/costs.rs`. **Every expected value below is hand-computed and written out** — do not replace any with whatever the code returns:

```rust
use backtest::{CostModel, funding_charge, funding_timestamps_in};
use botcore::Side;
use exchange::bybit::wire::FundingRate;
use botcore::Symbol;
use rust_decimal_macros::dec;

#[test]
fn a_maker_fee_is_the_rate_times_notional() {
    let m = CostModel { maker_fee_rate: dec!(0.0002) };
    // 50 units at 100 = 5000 notional; 5000 * 0.0002 = 1.0
    assert_eq!(m.maker_fee(dec!(50), dec!(100)), dec!(1.0));
    // 50 units at 104 = 5200 notional; 5200 * 0.0002 = 1.04
    assert_eq!(m.maker_fee(dec!(50), dec!(104)), dec!(1.04));
}

#[test]
fn a_long_pays_when_the_funding_rate_is_positive() {
    // 50 units at 100 = 5000 notional; 5000 * 0.0001 = 0.5 PAID.
    assert_eq!(funding_charge(Side::Buy, dec!(50), dec!(100), dec!(0.0001)), dec!(0.5));
}

#[test]
fn a_short_receives_when_the_funding_rate_is_positive() {
    // Same rate, opposite side: the short is PAID 0.5, so the charge is
    // negative. Getting this backwards inverts the cost of every short.
    assert_eq!(funding_charge(Side::Sell, dec!(50), dec!(100), dec!(0.0001)), dec!(-0.5));
}

#[test]
fn a_negative_rate_flips_both_sides() {
    // Negative rate: shorts pay longs.
    assert_eq!(funding_charge(Side::Buy, dec!(50), dec!(100), dec!(-0.0001)), dec!(-0.5));
    assert_eq!(funding_charge(Side::Sell, dec!(50), dec!(100), dec!(-0.0001)), dec!(0.5));
}

#[test]
fn only_funding_timestamps_strictly_inside_the_hold_are_charged() {
    let sym = Symbol::new("BTCUSDT");
    let r = |t: i64| FundingRate { symbol: sym.clone(), funding_time_ms: t, rate: dec!(0.0001) };
    let rates = vec![r(1000), r(2000), r(3000), r(4000)];

    // Held from 1000 to 3000. The boundaries are NOT charged: a position
    // opened exactly at a funding timestamp has not held through it, and one
    // closed exactly at one is already out.
    let charged = funding_timestamps_in(1000, 3000, &rates);
    let times: Vec<i64> = charged.iter().map(|f| f.funding_time_ms).collect();
    assert_eq!(times, vec![2000]);
}

#[test]
fn a_position_held_across_no_funding_timestamp_is_charged_nothing() {
    let sym = Symbol::new("BTCUSDT");
    let rates = vec![FundingRate { symbol: sym, funding_time_ms: 5000, rate: dec!(0.0001) }];
    assert!(funding_timestamps_in(1000, 3000, &rates).is_empty());
}

#[test]
fn a_full_round_trip_nets_out_to_the_hand_computed_figure() {
    // THE ORACLE. Long 50 units, entry 100, target 104, one funding period
    // held at 0.0001 with mark 100, maker fee 0.0002 both sides.
    //   gross pnl   = 50 * (104 - 100)        = 200
    //   entry fee   = 50 * 100   * 0.0002     =   1.00
    //   exit  fee   = 50 * 104   * 0.0002     =   1.04
    //   funding     = 50 * 100   * 0.0001     =   0.50  (long pays)
    //   net         = 200 - 1.00 - 1.04 - 0.50 = 197.46
    let m = CostModel { maker_fee_rate: dec!(0.0002) };
    let gross = dec!(50) * (dec!(104) - dec!(100));
    let entry_fee = m.maker_fee(dec!(50), dec!(100));
    let exit_fee = m.maker_fee(dec!(50), dec!(104));
    let funding = funding_charge(Side::Buy, dec!(50), dec!(100), dec!(0.0001));

    assert_eq!(gross - entry_fee - exit_fee - funding, dec!(197.46));
}
```

- [ ] **Step 3: Run to verify failure**

- [ ] **Step 4: Implement `costs.rs`**

`maker_fee` is `qty * price * maker_fee_rate`. `funding_charge` is `qty * mark_price * rate` for `Side::Buy` and its negation for `Side::Sell`. `funding_timestamps_in` filters `from_ms < t && t < to_ms` — **strictly** inside, both bounds excluded.

- [ ] **Step 5: Run tests to verify they pass**

- [ ] **Step 6: Commit**

```bash
cargo fmt -p backtest
git add crates/backtest config/backtest.toml
git -c user.email=avishkakavinda@proton.me -c user.name=acekavi commit -m "feat(backtest): maker fee and funding cost model"
```

---

### Task 3: `SimulatedExchange`

**Files:**
- Create: `crates/backtest/src/sim_exchange.rs`
- Modify: `crates/backtest/src/lib.rs`
- Test: `crates/backtest/tests/sim_exchange.rs`

**Interfaces:**
- Consumes: `limit_fill`, `resolve_exit`, `exit_was_ambiguous` (Task 1); `CostModel`, `funding_charge`, `funding_timestamps_in` (Task 2)
- Produces:
  - `pub struct SimulatedExchange { .. }` implementing `exchange::ExchangeClient`
  - `SimulatedExchange::new(starting_equity: Decimal, instruments: Vec<Instrument>, costs: CostModel) -> Self`
  - `SimulatedExchange::advance(&self, symbol: &Symbol, candle: &Candle, funding: &[FundingRate]) -> Vec<ClosedTrade>` — settle resting orders and open positions against one candle
  - `pub struct ClosedTrade { symbol, side, qty, entry_price, exit_price, entry_ms, exit_ms, gross_pnl, fees, funding, net_pnl, exit_reason: ExitReason, was_ambiguous: bool }`
  - `pub enum ExitReason { Stop, Target }`

**Interior mutability is required.** `ExchangeClient` methods take `&self`, and the simulator must record state through a shared reference — exactly as `engine::mock::MockExchange` does. **Read `crates/engine/src/mock.rs` first** and follow its `Mutex`-based pattern.

**`place_limit_entry` must not fill immediately.** It records a resting order. Fills only ever happen in `advance`, when a candle arrives. An order that fills on the same bar that created it is look-ahead bias.

**The daily-entry cap and position caps are NOT this crate's job.** `RiskManager` already enforces them and is being replayed. Do not reimplement them here — a second implementation would be free to disagree with the live one.

- [ ] **Step 1: Write the failing tests**

Cover at minimum:

1. `place_limit_entry` leaves the order resting: `positions()` is still empty and no trade is produced until a candle arrives.
2. A resting entry whose price is traded through becomes an open position at the limit price, and `balance()` reflects the entry fee.
3. **An entry only touched does not fill** — trade-through applies to entries, not just exits.
4. A long position whose candle trades through the stop closes as `ExitReason::Stop` at the stop-limit price.
5. A candle reaching both stop and target closes as `Stop` with `was_ambiguous == true`.
7. Equity after a closed trade equals starting equity plus `net_pnl`.
8. `cancel_order` removes a resting order so it can never fill afterwards.

And test 6 — **the oracle**, written out because it is the one that proves the pieces compose rather than merely each working alone. Adapt the constructor calls to your actual signatures, but **do not change any expected number**:

```rust
#[tokio::test]
async fn a_full_round_trip_reproduces_the_hand_computed_oracle() {
    // Same figures as crates/backtest/tests/costs.rs, but driven end to end
    // through place_limit_entry -> advance -> advance rather than by calling
    // the cost functions directly. If these disagree, the simulator and the
    // cost model have drifted apart and every backtest number is suspect.
    //   gross pnl = 50 * (104 - 100)      = 200
    //   entry fee = 50 * 100  * 0.0002    =   1.00
    //   exit  fee = 50 * 104  * 0.0002    =   1.04
    //   funding   = 50 * 100  * 0.0001    =   0.50  (long pays)
    //   net       = 200 - 1.00 - 1.04 - 0.50 = 197.46
    let sym = Symbol::new("BTCUSDT");
    let sim = SimulatedExchange::new(
        dec!(10000),
        vec![instrument(&sym)],
        CostModel { maker_fee_rate: dec!(0.0002) },
    );

    sim.place_limit_entry(LimitEntry {
        symbol: sym.clone(),
        side: Side::Buy,
        qty: dec!(50),
        price: dec!(100),
        order_link_id: "oracle-1".into(),
        stop_loss: dec!(98),
        stop_limit_price: dec!(98),
        take_profit: dec!(104),
    })
    .await
    .expect("placed");

    // Candle 1 trades through 100, filling the entry. It must NOT also
    // resolve an exit on the same bar.
    let entry_candle = candle_at(0, dec!(101), dec!(99));
    let closed = sim.advance(&sym, &entry_candle, &[]);
    assert!(closed.is_empty(), "an entry must not close on its own bar");
    assert_eq!(sim.positions().await.expect("positions").len(), 1);

    // Candle 2 trades through the target, with one funding timestamp
    // strictly inside the hold.
    let funding = vec![FundingRate {
        symbol: sym.clone(),
        funding_time_ms: 1,
        rate: dec!(0.0001),
    }];
    let exit_candle = candle_at(2, dec!(105), dec!(100.5));
    let closed = sim.advance(&sym, &exit_candle, &funding);

    assert_eq!(closed.len(), 1);
    let t = &closed[0];
    assert_eq!(t.exit_reason, ExitReason::Target);
    assert_eq!(t.gross_pnl, dec!(200));
    assert_eq!(t.fees, dec!(2.04), "1.00 entry + 1.04 exit");
    assert_eq!(t.funding, dec!(0.5), "positive: the long paid");
    assert_eq!(t.net_pnl, dec!(197.46));
    assert!(!t.was_ambiguous);

    let bal = sim.balance().await.expect("balance");
    assert_eq!(bal.equity, dec!(10197.46), "starting equity plus net pnl");
}
```

- [ ] **Step 2: Run to verify failure**

- [ ] **Step 3: Implement `sim_exchange.rs`**

State behind `Mutex`: `equity`, resting orders keyed by `order_link_id`, open positions keyed by symbol, and the closed-trade list.

`advance(symbol, candle, funding)`:
1. **Open positions first.** `resolve_exit` decides. On a close, compute gross PnL, the exit maker fee, and funding for every timestamp strictly inside the hold, then push a `ClosedTrade` and credit `net_pnl` to equity.
2. **Then resting entries.** `limit_fill` decides. On a fill, open a position at the limit price, charge the entry maker fee, and remember the stop-limit and target from the `LimitEntry`.

Settling exits **before** entries within one candle matters: an entry filled on this candle has not yet had a chance to be stopped out on the same candle, and letting it would be an intra-candle path assumption the data cannot support.

`ExchangeClient` methods: `balance` and `positions` read the simulated state; `instruments` returns what was configured; `klines` and `tickers` are unused by the replay driver — return empty and say why in a comment rather than `unimplemented!()`, which would panic a backtest on an incidental call.

- [ ] **Step 4: Run tests to verify they pass**

- [ ] **Step 5: Commit**

```bash
cargo fmt -p backtest
git add crates/backtest
git -c user.email=avishkakavinda@proton.me -c user.name=acekavi commit -m "feat(backtest): SimulatedExchange over historical candles"
```

---

### Task 4: The replay driver

**Files:**
- Create: `crates/backtest/src/replay.rs`
- Modify: `crates/backtest/src/lib.rs`
- Test: `crates/backtest/tests/replay.rs`

**Interfaces:**
- Consumes: `SimulatedExchange` (Task 3), `HistoryDb` (Plan 2a), `EngineLoop`
- Produces:
  - `pub struct BacktestConfig { pub start_ms: i64, pub end_ms: i64, pub starting_equity: Decimal, pub symbols: Vec<Symbol>, pub costs: CostModel, pub warmup_candles: usize }`
  - `pub async fn run_backtest(db: &HistoryDb, cfg: &BacktestConfig, strategy: Box<dyn Strategy>, risk: RiskManager) -> Result<BacktestResult, BacktestError>`
  - `pub enum BacktestError { History(String), Engine(String), GapInData { symbol: Symbol, from_ms: i64, to_ms: i64 } }` — define it in this task. `GapInData` is a named variant rather than a string so the caller can report exactly which hole stopped the run.
  - `pub struct BacktestResult { pub trades: Vec<ClosedTrade>, pub final_equity: Decimal, pub ambiguous_exits: usize, pub candles_replayed: usize }`

**Chronological ordering across symbols is mandatory.** Candles from all symbols must be merged and replayed in `open_time_ms` order. Replaying symbol-by-symbol would let the engine see BTC's whole future before ETH's first candle, and the position and daily-entry caps — which are global — would be enforced against a timeline that never existed.

**Ties must break deterministically.** When two symbols share an `open_time_ms`, order by symbol name. Without this, the result depends on map iteration order and two identical runs can disagree.

**Simulated time only.** The `now_ms` passed to `EngineLoop` is the current candle's close time. No wall clock anywhere.

- [ ] **Step 1: Write the failing tests**

1. **Determinism:** the same inputs run twice produce identical `BacktestResult` (equal trade lists and final equity). This is the test that protects every comparison the next plan makes.
2. **Chronological interleaving:** with two symbols whose candles interleave in time, the sequence the engine observes is globally time-ordered, not grouped by symbol.
3. **Warmup is not traded:** no trade opens before `warmup_candles` have been fed for a symbol.
4. **A strategy that never signals produces zero trades and zero costs** — the negative control from the spec. Final equity must be *exactly* the starting equity, not approximately.
5. **A gap in the stored data refuses rather than replaying across it.** Use `find_gaps` from Plan 2a; a hole means the engine would price a move that never happened continuously.

- [ ] **Step 2: Run to verify failure**

- [ ] **Step 3: Implement `replay.rs`**

Load candles per symbol and timeframe from `HistoryDb`, check gaps and refuse if any, warm the engine per symbol on every declared timeframe, then merge all candles into one chronological stream (ties by symbol name) and for each: call `sim.advance(...)` to settle fills, then `engine.on_candle_closed(...)`, then `engine.drive_stop_escalation(candle_close_ms)`.

Use an in-memory or temp-file `Journal` — `EngineLoop` requires one. It must not be the live trading journal; a backtest must never write to the real trade record.

- [ ] **Step 4: Run tests to verify they pass**

- [ ] **Step 5: Commit**

```bash
cargo fmt -p backtest
git add crates/backtest
git -c user.email=avishkakavinda@proton.me -c user.name=acekavi commit -m "feat(backtest): chronological replay driver over the live engine"
```

---

### Task 5: Reporting and the no-wall-clock guard

**Files:**
- Create: `crates/backtest/src/report.rs`, `crates/backtest/tests/no_wall_clock.rs`
- Modify: `crates/backtest/src/lib.rs`
- Test: `crates/backtest/tests/report.rs`

**Interfaces:**
- Consumes: `BacktestResult` (Task 4)
- Produces:
  - `pub struct RunSummary { pub trades: usize, pub wins: usize, pub losses: usize, pub gross_pnl: Decimal, pub total_fees: Decimal, pub total_funding: Decimal, pub net_pnl: Decimal, pub ambiguous_exits: usize }`
  - `pub fn summarise(result: &BacktestResult) -> RunSummary`

**Fees and funding are reported as separate line items, never netted silently.** A strategy that looks profitable while paying its entire edge away in costs must be visibly doing so.

The full metrics suite (expectancy, profit factor, drawdown, Sharpe) belongs to Plan 2c. This task produces only what is needed to eyeball a single run.

- [ ] **Step 1: Write the no-wall-clock guard test**

`crates/backtest/tests/no_wall_clock.rs` — a source-level test in the style of `crates/exchange/tests/no_market_orders.rs`. **Read that file first and follow its structure.** It must fail the build if any file under `crates/backtest/src/` contains `SystemTime::now`, `Instant::now`, or `Utc::now`.

Determinism is the property every comparison in Plan 2c rests on, and a single wall-clock call would break it silently — a compile-time guard is worth more than a comment.

- [ ] **Step 2: Write the failing report tests**

Assert on a hand-built `BacktestResult`: win/loss counts, that `gross_pnl - total_fees - total_funding == net_pnl` exactly, and that `ambiguous_exits` is carried through rather than dropped.

- [ ] **Step 3: Run to verify failure**

- [ ] **Step 4: Implement `report.rs`**

- [ ] **Step 5: Run tests to verify they pass**

- [ ] **Step 6: Commit**

```bash
cargo fmt -p backtest
git add crates/backtest
git -c user.email=avishkakavinda@proton.me -c user.name=acekavi commit -m "feat(backtest): run summary and wall-clock guard"
```

---

## Final verification

- [ ] `timeout 600 cargo build --workspace`
- [ ] `timeout 900 cargo test --workspace` — the 288 pre-existing tests must all still pass
- [ ] `timeout 900 cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `timeout 600 cargo test -p exchange --test no_market_orders`
- [ ] `timeout 600 cargo test -p backtest --test no_wall_clock`
- [ ] `grep -rn "f64" crates/backtest/src/` returns nothing
- [ ] The Task 2 oracle figure (`197.46`) is reproduced end-to-end by the Task 3 round-trip test — the cost model and the simulator agree
