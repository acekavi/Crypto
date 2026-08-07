# Phase 2c: Walk-Forward, Benchmark, and the Decision Gate

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Turn a single backtest run into a defensible **PASS or FAIL** verdict on whether a strategy has a real edge.

**Architecture:** Builds on Plan 2b's `run_backtest`. Adds a metrics suite, a rolling walk-forward harness, a seeded random-entry benchmark, and the pre-registered gate. All of it lives in `crates/backtest`.

**Tech Stack:** Rust edition 2024, `rust_decimal::Decimal`, `rand` (seeded, reproducible), `tokio`.

## The point of this plan

A backtester that reports numbers is easy. A backtester whose numbers you should **act on** is not. Everything here exists to defeat one failure mode: *convincing yourself a losing strategy works.*

Three defences, and they are the plan:

1. **Out-of-sample only.** Parameters are tuned on in-sample windows; only the following out-of-sample windows count as evidence.
2. **A random-entry benchmark.** Separates "my signal predicts price" from "1:2 R:R with a 1% cap makes money on any entry."
3. **Thresholds fixed in advance.** Already committed in the approved spec, reproduced below. They are not adjustable after seeing a result.

**A FAIL is a successful outcome for this phase.** It means the tooling stopped a losing strategy from reaching real money.

## Global Constraints

- `rust_decimal::Decimal` for money and all reported metrics. **Never `f64`** — `grep -rn "f64" crates/backtest/src/` must return nothing. (`rand` produces integers; convert without going through float.)
- **No wall-clock time** in `crates/backtest/src/`. `cargo test -p backtest --test no_wall_clock` must keep passing.
- **No market orders**; no quoted `"Market"` literal. `cargo test -p exchange --test no_market_orders` must keep passing.
- Domain crate is `botcore`, never `core`.
- `dec!` in **test code only**. Rust edition 2024, rust-version 1.97.
- **Every random draw is seeded and the seed is recorded in the output.** An unreproducible benchmark is not evidence.
- Comments explain **why**, not what.
- **Wrap every cargo command in `timeout 600`** (`timeout 900` for clippy/full-workspace).
- `cargo fmt -p <crate>`, never `--all`. No AI-attribution trailers:
  `git -c user.email=avishkakavinda@proton.me -c user.name=acekavi commit`
- **Commit after every task**, before writing any report.

## The pre-registered gate — fixed, not adjustable

From the approved spec (`docs/superpowers/specs/2026-08-07-phase2-backtester-design.md`). **All five must hold** on concatenated out-of-sample results:

| Criterion | Threshold |
|---|---|
| OOS expectancy | > 0, net of fees and funding |
| OOS trade count | ≥ 200 |
| Max drawdown | ≤ 15% |
| Profit factor | ≥ 1.3 |
| Random-entry benchmark | strategy beats the **95th percentile** |

Anything short of all five is a **FAIL**. No partial credit. **If a threshold looks wrong while implementing, STOP and report it — do not edit it.** Changing these numbers is the user's decision, made deliberately and visibly, never a quiet edit while looking at a disappointing result.

## File Structure

| File | Responsibility |
|---|---|
| `crates/backtest/src/metrics.rs` | Expectancy, profit factor, max drawdown, Sharpe, win rate — **pure, no I/O** |
| `crates/backtest/src/walk_forward.rs` | Rolling 6mo/2mo window harness |
| `crates/backtest/src/random_entry.rs` | Seeded random-entry `Strategy` and the benchmark distribution |
| `crates/backtest/src/gate.rs` | The five pre-registered criteria |
| `bot/src/bin/backtest.rs` | `backtest` binary |
| `crates/backtest/Cargo.toml` | **Modify** — add `rand` |

## Facts already established — do not re-derive

- `run_backtest(db, cfg, strategy, risk) -> Result<BacktestResult, BacktestError>`
- `BacktestConfig { start_ms, end_ms, starting_equity, symbols, instruments, costs, warmup_candles, entry_expiry_candles }` — derives `Clone`
- `BacktestResult { trades: Vec<ClosedTrade>, final_equity, ambiguous_exits, candles_replayed }`
- `ClosedTrade { symbol, side, qty, entry_price, exit_price, entry_ms, exit_ms, gross_pnl, fees, funding, net_pnl, exit_reason, was_ambiguous }`
- `strategy::Strategy` trait: `timeframes()`, `warmup_candles()`, `on_candle_close(&mut self, &MarketContext) -> Option<Signal>`
- `risk::{RiskManager, RiskParams}`; `RiskParams::defaults()` carries the owner's envelope
- `crates/history::HistoryDb`

---

### Task 1: The metrics suite

**Files:**
- Create: `crates/backtest/src/metrics.rs`
- Modify: `crates/backtest/src/lib.rs`
- Test: `crates/backtest/tests/metrics.rs`

**Interfaces:**
- Consumes: `ClosedTrade`
- Produces:
  - `pub struct Metrics { pub trade_count: usize, pub wins: usize, pub losses: usize, pub win_rate: Decimal, pub expectancy: Decimal, pub gross_profit: Decimal, pub gross_loss: Decimal, pub profit_factor: Option<Decimal>, pub max_drawdown_pct: Decimal, pub total_fees: Decimal, pub total_funding: Decimal, pub net_pnl: Decimal, pub ambiguous_exits: usize }`
  - `pub fn compute(trades: &[ClosedTrade], starting_equity: Decimal) -> Metrics`
  - `pub fn equity_curve(trades: &[ClosedTrade], starting_equity: Decimal) -> Vec<(i64, Decimal)>`

**Definitions — pin these exactly, they are easy to get subtly wrong:**

- **Expectancy** = `net_pnl.sum() / trade_count`, in quote currency, net of fees and funding. Zero trades → zero.
- **Gross profit** = sum of positive `net_pnl`. **Gross loss** = absolute sum of negative `net_pnl`.
- **Profit factor** = `gross_profit / gross_loss`. **`None` when `gross_loss` is zero** — not infinity, not a sentinel. A strategy with no losing trades in a sample has not proven a profit factor; it has proven the sample is too small. Returning `None` forces the gate to face that.
- **Max drawdown** = the largest peak-to-trough decline of the equity curve, as a **percentage of the running peak**, not of starting equity. A 15% drawdown after doubling is not the same risk as a 15% drawdown from the start, and the live halt measures against the peak too.
- **Equity curve** is ordered by `exit_ms`, since a trade affects equity when it closes.

- [ ] **Step 1: Write the failing tests**

```rust
use backtest::metrics::{compute, equity_curve};
use backtest::{ClosedTrade, ExitReason};
use botcore::{Side, Symbol};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// A closed trade carrying only what the metrics read. Prices are filler.
fn trade(exit_ms: i64, net_pnl: Decimal) -> ClosedTrade {
    ClosedTrade {
        symbol: Symbol::new("BTCUSDT"),
        side: Side::Buy,
        qty: dec!(1),
        entry_price: dec!(100),
        exit_price: dec!(100),
        entry_ms: exit_ms - 1,
        exit_ms,
        gross_pnl: net_pnl,
        fees: dec!(0),
        funding: dec!(0),
        net_pnl,
        exit_reason: if net_pnl >= Decimal::ZERO { ExitReason::Target } else { ExitReason::Stop },
        was_ambiguous: false,
    }
}

#[test]
fn expectancy_is_mean_net_pnl_per_trade() {
    // (+100 -50 +30) / 3 = 26.666...
    let m = compute(&[trade(1, dec!(100)), trade(2, dec!(-50)), trade(3, dec!(30))], dec!(10000));
    assert_eq!(m.trade_count, 3);
    assert_eq!(m.net_pnl, dec!(80));
    assert_eq!(m.expectancy, dec!(80) / dec!(3));
}

#[test]
fn profit_factor_is_gross_profit_over_gross_loss() {
    // wins 100 + 30 = 130; losses |−50| = 50; 130 / 50 = 2.6
    let m = compute(&[trade(1, dec!(100)), trade(2, dec!(-50)), trade(3, dec!(30))], dec!(10000));
    assert_eq!(m.gross_profit, dec!(130));
    assert_eq!(m.gross_loss, dec!(50));
    assert_eq!(m.profit_factor, Some(dec!(2.6)));
}

#[test]
fn profit_factor_is_none_when_nothing_lost() {
    // Not infinity and not a sentinel: a sample with no losers has not proven
    // a profit factor, it has proven the sample is too small. The gate must
    // confront that rather than see a flattering number.
    let m = compute(&[trade(1, dec!(100)), trade(2, dec!(30))], dec!(10000));
    assert_eq!(m.profit_factor, None);
}

#[test]
fn win_rate_counts_only_strictly_positive_trades() {
    // A scratch trade is not a win. Counting it as one inflates the rate on
    // exactly the strategies that scratch most.
    let m = compute(&[trade(1, dec!(100)), trade(2, dec!(0)), trade(3, dec!(-10))], dec!(10000));
    assert_eq!(m.wins, 1);
    assert_eq!(m.losses, 1);
    assert_eq!(m.trade_count, 3);
}

#[test]
fn max_drawdown_is_measured_from_the_running_peak_not_starting_equity() {
    // 10000 -> 20000 (peak) -> 17000. The decline is 3000 from a peak of
    // 20000 = 15%. Measured against starting equity it would look like a 30%
    // GAIN and no drawdown at all — the error that hides real risk after a
    // strategy has run up.
    let m = compute(&[trade(1, dec!(10000)), trade(2, dec!(-3000))], dec!(10000));
    assert_eq!(m.max_drawdown_pct, dec!(15));
}

#[test]
fn max_drawdown_takes_the_deepest_of_several_declines() {
    // +1000 (11000), -500 (10500), +2000 (12500 peak), -2500 (10000).
    // First decline: 500/11000 = 4.545%. Second: 2500/12500 = 20%.
    let m = compute(
        &[trade(1, dec!(1000)), trade(2, dec!(-500)), trade(3, dec!(2000)), trade(4, dec!(-2500))],
        dec!(10000),
    );
    assert_eq!(m.max_drawdown_pct, dec!(20));
}

#[test]
fn a_run_that_only_gains_has_no_drawdown() {
    let m = compute(&[trade(1, dec!(100)), trade(2, dec!(200))], dec!(10000));
    assert_eq!(m.max_drawdown_pct, dec!(0));
}

#[test]
fn no_trades_produces_zeroed_metrics_rather_than_a_divide_by_zero() {
    let m = compute(&[], dec!(10000));
    assert_eq!(m.trade_count, 0);
    assert_eq!(m.expectancy, dec!(0));
    assert_eq!(m.profit_factor, None);
    assert_eq!(m.max_drawdown_pct, dec!(0));
}

#[test]
fn the_equity_curve_is_ordered_by_exit_time_not_input_order() {
    // A trade affects equity when it CLOSES. Trades arriving out of order
    // (they can, across symbols) must not produce a curve that dips and
    // recovers in an order that never happened — that would invent drawdowns.
    let curve = equity_curve(&[trade(3, dec!(100)), trade(1, dec!(50))], dec!(10000));
    assert_eq!(curve, vec![(1, dec!(10050)), (3, dec!(10150))]);
}

#[test]
fn fees_and_funding_are_reported_separately_not_folded_into_pnl() {
    // A strategy paying its entire edge away in costs must be visibly doing
    // so, so these stay their own line items.
    let mut t = trade(1, dec!(10));
    t.gross_pnl = dec!(15);
    t.fees = dec!(3);
    t.funding = dec!(2);
    let m = compute(&[t], dec!(10000));
    assert_eq!(m.total_fees, dec!(3));
    assert_eq!(m.total_funding, dec!(2));
    assert_eq!(m.net_pnl, dec!(10));
}
```

- [ ] **Step 2: Run to verify failure** — `timeout 600 cargo test -p backtest --test metrics`
- [ ] **Step 3: Implement `metrics.rs`**
- [ ] **Step 4: Run tests to verify they pass**
- [ ] **Step 5: Commit**

```bash
cargo fmt -p backtest
git add crates/backtest
git -c user.email=avishkakavinda@proton.me -c user.name=acekavi commit -m "feat(backtest): metrics suite with peak-relative drawdown"
```

---

### Task 2: The walk-forward harness

**Files:**
- Create: `crates/backtest/src/walk_forward.rs`
- Modify: `crates/backtest/src/lib.rs`
- Test: `crates/backtest/tests/walk_forward.rs`

**Interfaces:**
- Produces:
  - `pub struct WalkForwardConfig { pub in_sample_ms: i64, pub out_of_sample_ms: i64 }` with `defaults()` = 6 months in-sample, 2 months out-of-sample (use 30-day months: `180` and `60` days in ms)
  - `pub struct Fold { pub index: usize, pub is_start_ms: i64, pub is_end_ms: i64, pub oos_start_ms: i64, pub oos_end_ms: i64 }`
  - `pub fn folds(start_ms: i64, end_ms: i64, wf: &WalkForwardConfig) -> Vec<Fold>`
  - `pub struct WalkForwardResult { pub folds: Vec<FoldResult>, pub oos_trades: Vec<ClosedTrade>, pub oos_metrics: Metrics }`
  - `pub struct FoldResult { pub fold: Fold, pub chosen_params: StrategyParams, pub is_metrics: Metrics, pub oos_metrics: Metrics }`
  - `pub async fn run_walk_forward(db, cfg: &BacktestConfig, wf: &WalkForwardConfig, grid: &[StrategyParams]) -> Result<WalkForwardResult, BacktestError>`

**The rules:**

- Windows **roll by the out-of-sample length**, so out-of-sample periods are contiguous and non-overlapping. Overlapping OOS windows would double-count the same trades and inflate the trade count the gate checks.
- A fold is emitted only if its **full** in-sample *and* out-of-sample windows fit inside `[start_ms, end_ms]`. A truncated final fold is a shorter, noisier sample masquerading as a full one.
- **Only concatenated out-of-sample results are evidence.** `oos_metrics` is computed over `oos_trades` — every fold's OOS trades concatenated. In-sample metrics are retained per fold for diagnosis and **must never feed the gate**.
- Tuning: for each fold, run every `StrategyParams` in `grid` over the in-sample window and pick the one with the highest **in-sample expectancy**. Ties break on the grid's declared order so the choice is deterministic. Then run **that one** over the out-of-sample window.
- `grid` is a small, explicitly declared list. **Do not add a search algorithm** — the spec puts optimisation beyond a declared grid out of scope, because that is how overfitting gets industrialised.

- [ ] **Step 1: Write the failing tests**

Cover with exact assertions:
1. `folds` over exactly 8 months with 6/2 produces **one** fold, covering IS `[0, 6mo)` and OOS `[6mo, 8mo)`.
2. Over 12 months it produces **3** folds (rolling by 2 months), and each fold's `oos_start_ms` equals the previous fold's `oos_end_ms` — contiguous, non-overlapping.
   *(Plan correction, 2026-08-07: this said 4. The first fold consumes 6+2=8 months and each further fold needs 2 more, so 12 months yields 3. Caught during execution — the implementation was right and the plan's arithmetic was wrong.)*
3. A range too short for one full fold produces **zero** folds, not a truncated one.
4. `run_walk_forward` concatenates OOS trades across folds and `oos_metrics.trade_count` equals `oos_trades.len()`.
5. **The chosen parameters come from in-sample only.** Build a grid where one entry is clearly best in-sample and a different one clearly best out-of-sample, and assert the in-sample winner is what got used. This is the test that proves the harness is not peeking.
6. Determinism: the same inputs produce identical `WalkForwardResult` twice.

- [ ] **Step 2: Run to verify failure**
- [ ] **Step 3: Implement `walk_forward.rs`**
- [ ] **Step 4: Run tests to verify they pass**
- [ ] **Step 5: Commit**

```bash
cargo fmt -p backtest
git add crates/backtest
git -c user.email=avishkakavinda@proton.me -c user.name=acekavi commit -m "feat(backtest): rolling walk-forward harness"
```

---

### Task 3: The random-entry benchmark

**Files:**
- Create: `crates/backtest/src/random_entry.rs`
- Modify: `crates/backtest/src/lib.rs`, `crates/backtest/Cargo.toml` (add `rand`)
- Test: `crates/backtest/tests/random_entry.rs`

**Interfaces:**
- Produces:
  - `pub struct RandomEntryStrategy { .. }` implementing `strategy::Strategy`, constructed as `RandomEntryStrategy::new(seed: u64, signal_probability_per_candle: Decimal, timeframes: Vec<Timeframe>, warmup: usize)`
  - `pub struct BenchmarkDistribution { pub seeds: Vec<u64>, pub expectancies: Vec<Decimal> }`
  - `pub fn percentile(sorted: &[Decimal], p: Decimal) -> Decimal`
  - `pub async fn run_benchmark(db, cfg, risk_params, seeds: &[u64], signal_probability: Decimal) -> Result<BenchmarkDistribution, BacktestError>`

**What this separates.** Two very different claims look identical in a single equity curve:
- "my entry signal predicts price" — a real edge, and
- "1:2 R:R with a 1% risk cap makes money on any entry" — risk management flattering noise.

The benchmark uses **identical** sizing, exits, universe, position caps, daily caps, fees and funding. **The only difference is when entries fire.** That is what isolates entry quality.

**Beating the median proves nothing** — with 100 random runs some do well by luck. The gate requires the **95th percentile**.

**Seeding:** use `rand::rngs::StdRng::seed_from_u64`. Every seed is recorded in `BenchmarkDistribution.seeds`. Same seed → same trades, always. An unreproducible benchmark is not evidence.

Sides must be drawn randomly too — a benchmark that only goes long measures a market-direction bias, not an entry edge.

- [ ] **Step 1: Add `rand` to `crates/backtest/Cargo.toml`**

Use a recent `rand` and pin it. **Do not draw floats** — draw an integer and compare against a `Decimal`-scaled threshold, so the constraint against `f64` holds.

- [ ] **Step 2: Write the failing tests**

1. **Same seed → identical trades.** Two runs with seed 42 produce the same trade list.
2. **Different seeds → different trades.** Otherwise the seed is not wired in and the whole distribution is one run repeated.
3. A probability of zero produces no signals; a probability of one signals every eligible candle. Pins the boundaries.
4. **Both sides are drawn.** Over a long run with many signals, both `Side::Buy` and `Side::Sell` appear.
5. `percentile` on a known sorted vector returns the hand-computed value; state and test the interpolation convention at the 95th percentile.
6. `run_benchmark` returns one expectancy per seed, in seed order.

- [ ] **Step 3: Run to verify failure**
- [ ] **Step 4: Implement `random_entry.rs`**
- [ ] **Step 5: Run tests to verify they pass**
- [ ] **Step 6: Commit**

```bash
cargo fmt -p backtest
git add crates/backtest Cargo.lock
git -c user.email=avishkakavinda@proton.me -c user.name=acekavi commit -m "feat(backtest): seeded random-entry benchmark"
```

---

### Task 4: The pre-registered gate

**Files:**
- Create: `crates/backtest/src/gate.rs`
- Modify: `crates/backtest/src/lib.rs`
- Test: `crates/backtest/tests/gate.rs`

**Interfaces:**
- Produces:
  - `pub struct GateThresholds { pub min_expectancy: Decimal, pub min_trades: usize, pub max_drawdown_pct: Decimal, pub min_profit_factor: Decimal, pub benchmark_percentile: Decimal }` with `pre_registered()` returning **exactly** the spec's values: `0`, `200`, `15`, `1.3`, `95`
  - `pub enum Criterion { Expectancy, TradeCount, MaxDrawdown, ProfitFactor, BenchmarkPercentile }`
  - `pub struct CriterionResult { pub criterion: Criterion, pub passed: bool, pub actual: String, pub required: String }`
  - `pub struct Verdict { pub passed: bool, pub criteria: Vec<CriterionResult> }`
  - `pub fn evaluate(oos: &Metrics, benchmark: &BenchmarkDistribution, thresholds: &GateThresholds) -> Verdict`

**Rules:**

- `Verdict.passed` is true **only if every criterion passed.** No partial credit, no weighting.
- **Every criterion is evaluated and reported even after one fails.** Short-circuiting would hide how far off the others were, which is the most useful thing in a FAIL.
- `profit_factor == None` (no losing trades) **fails** the profit-factor criterion, reported as "undefined — no losing trades". A sample with no losers has not demonstrated a profit factor.
- The benchmark criterion: the strategy's OOS expectancy must be **strictly greater** than the 95th percentile of the benchmark distribution.

- [ ] **Step 1: Write the failing tests**

1. All five satisfied → `passed == true` with five passing criteria.
2. **Each criterion failing alone flips the verdict to false** — five separate tests, each with the other four comfortably satisfied. This is what proves no criterion is decorative.
3. **A failing verdict still reports all five criteria**, not just the first failure.
4. `profit_factor: None` fails that criterion with an explanatory `actual`.
5. Expectancy exactly equal to the 95th percentile **fails** (strictly greater is required).
6. Trade count of exactly 200 **passes** (`>=`), 199 fails — the boundary is inclusive.
7. `pre_registered()` returns exactly `0`, `200`, `15`, `1.3`, `95`. **This test is a tripwire**: if someone edits a threshold, it fails loudly rather than silently changing what PASS means.

- [ ] **Step 2: Run to verify failure**
- [ ] **Step 3: Implement `gate.rs`**
- [ ] **Step 4: Run tests to verify they pass**
- [ ] **Step 5: Commit**

```bash
cargo fmt -p backtest
git add crates/backtest
git -c user.email=avishkakavinda@proton.me -c user.name=acekavi commit -m "feat(backtest): pre-registered pass/fail gate"
```

---

### Task 5: The `backtest` binary

**Files:**
- Create: `bot/src/bin/backtest.rs`
- Test: manual run, plus a smoke test if practical

**What it does:** loads `config/backtest.toml` and the profile config, opens `data/history.db`, runs the walk-forward over `PullbackStrategy` with a declared parameter grid, runs the random-entry benchmark, evaluates the gate, and prints a report.

**The report must show, in this order:**
1. The **verdict** — PASS or FAIL — first, before any supporting numbers.
2. Each of the five criteria: actual vs required, pass/fail.
3. OOS metrics with **fees and funding as separate line items**.
4. Per-fold OOS summary, so a result driven by one lucky fold is visible.
5. `ambiguous_exits` as a count **and as a percentage of trades** — if a large fraction of trades hit the intra-candle ambiguity rule, the result rests on an assumption rather than on data, and the report must say so.
6. The benchmark distribution: median, 95th percentile, and the strategy's own expectancy against them.
7. The seeds used, the config hash, and the history range — so the run is reproducible.

**Print the honest limitations from the spec** at the end of any PASS: delisted symbols are absent (survivorship bias), the intra-candle path is assumed, escalation fidelity is limited, and **a passing backtest is evidence, not proof**.

- [ ] **Step 1: Write the binary**
- [ ] **Step 2: Run it against the downloaded history**

```bash
cargo run --bin backtest 2>&1 | tail -60
```

Report the **real verdict and the real numbers**, whatever they are. A FAIL is a successful outcome — do not tune anything to chase a PASS, and do not adjust a threshold. If the run reveals a bug, fix the bug and say so.

- [ ] **Step 3: Commit**

```bash
cargo fmt -p bot
git add bot
git -c user.email=avishkakavinda@proton.me -c user.name=acekavi commit -m "feat(bot): backtest binary reporting the pre-registered verdict"
```

---

## Final verification

- [ ] `timeout 600 cargo build --workspace`
- [ ] `timeout 900 cargo test --workspace` — all pre-existing tests still pass
- [ ] `timeout 900 cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `timeout 600 cargo test -p backtest --test no_wall_clock`
- [ ] `timeout 600 cargo test -p exchange --test no_market_orders`
- [ ] `grep -rn "f64" crates/backtest/src/` returns nothing
- [ ] `pre_registered()` still returns `0`, `200`, `15`, `1.3`, `95` — unchanged from the approved spec
