# Pairs Runtime Rust Port — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the three Python `pairs_bot.py` processes with one Rust binary that trades the same strategy at exact arithmetic parity, cannot orphan a leg, and stores state durably.

**Architecture:** A new pure `crates/pairs` crate (spread statistics, signal rules, sizing, two-leg executor, supervisor) built on the existing workspace — `crates/exchange` for signed/rate-limited/retried Bybit V5, `crates/persistence` for the durable journal, `crates/backtest` for validation. Leg placement is added to the existing `ExchangeClient` trait so the backtester and the live bot drive one execution path. One process runs all pairs as tokio tasks over a shared REST client.

**Tech Stack:** Rust 2024 (rust-version 1.97), tokio, `rust_decimal`, `turso` (journal), `reqwest`+`wiremock` (exchange + its tests), `toml` (config), `tracing`.

**Spec:** `docs/superpowers/specs/2026-09-05-pairs-runtime-rust-design.md`

## Global Constraints

Every task's requirements implicitly include this section.

- **Limit orders only — no market orders anywhere, including unwinds.** An orphaned leg is flattened with an aggressive reduce-only *limit* on an escalating price ladder. If the ladder is exhausted, halt; never fall back to a market order.
- Leg orders are `timeInForce: "GTC"`, `orderType: "Limit"`, `category: "linear"`, `positionIdx: 0`. Never `PostOnly` (rejected at a crossing price), never `IOC` (a partial fill that cancels the remainder leaves the pair mismatched).
- `rust_decimal::Decimal` for all money, quantity, and **rates** (fees, risk fractions). `f64` only inside spread/z-score statistics, matching the Python it must be bit-comparable to. A rate knob is not a statistic: `fee_per_leg` and `risk_pct_of_equity` are both `Decimal`, and no code path may convert either through `f64`.
- Rolling statistics use **population** variance (`/ window`, not `/ (window - 1)`) with a `max(var, 1e-12)` floor, over a window that **excludes the current bar**.
- Exit-reason precedence stays `breakeven → time → target → stop`. Do not "fix" it: every exit resolves on the same bar at the same price, so the order changes only the label.
- Edition/rust-version come from `[workspace.package]`: `edition.workspace = true`, `rust-version.workspace = true`. Shared deps use `.workspace = true`.
- Every new public item carries a doc comment saying *why*, matching the surrounding code. Tests live in `#[cfg(test)] mod tests` for unit tests and `crates/<name>/tests/` for integration tests.
- Verification command for every task: `cargo test -p <crate>` then `cargo clippy --workspace --all-targets -- -D warnings`.
- Nothing in this plan touches `scripts/render_pairs_dashboard.py` until Task 11. The Python bots keep running untouched until Task 12.

## File Structure

**New crate `crates/pairs`** — one responsibility per file, no file over ~350 lines:

| File | Responsibility |
|---|---|
| `src/lib.rs` | Module declarations and re-exports only |
| `src/spread.rs` | `RollingZ` — O(1) rolling mean/sd/z. No I/O, no domain types |
| `src/signal.rs` | `PairParams`, `PairSide`, `ExitReason`, `SignalEngine` |
| `src/sizing.rs` | `Sizing`, `CapReason`, `per_leg_notional` |
| `src/settle.rs` | `LegOutcome`, `Settlement`, `settle` — pure, no async |
| `src/executor.rs` | `open_pair`, `close_pair`, `unwind_leg`. Async, trait-driven |
| `src/state.rs` | `PairPosition`, `PairEvent` — the journalled shapes |
| `src/supervisor.rs` | `PortfolioGuard` + the per-pair loop |
| `tests/support/mod.rs` | `FaultExchange` — a scriptable `ExchangeClient` |

**Modified:**

| File | Change |
|---|---|
| `crates/botcore/src/order.rs` | Add `LimitLeg` |
| `crates/botcore/src/lib.rs` | Re-export `LimitLeg` |
| `crates/exchange/src/traits.rs` | Add `place_limit_leg`, `order_by_link_id`, `ticker` to `ExchangeClient` |
| `crates/exchange/src/bybit/wire.rs` | Add `bid1`/`ask1` to `Ticker` — it has no top of book today |
| `crates/exchange/src/bybit/rest.rs` | Implement both |
| `crates/exchange/src/bybit/wire.rs` | Add `OrderStatusRow` |
| `crates/engine/src/mock.rs` | Implement the two new trait methods |
| `crates/backtest/src/sim_exchange.rs` | Implement the two new trait methods |
| `crates/persistence/src/journal.rs` | Add `pair_positions` + `pair_events` tables and accessors |
| `bot/Cargo.toml` | Add the `pairs` bin and the `pairs` dep |
| `bot/src/pairs_config.rs` | New — `PairsConfig` loading |
| `bot/src/bin/pairs.rs` | New — the binary |
| `config/pairs-testnet.toml` | New |
| `config/pairs-mainnet.toml` | New |

---
### Task 1: `crates/pairs` — O(1) rolling spread statistics

**Files:**
- Create: `crates/pairs/Cargo.toml`
- Create: `crates/pairs/src/lib.rs`
- Create: `crates/pairs/src/spread.rs`
- Modify: `Cargo.toml:2` (workspace `members`)

**Interfaces:**
- Consumes: nothing.
- Produces: `pairs::spread::{RollingZ, Stats, log_spread}`.
  - `RollingZ::new(window: usize) -> RollingZ`
  - `RollingZ::push(&mut self, value: f64) -> Option<Stats>`
  - `RollingZ::window(&self) -> usize`
  - `Stats { mean: f64, sd: f64, z: f64 }` (all `pub`, `Copy`)
  - `log_spread(a: f64, b: f64) -> f64`

**Context the implementer needs:** this replaces `rolling_mean_stddev` and `rolling_zscores` in [`scripts/pairs_bot.py:296-318`](../../../scripts/pairs_bot.py). The Python is O(n·window) — it re-slices and re-sums the whole window every bar, costing 12.5 s of CPU per dashboard refresh over ~17k bars. The semantics that must be preserved exactly: `z[i]` scores `values[i]` against the mean and sd of `values[i-window .. i]`, i.e. the window **excludes** the value being scored. That exclusion is what keeps the strategy free of lookahead; do not "simplify" it to include the current bar.

- [ ] **Step 1: Create the crate skeleton and register it in the workspace**

`crates/pairs/Cargo.toml`:

```toml
[package]
name = "pairs"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true

[dependencies]
botcore = { path = "../botcore" }
rust_decimal.workspace = true
serde.workspace = true
thiserror.workspace = true

[dev-dependencies]
rust_decimal_macros.workspace = true
```

`crates/pairs/src/lib.rs`:

```rust
pub mod spread;

pub use spread::{RollingZ, Stats, log_spread};
```

In the root `Cargo.toml`, add `"crates/pairs"` to `members`:

```toml
members = ["crates/botcore", "crates/indicators", "crates/exchange", "crates/persistence", "crates/strategy", "crates/risk", "crates/engine", "bot", "crates/history", "crates/backtest", "crates/pairs"]
```

- [ ] **Step 2: Write the failing tests**

`crates/pairs/src/spread.rs` — tests first, with the module body empty:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// The exact series the Python reference was evaluated on, so the golden
    /// numbers below are comparable rather than merely plausible. Generated by
    /// `math.sin(i/7)*0.03 + math.cos(i/13)*0.02 + 0.5`.
    fn golden_series() -> Vec<f64> {
        (0..40)
            .map(|i| {
                let i = i as f64;
                (i / 7.0).sin() * 0.03 + (i / 13.0).cos() * 0.02 + 0.5
            })
            .collect()
    }

    #[test]
    fn no_score_is_produced_until_a_full_window_precedes_the_value() {
        let mut r = RollingZ::new(8);
        for v in golden_series().iter().take(8) {
            assert_eq!(r.push(*v), None);
        }
        assert!(r.push(golden_series()[8]).is_some());
    }

    #[test]
    fn scores_match_the_python_reference_to_within_one_part_in_a_billion() {
        // Captured from scripts/pairs_bot.py rolling_zscores(values, 8).
        let expected = [
            1.473716198484,
            1.322946909167,
            1.106905206937,
            0.759560999308,
            0.102115965891,
            -1.394626742276,
        ];
        let mut r = RollingZ::new(8);
        let mut got = Vec::new();
        for v in golden_series() {
            if let Some(s) = r.push(v) {
                got.push(s.z);
            }
        }
        for (i, want) in expected.iter().enumerate() {
            assert!(
                (got[i] - want).abs() < 1e-9,
                "z[{}]: got {}, want {}",
                i + 8,
                got[i],
                want
            );
        }
    }

    #[test]
    fn mean_and_sd_match_the_python_reference() {
        // rolling_mean_stddev(values, 8) at i == 8.
        let mut r = RollingZ::new(8);
        let series = golden_series();
        let mut first = None;
        for v in series {
            if let Some(s) = r.push(v) {
                first = Some(s);
                break;
            }
        }
        let s = first.expect("a window's worth of values was pushed");
        assert!((s.mean - 0.532605712195).abs() < 1e-9, "mean was {}", s.mean);
        assert!((s.sd - 0.007477698075).abs() < 1e-9, "sd was {}", s.sd);
    }

    #[test]
    fn the_incremental_accumulator_never_drifts_from_an_exact_recomputation() {
        // The whole point of the O(1) form is that it stays exact. A long run
        // over a large-mean, small-variance series is where sum-of-squares
        // cancellation would show up if the offset trick were dropped.
        let window = 240;
        let series: Vec<f64> = (0..5000)
            .map(|i| {
                let i = i as f64;
                -2.302_585_092_994_046 + (i / 31.0).sin() * 0.004
            })
            .collect();
        let mut r = RollingZ::new(window);
        for (i, v) in series.iter().enumerate() {
            let got = r.push(*v);
            if i < window {
                continue;
            }
            let hist = &series[i - window..i];
            let mean = hist.iter().sum::<f64>() / window as f64;
            let var = hist.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / window as f64;
            let sd = var.max(1e-12).sqrt();
            let want_z = (v - mean) / sd;
            let got = got.expect("window is full");
            assert!(
                (got.z - want_z).abs() < 1e-9,
                "drift at i={i}: got {}, want {want_z}",
                got.z
            );
        }
    }

    #[test]
    fn a_flat_window_uses_the_variance_floor_instead_of_dividing_by_zero() {
        // Python clamps with max(var, 1e-12) before the sqrt; a constant
        // series would otherwise produce sd == 0 and an infinite z.
        let mut r = RollingZ::new(4);
        for _ in 0..4 {
            r.push(1.0);
        }
        let s = r.push(1.000_001).expect("window is full");
        assert!(s.z.is_finite(), "z was {}", s.z);
        assert!((s.sd - 1e-6).abs() < 1e-9, "sd was {}", s.sd);
    }

    #[test]
    fn log_spread_is_the_difference_of_natural_logs() {
        // Captured from scripts/pairs_bot.py spread_series.
        assert!((log_spread(10.0, 2.0) - 1.609437912434).abs() < 1e-9);
        assert!((log_spread(11.0, 2.5) - 1.481604540924).abs() < 1e-9);
        assert!((log_spread(12.0, 2.4) - 1.609437912434).abs() < 1e-9);
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p pairs`
Expected: FAIL — `cannot find type RollingZ in this scope`.

- [ ] **Step 4: Write the implementation**

Above the test module in `crates/pairs/src/spread.rs`:

```rust
use std::collections::VecDeque;

/// Floor applied to the variance before the square root.
///
/// Mirrors `max(var, 1e-12)` in the Python. Without it a window of identical
/// prices gives `sd == 0` and every z becomes infinite — which on a pair whose
/// legs are briefly quoted flat would fire an entry signal on noise.
const VAR_FLOOR: f64 = 1e-12;

/// Rolling mean, standard deviation and z-score over a fixed window that
/// **excludes** the value being scored.
///
/// The exclusion is the whole design: scoring a value against a window that
/// contains it leaks the present into the statistic and makes every backtest
/// optimistic. `push` therefore computes against what came before, then folds
/// the value in.
///
/// O(1) per push. The Python it replaces re-summed the entire window every bar
/// (O(n·window)), which is 12.5 s of CPU per dashboard refresh at this
/// project's data sizes.
#[derive(Debug, Clone)]
pub struct RollingZ {
    window: usize,
    buf: VecDeque<f64>,
    /// Values are accumulated as `x - offset` rather than as `x`.
    ///
    /// The variance is computed as `E[x²] - E[x]²`, and log-spreads sit far
    /// from zero (AAVE/ETH runs near -2.3) while their standard deviation is
    /// around 0.02. Subtracting two numbers near 5.29 to recover 4e-4 throws
    /// away most of the available precision. Centring first keeps both terms
    /// small, so the incremental form stays as accurate as a full recompute.
    offset: f64,
    sum: f64,
    sumsq: f64,
    /// Pushes since the accumulators were last rebuilt exactly from `buf`.
    /// Bounds accumulated rounding drift to one window's worth of updates at
    /// a cost that amortises to O(1).
    since_rebuild: usize,
}

/// The statistics of one window, plus the score of the value that was pushed
/// against them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stats {
    pub mean: f64,
    pub sd: f64,
    pub z: f64,
}

impl RollingZ {
    /// # Panics
    /// If `window` is zero. A zero window is a programming error, not a
    /// runtime condition — the Python raised `ValueError` from the same place.
    pub fn new(window: usize) -> Self {
        assert!(window > 0, "rolling window must be positive");
        Self {
            window,
            buf: VecDeque::with_capacity(window + 1),
            offset: 0.0,
            sum: 0.0,
            sumsq: 0.0,
            since_rebuild: 0,
        }
    }

    pub fn window(&self) -> usize {
        self.window
    }

    /// Whether a full window has accumulated, so the next push will score.
    pub fn is_warm(&self) -> bool {
        self.buf.len() >= self.window
    }

    /// Score `value` against the preceding window, then admit it to the window.
    ///
    /// Returns `None` until `window` values have been seen — the caller must
    /// treat that as "no opinion", never as a zero score.
    pub fn push(&mut self, value: f64) -> Option<Stats> {
        let stats = self.stats_for(value);

        self.buf.push_back(value);
        let d = value - self.offset;
        self.sum += d;
        self.sumsq += d * d;

        if self.buf.len() > self.window {
            let old = self.buf.pop_front().expect("buffer is non-empty");
            let d = old - self.offset;
            self.sum -= d;
            self.sumsq -= d * d;
        }

        self.since_rebuild += 1;
        if self.since_rebuild >= self.window {
            self.rebuild();
        }

        stats
    }

    fn stats_for(&self, value: f64) -> Option<Stats> {
        if self.buf.len() < self.window {
            return None;
        }
        let n = self.window as f64;
        let centred_mean = self.sum / n;
        let mean = self.offset + centred_mean;
        // Population variance (/ n), matching the Python. Using the sample
        // form (/ (n-1)) would shift every z by a factor of sqrt(n/(n-1)) and
        // silently retune every entry threshold.
        let var = (self.sumsq / n - centred_mean * centred_mean).max(VAR_FLOOR);
        let sd = var.sqrt();
        Some(Stats {
            mean,
            sd,
            z: (value - mean) / sd,
        })
    }

    /// Recompute the accumulators exactly from the buffer and re-centre.
    fn rebuild(&mut self) {
        self.offset = self.buf.front().copied().unwrap_or(0.0);
        self.sum = 0.0;
        self.sumsq = 0.0;
        for &x in &self.buf {
            let d = x - self.offset;
            self.sum += d;
            self.sumsq += d * d;
        }
        self.since_rebuild = 0;
    }
}

/// The pair spread: `ln(a) - ln(b)`.
///
/// A log spread rather than a ratio or a difference because it makes the two
/// legs' percentage moves additive, which is what lets an equal-notional pair
/// be scored with a single number.
pub fn log_spread(a: f64, b: f64) -> f64 {
    a.ln() - b.ln()
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p pairs`
Expected: PASS, 6 tests.

- [ ] **Step 6: Lint**

Run: `cargo clippy -p pairs --all-targets -- -D warnings`
Expected: no output.

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml crates/pairs
git commit -m "feat(pairs): O(1) rolling spread statistics at Python parity"
```

---

### Task 2: `crates/pairs` — signal rules

**Files:**
- Create: `crates/pairs/src/signal.rs`
- Modify: `crates/pairs/src/lib.rs`

**Interfaces:**
- Consumes: nothing from Task 1 (this module is independent).
- Produces: `pairs::signal::{PairSide, PairParams, ExitReason, SignalEngine, unrealized_pnl_fraction}`.
  - `PairSide::{LongSpread, ShortSpread}`, serde as `"long_spread"` / `"short_spread"`
  - `PairParams` — all fields `pub` (listed in Step 4)
  - `ExitReason::{Breakeven, Time, Target, Stop}` with `as_str(self) -> &'static str`
  - `SignalEngine::new(params: PairParams) -> SignalEngine`
  - `SignalEngine::entry_signal(&self, z: f64) -> Option<PairSide>`
  - `SignalEngine::should_arm_breakeven(&self, side: PairSide, z: f64) -> bool`
  - `SignalEngine::exit_reason(&self, side: PairSide, z: f64, age_bars: i64, breakeven_armed: bool, pnl_fraction: Option<Decimal>) -> Option<ExitReason>`
  - `SignalEngine::params(&self) -> &PairParams`
  - `unrealized_pnl_fraction(side, a_entry, b_entry, a_now, b_now, fee_per_leg) -> Decimal`

**Context the implementer needs:** ports `PairSignalEngine` and `unrealized_pair_pnl_fraction` from [`scripts/pairs_bot.py:223-278`](../../../scripts/pairs_bot.py). Two traps:

1. **The exit precedence must stay `breakeven → time → target → stop`.** It looks wrong — a bar that hits the stop *and* exceeds max-hold is labelled `time` — but every exit resolves on the same bar at the same price, so the precedence changes only the label and never the PnL. Reordering it would create a backtest divergence for no gain.
2. **The long-spread thresholds are the negation of the short-spread ones**, and the live config uses negative `target_z` (BNB/XAUT runs `target_z = -4.5`). That means a long-spread target sits at `z >= +4.5` — a real, deliberate "ride it to the opposite extreme" setting that reads like a sign error. Write the negation exactly once, in `PairSide`, so no call site can get it wrong.

- [ ] **Step 1: Write the failing tests**

`crates/pairs/src/signal.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn params() -> PairParams {
        PairParams {
            leg_a: Symbol::new("DOGEUSDT"),
            leg_b: Symbol::new("XRPUSDT"),
            timeframe: Timeframe::H1,
            rolling_window: 240,
            entry_z: 3.5,
            stop_z: 4.5,
            target_z: 0.5,
            max_hold_bars: 72,
            fee_per_leg: dec!(0.0002),
            per_leg_notional_usdt: dec!(25),
            risk_pct_of_equity: dec!(0.02),
            max_notional_multiple_of_equity: dec!(1),
            enable_breakeven: true,
            breakeven_r_multiple: dec!(2),
        }
    }

    #[test]
    fn a_high_positive_z_is_a_short_spread_signal() {
        let e = SignalEngine::new(params());
        assert_eq!(e.entry_signal(3.6), Some(PairSide::ShortSpread));
    }

    #[test]
    fn a_low_negative_z_is_a_long_spread_signal() {
        let e = SignalEngine::new(params());
        assert_eq!(e.entry_signal(-3.6), Some(PairSide::LongSpread));
    }

    #[test]
    fn no_signal_is_produced_inside_the_band() {
        let e = SignalEngine::new(params());
        assert_eq!(e.entry_signal(1.2), None);
        assert_eq!(e.entry_signal(-3.4), None);
    }

    #[test]
    fn the_band_edge_is_inclusive() {
        // Python uses >= and <=; an exclusive comparison would silently skip
        // the exact-threshold bar.
        let e = SignalEngine::new(params());
        assert_eq!(e.entry_signal(3.5), Some(PairSide::ShortSpread));
        assert_eq!(e.entry_signal(-3.5), Some(PairSide::LongSpread));
    }

    #[test]
    fn a_short_spread_takes_target_when_z_reverts() {
        let e = SignalEngine::new(params());
        assert_eq!(
            e.exit_reason(PairSide::ShortSpread, 0.4, 1, false, None),
            Some(ExitReason::Target)
        );
    }

    #[test]
    fn a_long_spread_takes_the_stop_when_z_keeps_widening() {
        let e = SignalEngine::new(params());
        assert_eq!(
            e.exit_reason(PairSide::LongSpread, -4.6, 1, false, None),
            Some(ExitReason::Stop)
        );
    }

    #[test]
    fn the_time_stop_fires_after_max_hold_bars() {
        let e = SignalEngine::new(params());
        assert_eq!(
            e.exit_reason(PairSide::ShortSpread, 2.0, 73, false, None),
            Some(ExitReason::Time)
        );
        assert_eq!(e.exit_reason(PairSide::ShortSpread, 2.0, 72, false, None), None);
    }

    #[test]
    fn time_outranks_stop_on_a_bar_that_hits_both() {
        // Deliberate parity with the Python. Both exits resolve on the same
        // bar at the same price, so this changes the label and nothing else —
        // and matching the label keeps backtest comparisons honest.
        let e = SignalEngine::new(params());
        assert_eq!(
            e.exit_reason(PairSide::ShortSpread, 5.0, 100, false, None),
            Some(ExitReason::Time)
        );
    }

    #[test]
    fn a_negative_target_z_puts_the_long_spread_target_at_the_opposite_extreme() {
        // BNB/XAUT runs target_z = -4.5. Long-spread targets at +4.5. This
        // reads like a sign error and is not one.
        let p = PairParams {
            target_z: -4.5,
            ..params()
        };
        let e = SignalEngine::new(p);
        assert_eq!(e.exit_reason(PairSide::LongSpread, 4.6, 1, false, None), Some(ExitReason::Target));
        assert_eq!(e.exit_reason(PairSide::LongSpread, 4.4, 1, false, None), None);
        assert_eq!(e.exit_reason(PairSide::ShortSpread, -4.6, 1, false, None), Some(ExitReason::Target));
    }

    #[test]
    fn breakeven_arms_at_two_risk_bands_of_favourable_movement() {
        // entry 3.5, stop 4.5 -> risk band 1.0; 2R of retracement is z <= 1.5.
        let e = SignalEngine::new(params());
        assert!(e.should_arm_breakeven(PairSide::ShortSpread, 1.5));
        assert!(!e.should_arm_breakeven(PairSide::ShortSpread, 1.6));
        assert!(e.should_arm_breakeven(PairSide::LongSpread, -1.5));
        assert!(!e.should_arm_breakeven(PairSide::LongSpread, -1.6));
    }

    #[test]
    fn breakeven_never_arms_when_the_feature_is_disabled() {
        // All three live profiles run with breakeven off.
        let p = PairParams {
            enable_breakeven: false,
            ..params()
        };
        let e = SignalEngine::new(p);
        assert!(!e.should_arm_breakeven(PairSide::ShortSpread, 0.0));
    }

    #[test]
    fn an_armed_breakeven_exits_the_moment_pnl_turns_non_positive() {
        let e = SignalEngine::new(params());
        let pnl = unrealized_pnl_fraction(
            PairSide::ShortSpread,
            dec!(10),
            dec!(1),
            dec!(10.3),
            dec!(0.99),
            dec!(0.0002),
        );
        assert!(pnl <= dec!(0), "pnl fraction was {pnl}");
        assert_eq!(
            e.exit_reason(PairSide::ShortSpread, 1.6, 5, true, Some(pnl)),
            Some(ExitReason::Breakeven)
        );
    }

    #[test]
    fn an_unarmed_breakeven_ignores_a_losing_pnl() {
        let e = SignalEngine::new(params());
        assert_eq!(
            e.exit_reason(PairSide::ShortSpread, 1.6, 5, false, Some(dec!(-0.5))),
            None
        );
    }

    #[test]
    fn pnl_fraction_nets_four_legs_of_fees() {
        // Two legs in, two legs out.
        let pnl = unrealized_pnl_fraction(
            PairSide::LongSpread,
            dec!(100),
            dec!(100),
            dec!(100),
            dec!(100),
            dec!(0.0002),
        );
        assert_eq!(pnl, dec!(-0.0008));
    }

    #[test]
    fn pair_side_serialises_to_the_python_strings() {
        // The journal and the Python dashboard both read these.
        assert_eq!(
            serde_json::to_string(&PairSide::LongSpread).unwrap(),
            "\"long_spread\""
        );
        assert_eq!(
            serde_json::to_string(&PairSide::ShortSpread).unwrap(),
            "\"short_spread\""
        );
    }
}
```

Add `serde_json.workspace = true` to `crates/pairs/Cargo.toml` `[dev-dependencies]`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p pairs signal`
Expected: FAIL — `cannot find type PairParams in this scope`.

- [ ] **Step 3: Write the implementation**

Above the test module in `crates/pairs/src/signal.rs`:

```rust
use botcore::{Symbol, Timeframe};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// Which way the spread is held.
///
/// `LongSpread` is long leg A and short leg B; `ShortSpread` is the mirror.
/// Named for the spread rather than for the legs because every threshold in
/// this module is expressed in spread z-units.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PairSide {
    LongSpread,
    ShortSpread,
}

impl PairSide {
    /// Has this side's target been reached at `z`?
    ///
    /// The long-spread thresholds are the negation of the short-spread ones,
    /// and that negation is written here — once — on purpose. The live config
    /// runs `target_z = -4.5` on BNB/XAUT, which puts the *long*-spread target
    /// at `z >= +4.5`: a deliberate "hold until the spread overshoots to the
    /// opposite extreme", and something that reads exactly like a sign error
    /// at any call site that re-derives it.
    pub fn target_reached(self, z: f64, target_z: f64) -> bool {
        match self {
            PairSide::ShortSpread => z <= target_z,
            PairSide::LongSpread => z >= -target_z,
        }
    }

    /// Has this side's stop been hit at `z`? The spread moved further against
    /// the position rather than reverting.
    pub fn stop_hit(self, z: f64, stop_z: f64) -> bool {
        match self {
            PairSide::ShortSpread => z >= stop_z,
            PairSide::LongSpread => z <= -stop_z,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            PairSide::LongSpread => "long_spread",
            PairSide::ShortSpread => "short_spread",
        }
    }
}

/// Everything that defines one pair's behaviour.
///
/// `Clone` rather than `Copy` because it carries two `Symbol`s; it is cloned
/// once per bot at startup and never in a hot path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PairParams {
    pub leg_a: Symbol,
    pub leg_b: Symbol,
    pub timeframe: Timeframe,
    pub rolling_window: usize,
    pub entry_z: f64,
    pub stop_z: f64,
    pub target_z: f64,
    pub max_hold_bars: i64,
    pub fee_per_leg: Decimal,
    /// Used only when `risk_pct_of_equity` is zero, which is the backtest's
    /// fixed-size mode. Every live profile runs risk-based sizing.
    pub per_leg_notional_usdt: Decimal,
    pub risk_pct_of_equity: Decimal,
    /// Ceiling on per-leg notional as a multiple of total equity. Defaults to
    /// `1`, which reproduces the Python's behaviour exactly — see
    /// `sizing::per_leg_notional` for why that default is worth arguing about.
    pub max_notional_multiple_of_equity: Decimal,
    pub enable_breakeven: bool,
    pub breakeven_r_multiple: Decimal,
}

impl PairParams {
    pub fn display_pair(&self) -> String {
        format!("{}/{}", self.leg_a, self.leg_b)
    }

    /// `|entry - target| / |stop - entry|`. Reported at startup so a
    /// misconfigured pair is visible in the first log line.
    pub fn reward_risk_ratio(&self) -> f64 {
        (self.entry_z - self.target_z).abs() / (self.stop_z - self.entry_z).abs()
    }

    /// `doge_xrp` for DOGEUSDT/XRPUSDT. Used as the `orderLinkId` prefix, so
    /// every order this bot places is attributable to it from Bybit's UI.
    pub fn slug(&self) -> String {
        fn norm(s: &Symbol) -> String {
            s.as_str().trim_end_matches("USDT").to_lowercase()
        }
        format!("{}_{}", norm(&self.leg_a), norm(&self.leg_b))
    }
}

/// Why a position was closed. The string forms are what the journal and the
/// Python dashboard already read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitReason {
    Breakeven,
    Time,
    Target,
    Stop,
}

impl ExitReason {
    pub fn as_str(self) -> &'static str {
        match self {
            ExitReason::Breakeven => "breakeven",
            ExitReason::Time => "time",
            ExitReason::Target => "target",
            ExitReason::Stop => "stop",
        }
    }
}

/// The entry and exit rules. Pure: no clock, no I/O, no interior mutability,
/// so every decision is reproducible from its arguments alone.
#[derive(Debug, Clone)]
pub struct SignalEngine {
    params: PairParams,
}

impl SignalEngine {
    pub fn new(params: PairParams) -> Self {
        Self { params }
    }

    pub fn params(&self) -> &PairParams {
        &self.params
    }

    /// A z at or beyond the entry band, or nothing.
    ///
    /// The comparison is inclusive, matching the Python: an exclusive one
    /// would skip a bar that landed exactly on the threshold.
    pub fn entry_signal(&self, z: f64) -> Option<PairSide> {
        if z >= self.params.entry_z {
            Some(PairSide::ShortSpread)
        } else if z <= -self.params.entry_z {
            Some(PairSide::LongSpread)
        } else {
            None
        }
    }

    /// Has the spread retraced far enough to justify protecting the trade?
    ///
    /// Measured in risk bands (`stop_z - entry_z`) rather than in absolute z,
    /// so the rule transfers across pairs with different thresholds.
    pub fn should_arm_breakeven(&self, side: PairSide, z: f64) -> bool {
        if !self.params.enable_breakeven {
            return false;
        }
        let risk_band = self.params.stop_z - self.params.entry_z;
        let arm_multiple: f64 = self
            .params
            .breakeven_r_multiple
            .try_into()
            .unwrap_or(f64::INFINITY);
        match side {
            PairSide::ShortSpread => z <= self.params.entry_z - arm_multiple * risk_band,
            PairSide::LongSpread => z >= -self.params.entry_z + arm_multiple * risk_band,
        }
    }

    /// Why this position should close now, if it should.
    ///
    /// Precedence is `breakeven → time → target → stop`, matching the Python
    /// exactly. It looks wrong — a bar that trips both the stop and max-hold
    /// is labelled `time` — but all four exits resolve on the same bar at the
    /// same price, so the order changes the label and nothing else. Keeping it
    /// is what lets a Rust backtest be compared to a Python one line by line.
    pub fn exit_reason(
        &self,
        side: PairSide,
        z: f64,
        age_bars: i64,
        breakeven_armed: bool,
        pnl_fraction: Option<Decimal>,
    ) -> Option<ExitReason> {
        if breakeven_armed && pnl_fraction.is_some_and(|p| p <= Decimal::ZERO) {
            return Some(ExitReason::Breakeven);
        }
        if age_bars > self.params.max_hold_bars {
            return Some(ExitReason::Time);
        }
        if side.target_reached(z, self.params.target_z) {
            return Some(ExitReason::Target);
        }
        if side.stop_hit(z, self.params.stop_z) {
            return Some(ExitReason::Stop);
        }
        None
    }
}

/// Unrealised PnL as a fraction of one leg's notional, net of all four fee legs
/// (two in, two out).
///
/// Assumes the two legs carry equal notional. They do not exactly — `sized_qty`
/// rounds each leg up independently to its own `qty_step` and `min_order_qty` —
/// so on a small account this is an approximation, and it is the input to the
/// breakeven exit. Task 7 journals the realised per-leg notionals so the size
/// of that approximation is measurable rather than assumed.
pub fn unrealized_pnl_fraction(
    side: PairSide,
    a_entry: Decimal,
    b_entry: Decimal,
    a_now: Decimal,
    b_now: Decimal,
    fee_per_leg: Decimal,
) -> Decimal {
    let a_ret = a_now / a_entry - Decimal::ONE;
    let b_ret = b_now / b_entry - Decimal::ONE;
    let gross = match side {
        PairSide::LongSpread => a_ret - b_ret,
        PairSide::ShortSpread => b_ret - a_ret,
    };
    // No f64 conversion and so no fallback: a silent zero-fee substitution
    // would overstate PnL and make the breakeven exit fire late, which is the
    // unsafe direction for a risk-control input.
    let fees = Decimal::from(4) * fee_per_leg;
    gross - fees
}
```

Update `crates/pairs/src/lib.rs`:

```rust
pub mod signal;
pub mod spread;

pub use signal::{ExitReason, PairParams, PairSide, SignalEngine, unrealized_pnl_fraction};
pub use spread::{RollingZ, Stats, log_spread};
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p pairs`
Expected: PASS, 21 tests.

- [ ] **Step 5: Lint and commit**

```bash
cargo clippy -p pairs --all-targets -- -D warnings
git add crates/pairs
git commit -m "feat(pairs): entry, exit and breakeven rules at Python parity"
```

---
### Task 3: `crates/pairs` — per-leg sizing, with the cap made visible

**Files:**
- Create: `crates/pairs/src/sizing.rs`
- Modify: `crates/pairs/src/lib.rs`

**Interfaces:**
- Consumes: `pairs::signal::PairParams` (Task 2).
- Produces: `pairs::sizing::{Sizing, CapReason, SizingError, per_leg_notional}`.
  - `per_leg_notional(params: &PairParams, total_equity: Decimal, available_equity: Decimal, spread_sigma: f64) -> Result<Sizing, SizingError>`
  - `Sizing { notional: Decimal, uncapped: Decimal, capped_by: Option<CapReason> }`
  - `CapReason::{AvailableEquity, MaxNotionalMultiple}`
  - `SizingError::{NonPositiveSigma, NonPositiveNotional}`

**Context the implementer needs:** ports `risk_based_per_leg_notional` from [`scripts/pairs_bot.py:280-292`](../../../scripts/pairs_bot.py). The formula is `total_equity * risk_pct / sigma`, then `min(desired, available_equity)`.

The thing to understand before writing it: **at realistic sigma the numerator exceeds equity, so the `min()` binds and each leg goes out at 100% of equity — 2x gross exposure.** This was confirmed by instrumenting the backtest: per-leg notional tracks equity exactly once the cap engages, which is what produces the dashboard's 8,249% headline for AAVE/ETH. That is a live risk setting nobody typed.

This task does **not** change the number. It keeps the arithmetic bit-identical and makes the cap *observable*: the return type reports what capped it and what the uncapped figure was, so the journal and the logs carry it. The new `max_notional_multiple_of_equity` knob defaults to `1`, which cannot change today's behaviour, but turns the ceiling into a decision someone can lower.

Parity detail: the Python converts the float sigma with `Decimal(str(spread_sigma))`. Rust's `f64` `Display` produces the same shortest-round-trip form as Python's `str()`, so `Decimal::from_str(&sigma.to_string())` reproduces it. `Decimal::try_from(f64)` does **not** — it rounds differently — so do not use it here.

- [ ] **Step 1: Write the failing tests**

`crates/pairs/src/sizing.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use botcore::{Symbol, Timeframe};
    use rust_decimal_macros::dec;

    fn params() -> PairParams {
        PairParams {
            leg_a: Symbol::new("DOGEUSDT"),
            leg_b: Symbol::new("XRPUSDT"),
            timeframe: Timeframe::H1,
            rolling_window: 240,
            entry_z: 3.5,
            stop_z: 4.5,
            target_z: 0.5,
            max_hold_bars: 72,
            fee_per_leg: dec!(0.0002),
            per_leg_notional_usdt: dec!(25),
            risk_pct_of_equity: dec!(0.02),
            max_notional_multiple_of_equity: dec!(1),
            enable_breakeven: true,
            breakeven_r_multiple: dec!(2),
        }
    }

    #[test]
    fn size_scales_with_equity_and_inversely_with_spread_volatility() {
        // Python: 10000 * 0.02 / 0.05 == 4000, under the cap.
        let s = per_leg_notional(&params(), dec!(10000), dec!(10000), 0.05).unwrap();
        assert_eq!(s.notional, dec!(4000));
        assert_eq!(s.capped_by, None);
    }

    #[test]
    fn available_equity_caps_the_size_and_says_so() {
        // Python: 10000 * 0.02 / 0.0004 == 500000, capped to available 8000.
        let s = per_leg_notional(&params(), dec!(10000), dec!(8000), 0.0004).unwrap();
        assert_eq!(s.notional, dec!(8000));
        assert_eq!(s.uncapped, dec!(500000));
        assert_eq!(s.capped_by, Some(CapReason::AvailableEquity));
    }

    #[test]
    fn the_notional_multiple_can_bind_before_available_equity_does() {
        // The knob that exists so 2x gross is a choice rather than an accident.
        let p = PairParams {
            max_notional_multiple_of_equity: dec!(0.25),
            ..params()
        };
        let s = per_leg_notional(&p, dec!(10000), dec!(10000), 0.0004).unwrap();
        assert_eq!(s.notional, dec!(2500));
        assert_eq!(s.capped_by, Some(CapReason::MaxNotionalMultiple));
    }

    #[test]
    fn the_default_multiple_of_one_reproduces_the_python_exactly() {
        // available <= total in every real account state, so with the default
        // multiple the available-equity cap is always the binding one — which
        // is precisely the Python's behaviour.
        let s = per_leg_notional(&params(), dec!(10000), dec!(9000), 0.0001).unwrap();
        assert_eq!(s.notional, dec!(9000));
        assert_eq!(s.capped_by, Some(CapReason::AvailableEquity));
    }

    #[test]
    fn a_non_positive_sigma_is_refused_rather_than_dividing_by_zero() {
        assert_eq!(
            per_leg_notional(&params(), dec!(10000), dec!(10000), 0.0),
            Err(SizingError::NonPositiveSigma)
        );
        assert_eq!(
            per_leg_notional(&params(), dec!(10000), dec!(10000), -0.01),
            Err(SizingError::NonPositiveSigma)
        );
    }

    #[test]
    fn a_drained_account_is_refused_rather_than_sending_a_zero_qty_order() {
        assert_eq!(
            per_leg_notional(&params(), dec!(10000), dec!(0), 0.05),
            Err(SizingError::NonPositiveNotional)
        );
    }

    #[test]
    fn sigma_is_converted_the_way_python_converts_it() {
        // Decimal(str(0.1)) is 0.1, not the binary-expansion 0.1000000000000000055.
        // Decimal::try_from(0.1_f64) would not reproduce this.
        let s = per_leg_notional(&params(), dec!(100), dec!(1000000), 0.1).unwrap();
        assert_eq!(s.notional, dec!(20));
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p pairs sizing`
Expected: FAIL — `cannot find function per_leg_notional in this scope`.

- [ ] **Step 3: Write the implementation**

Above the test module in `crates/pairs/src/sizing.rs`:

```rust
use std::str::FromStr;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::signal::PairParams;

/// What stopped the risk formula from getting the size it asked for.
///
/// Recorded rather than discarded because in this strategy the cap is not an
/// edge case: at realistic spread volatility the risk formula asks for several
/// times equity, so the cap *is* the size on most entries. A `min()` that
/// silently decides position size should be legible in the journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapReason {
    AvailableEquity,
    MaxNotionalMultiple,
}

/// A sizing decision, including the figure that was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sizing {
    /// What to actually send, per leg.
    pub notional: Decimal,
    /// What the risk formula asked for before any cap.
    pub uncapped: Decimal,
    pub capped_by: Option<CapReason>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SizingError {
    #[error("spread sigma must be positive")]
    NonPositiveSigma,
    #[error("per-leg notional resolved to zero or less")]
    NonPositiveNotional,
}

/// Per-leg notional in USDT: risk a fixed fraction of equity per unit of
/// spread volatility, then cap.
///
/// Parity note: the Python converts the `f64` sigma with `Decimal(str(sigma))`.
/// Rust's `f64` `Display` emits the same shortest-round-trip decimal as
/// Python's `str()`, so going through the string is what reproduces it.
/// `Decimal::try_from(f64)` rounds differently and would put this out of
/// parity on the third significant figure.
pub fn per_leg_notional(
    params: &PairParams,
    total_equity: Decimal,
    available_equity: Decimal,
    spread_sigma: f64,
) -> Result<Sizing, SizingError> {
    if !(spread_sigma > 0.0) {
        return Err(SizingError::NonPositiveSigma);
    }
    let sigma =
        Decimal::from_str(&spread_sigma.to_string()).map_err(|_| SizingError::NonPositiveSigma)?;
    if sigma <= Decimal::ZERO {
        return Err(SizingError::NonPositiveSigma);
    }

    let uncapped = total_equity * params.risk_pct_of_equity / sigma;

    let multiple_cap = total_equity * params.max_notional_multiple_of_equity;
    let (notional, capped_by) = if uncapped <= available_equity && uncapped <= multiple_cap {
        (uncapped, None)
    } else if available_equity <= multiple_cap {
        (available_equity, Some(CapReason::AvailableEquity))
    } else {
        (multiple_cap, Some(CapReason::MaxNotionalMultiple))
    };

    if notional <= Decimal::ZERO {
        return Err(SizingError::NonPositiveNotional);
    }
    Ok(Sizing {
        notional,
        uncapped,
        capped_by,
    })
}
```

Add to `crates/pairs/src/lib.rs`:

```rust
pub mod sizing;

pub use sizing::{CapReason, Sizing, SizingError, per_leg_notional};
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p pairs`
Expected: PASS, 28 tests.

- [ ] **Step 5: Lint and commit**

```bash
cargo clippy -p pairs --all-targets -- -D warnings
git add crates/pairs
git commit -m "feat(pairs): per-leg sizing with the equity cap reported rather than silent"
```

---

### Task 4: Leg placement on `ExchangeClient`

**Files:**
- Modify: `crates/botcore/src/order.rs` (append `LimitLeg`, `OrderStatus`, `OrderState::is_terminal`)
- Modify: `crates/botcore/src/lib.rs:8` (re-export)
- Modify: `crates/exchange/src/traits.rs:22-40` (two new trait methods)
- Modify: `crates/exchange/src/bybit/rest.rs` (implement both)
- Modify: `crates/exchange/src/bybit/wire.rs:292-355` (`avgPrice` + `into_order_status`)
- Modify: `crates/engine/src/mock.rs` (implement both)
- Modify: `crates/backtest/src/sim_exchange.rs` (implement both)
- Test: `crates/exchange/tests/rest_legs.rs`

**Interfaces:**
- Consumes: nothing from Tasks 1–3.
- Produces:
  - `botcore::LimitLeg { symbol: Symbol, side: Side, qty: Decimal, price: Decimal, order_link_id: String, reduce_only: bool }`
  - `botcore::OrderStatus { symbol, order_id, order_link_id, side, state: OrderState, qty, cum_exec_qty, avg_price, updated_time_ms }`
  - `OrderState::is_terminal(self) -> bool`
  - `ExchangeClient::place_limit_leg(&self, req: LimitLeg) -> Result<OrderAck, ExchangeError>`
  - `ExchangeClient::order_by_link_id(&self, symbol: &Symbol, link_id: &str) -> Result<Option<OrderStatus>, ExchangeError>`

**Context the implementer needs:** `place_limit_entry` cannot be reused for pair legs. It hard-codes `timeInForce: "PostOnly"` and mandatorily attaches `stopLoss`/`takeProfit` ([`crates/exchange/src/bybit/rest.rs:499-515`](../../crates/exchange/src/bybit/rest.rs)). A pair leg is priced *through* the book, so `PostOnly` would be rejected outright; and a pair's stop is a spread z-score, not a price on either leg, so there is no per-leg stop to attach. Hence a second, narrower order type.

These go on `ExchangeClient` rather than a parallel trait for the reason stated in that trait's own doc comment: `SimulatedExchange` implements the same trait, and that is what makes the backtester drive the identical pipeline as live trading. A separate trait would give the pairs backtest a different execution path from the pairs bot.

`LimitLeg` still carries no market-order escape hatch — the limit-only rule stays enforced by what can be constructed.

- [ ] **Step 1: Write the failing tests**

`crates/exchange/tests/rest_legs.rs`:

```rust
use botcore::{LimitLeg, OrderState, Side, Symbol};
use exchange::ExchangeClient;
use exchange::bybit::rest::BybitRest;
use exchange::bybit::sign::Credentials;
use rust_decimal_macros::dec;
use wiremock::matchers::{body_partial_json, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn creds() -> Credentials {
    Credentials {
        api_key: "k".into(),
        api_secret: "s".into(),
    }
}

fn leg() -> LimitLeg {
    LimitLeg {
        symbol: Symbol::new("AAVEUSDT"),
        side: Side::Buy,
        qty: dec!(0.5),
        price: dec!(300.25),
        order_link_id: "aave_eth-a-1700000000".into(),
        reduce_only: false,
    }
}

#[tokio::test]
async fn a_leg_is_sent_as_a_gtc_limit_with_no_protection_attached() {
    let server = MockServer::start().await;

    // GTC, not PostOnly: the price crosses the book on purpose, and PostOnly
    // would be rejected. Not IOC: a partial fill that cancels the remainder
    // leaves the pair mismatched. And no stopLoss/takeProfit, because a pair's
    // stop is a spread z-score, not a price on either leg.
    Mock::given(method("POST"))
        .and(path("/v5/order/create"))
        .and(body_partial_json(serde_json::json!({
            "category": "linear",
            "symbol": "AAVEUSDT",
            "side": "Buy",
            "orderType": "Limit",
            "timeInForce": "GTC",
            "positionIdx": 0,
            "qty": "0.5",
            "price": "300.25",
            "orderLinkId": "aave_eth-a-1700000000",
            "reduceOnly": false
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "retCode": 0, "retMsg": "OK",
            "result": {"orderId": "oid-9", "orderLinkId": "aave_eth-a-1700000000"},
            "time": 1700007200000i64
        })))
        .mount(&server)
        .await;

    let client = BybitRest::new(server.uri(), creds());
    let ack = client.place_limit_leg(leg()).await.expect("leg placed");
    assert_eq!(ack.order_id, "oid-9");
}

#[tokio::test]
async fn a_closing_leg_is_marked_reduce_only() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v5/order/create"))
        .and(body_partial_json(serde_json::json!({"reduceOnly": true})))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "retCode": 0, "retMsg": "OK",
            "result": {"orderId": "oid-10", "orderLinkId": "x"},
            "time": 1700007200000i64
        })))
        .mount(&server)
        .await;

    let client = BybitRest::new(server.uri(), creds());
    client
        .place_limit_leg(LimitLeg {
            reduce_only: true,
            ..leg()
        })
        .await
        .expect("closing leg placed");
}

#[tokio::test]
async fn a_resting_order_is_found_in_the_realtime_table() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v5/order/realtime"))
        .and(query_param("orderLinkId", "link-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "retCode": 0, "retMsg": "OK",
            "result": {"list": [{
                "symbol": "AAVEUSDT", "orderId": "oid-1", "orderLinkId": "link-1",
                "side": "Buy", "price": "300.25", "qty": "0.5",
                "cumExecQty": "0", "avgPrice": "", "orderStatus": "New",
                "createdTime": "1700007200000", "updatedTime": "1700007201000"
            }]},
            "time": 1700007202000i64
        })))
        .mount(&server)
        .await;

    let client = BybitRest::new(server.uri(), creds());
    let got = client
        .order_by_link_id(&Symbol::new("AAVEUSDT"), "link-1")
        .await
        .expect("query succeeded")
        .expect("order found");
    assert_eq!(got.state, OrderState::New);
    assert_eq!(got.cum_exec_qty, dec!(0));
    // An empty avgPrice means nothing filled; it must decode to zero rather
    // than failing the whole order.
    assert_eq!(got.avg_price, dec!(0));
    assert!(!got.state.is_terminal());
}

#[tokio::test]
async fn a_filled_order_that_has_left_the_realtime_table_is_found_in_history() {
    let server = MockServer::start().await;
    // Bybit drops terminal orders out of /order/realtime after a short window.
    // Checking only realtime is how the Python's wait loop could time out on an
    // order that had in fact filled.
    Mock::given(method("GET"))
        .and(path("/v5/order/realtime"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "retCode": 0, "retMsg": "OK", "result": {"list": []}, "time": 1700007202000i64
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v5/order/history"))
        .and(query_param("orderLinkId", "link-2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "retCode": 0, "retMsg": "OK",
            "result": {"list": [{
                "symbol": "AAVEUSDT", "orderId": "oid-2", "orderLinkId": "link-2",
                "side": "Sell", "price": "300.25", "qty": "0.5",
                "cumExecQty": "0.5", "avgPrice": "300.31", "orderStatus": "Filled",
                "createdTime": "1700007200000", "updatedTime": "1700007205000"
            }]},
            "time": 1700007206000i64
        })))
        .mount(&server)
        .await;

    let client = BybitRest::new(server.uri(), creds());
    let got = client
        .order_by_link_id(&Symbol::new("AAVEUSDT"), "link-2")
        .await
        .expect("query succeeded")
        .expect("order found in history");
    assert_eq!(got.state, OrderState::Filled);
    assert_eq!(got.avg_price, dec!(300.31));
    assert_eq!(got.cum_exec_qty, dec!(0.5));
    assert!(got.state.is_terminal());
}

#[tokio::test]
async fn an_unknown_link_id_is_none_rather_than_an_error() {
    // A placement that never reached the exchange must be distinguishable
    // from one that was rejected. `None` says "no such order"; an Err would
    // send the executor down the unwind path for a leg that does not exist.
    let server = MockServer::start().await;
    for p in ["/v5/order/realtime", "/v5/order/history"] {
        Mock::given(method("GET"))
            .and(path(p))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "retCode": 0, "retMsg": "OK", "result": {"list": []}, "time": 1700007202000i64
            })))
            .mount(&server)
            .await;
    }

    let client = BybitRest::new(server.uri(), creds());
    let got = client
        .order_by_link_id(&Symbol::new("AAVEUSDT"), "nope")
        .await
        .expect("query succeeded");
    assert!(got.is_none());
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p exchange --test rest_legs`
Expected: FAIL — `no method named place_limit_leg`.

- [ ] **Step 3: Add the botcore types**

Append to `crates/botcore/src/order.rs`:

```rust
/// One leg of a pair trade: a plain limit order with no protection attached.
///
/// Separate from [`LimitEntry`] because the two are genuinely different orders.
/// A `LimitEntry` is PostOnly and carries a stop and target, which is right for
/// a directional setup. A pair leg is priced *through* the book so it fills now
/// — PostOnly would be rejected — and has no per-leg stop, because the pair's
/// stop is a spread z-score that neither leg's price can express.
///
/// There is still no market-order equivalent anywhere in the workspace. The
/// limit-only rule stays enforced by what can be constructed, including on the
/// unwind path where a market order would be most tempting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LimitLeg {
    pub symbol: Symbol,
    pub side: Side,
    pub qty: Decimal,
    pub price: Decimal,
    /// Deterministic idempotency key, max 36 characters.
    pub order_link_id: String,
    /// `true` for a leg that closes an existing position. The exchange then
    /// refuses to let it open one in the opposite direction, which is what
    /// makes a duplicate close attempt harmless rather than a new position.
    pub reduce_only: bool,
}

/// The exchange's view of one order, resting or terminal.
///
/// Distinct from [`OpenOrder`] because it carries `avg_price` — the volume
/// weighted fill price, which is what a pair position's PnL is computed
/// against. `OpenOrder::price` is the price the order was *placed* at, and
/// using it as the entry price would quietly understate slippage on every
/// trade.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderStatus {
    pub symbol: Symbol,
    pub order_id: String,
    pub order_link_id: String,
    pub side: Side,
    pub state: OrderState,
    pub qty: Decimal,
    pub cum_exec_qty: Decimal,
    /// Zero when nothing has filled. Bybit sends `""` in that case.
    pub avg_price: Decimal,
    pub updated_time_ms: i64,
}

impl OrderState {
    /// Whether this order will never change again.
    ///
    /// The executor polls until this is true. `PartiallyFilled` is
    /// deliberately *not* terminal: a partial fill is still working, and
    /// treating it as done would record a pair position at the wrong size.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            OrderState::Filled | OrderState::Cancelled | OrderState::Rejected
        )
    }
}
```

Update the re-export in `crates/botcore/src/lib.rs`:

```rust
pub use order::{LimitEntry, LimitLeg, OpenOrder, OrderAck, OrderState, OrderStatus, Side};
```

- [ ] **Step 4: Add the wire decoding**

In `crates/exchange/src/bybit/wire.rs`, add one field to `OpenOrderRow` (after `updated_time`):

```rust
    // Empty string when nothing has filled, which is why this is optional and
    // decodes to zero rather than failing: an unfilled order is a normal state,
    // not a decode error.
    #[serde(rename = "avgPrice", default)]
    pub avg_price: Option<String>,
```

And add a second conversion beside `into_open_order`, inside the same `impl OpenOrderRow`:

```rust
    /// Decode into the richer status shape the pair executor polls on.
    ///
    /// Shares `OpenOrderRow` with `into_open_order` rather than introducing a
    /// second row type, so a Bybit field rename can only break one decoder.
    pub fn into_order_status(self) -> Result<OrderStatus, ExchangeError> {
        let symbol = Symbol::new(self.symbol.clone());
        let side = parse_side(&self.side)?;
        let avg_price = match self.avg_price.as_deref() {
            None | Some("") => Decimal::ZERO,
            Some(s) => req_decimal(s, "avgPrice")?,
        };
        let open = self.into_open_order()?;
        Ok(OrderStatus {
            symbol,
            order_id: open.order_id,
            order_link_id: open.order_link_id,
            side,
            state: open.state,
            qty: open.qty,
            cum_exec_qty: open.cum_exec_qty,
            avg_price,
            updated_time_ms: open.updated_time_ms,
        })
    }
```

Add `OrderStatus` to the `botcore` import list at the top of `wire.rs`.

- [ ] **Step 5: Add the trait methods**

In `crates/exchange/src/traits.rs`, inside `pub trait ExchangeClient`, after `place_limit_entry`:

```rust
    /// Place one leg of a pair trade: a GTC limit with no protection attached.
    ///
    /// `orderType` is hard-coded to `"Limit"` here exactly as it is in
    /// `place_limit_entry`; no parameter can change it.
    async fn place_limit_leg(&self, req: LimitLeg) -> Result<OrderAck, ExchangeError>;

    /// Look one order up by its `orderLinkId`, wherever it currently lives.
    ///
    /// Checks the realtime table first, then order history. Both are needed:
    /// Bybit drops terminal orders out of `/v5/order/realtime` after a short
    /// window, so a poll that only reads realtime can time out on an order
    /// that has in fact filled — and then the caller unwinds a leg it still
    /// holds.
    ///
    /// `Ok(None)` means the exchange has never heard of this id, which is a
    /// materially different state from a rejection and must not be collapsed
    /// into an error.
    async fn order_by_link_id(
        &self,
        symbol: &Symbol,
        link_id: &str,
    ) -> Result<Option<OrderStatus>, ExchangeError>;
```

Add `LimitLeg, OrderStatus` to the `botcore` import at the top of `traits.rs`.

- [ ] **Step 6: Implement on `BybitRest`**

In `crates/exchange/src/bybit/rest.rs`, inside `impl ExchangeClient for BybitRest`, after `place_limit_entry`:

```rust
    async fn place_limit_leg(&self, req: LimitLeg) -> Result<OrderAck, ExchangeError> {
        let body = json!({
            "category": "linear",
            "symbol": req.symbol.as_str(),
            "side": req.side.as_bybit(),
            "orderType": "Limit",
            // GTC and not PostOnly: the price crosses the book deliberately.
            // GTC and not IOC: a partial fill that cancels its own remainder
            // would leave the pair carrying mismatched leg sizes with nothing
            // recording the intent.
            "timeInForce": "GTC",
            "positionIdx": 0,
            "qty": req.qty.normalize().to_string(),
            "price": req.price.normalize().to_string(),
            "orderLinkId": req.order_link_id,
            "reduceOnly": req.reduce_only,
        });
        let res: OrderCreateResult = self.post("/v5/order/create", body).await?;
        Ok(OrderAck {
            order_id: res.order_id,
            order_link_id: res.order_link_id,
        })
    }

    async fn order_by_link_id(
        &self,
        symbol: &Symbol,
        link_id: &str,
    ) -> Result<Option<OrderStatus>, ExchangeError> {
        for path in ["/v5/order/realtime", "/v5/order/history"] {
            let res: ListResult<OpenOrderRow> = self
                .get(
                    path,
                    &[
                        ("category", "linear".into()),
                        ("symbol", symbol.as_str().into()),
                        ("orderLinkId", link_id.into()),
                    ],
                )
                .await?;
            if let Some(row) = res.list.into_iter().next() {
                return Ok(Some(row.into_order_status()?));
            }
        }
        Ok(None)
    }
```

Add `LimitLeg, OrderStatus` to the `botcore` import at the top of `rest.rs`.

- [ ] **Step 7: Implement on the two other `ExchangeClient`s**

`crates/engine/src/mock.rs` and `crates/backtest/src/sim_exchange.rs` will not compile until they implement the new methods. Give each the minimal behaviour consistent with what it already does:

- `MockExchange`: record the leg in the same `Vec` it already records entries in, return a synthetic `OrderAck`; `order_by_link_id` returns whatever the test scripted, `None` by default.
- `SimulatedExchange`: fill the leg at its limit price plus the crossing cost the simulator already models for entries, and return a `Filled` `OrderStatus` from `order_by_link_id`.

Run `cargo check --workspace --all-targets` and fix each error the compiler names. This is the point of putting the methods on the shared trait: the compiler enumerates every implementor rather than leaving one silently unported.

- [ ] **Step 8: Run the tests to verify they pass**

Run: `cargo test -p exchange && cargo test --workspace`
Expected: PASS, including the 5 new tests in `rest_legs.rs` and all 540 pre-existing tests.

- [ ] **Step 9: Lint and commit**

```bash
cargo clippy --workspace --all-targets -- -D warnings
git add crates/botcore crates/exchange crates/engine crates/backtest
git commit -m "feat(exchange): GTC leg placement and link-id order lookup"
```

---
### Task 5: `crates/pairs` — pure two-leg helpers (pricing and settlement)

**Files:**
- Create: `crates/pairs/src/pricing.rs`
- Create: `crates/pairs/src/settle.rs`
- Modify: `crates/pairs/src/lib.rs`
- Modify: `crates/botcore/src/symbol.rs` (add `Instrument::min_notional`)
- Modify: `crates/exchange/src/bybit/wire.rs:116-140` (decode `minNotionalValue`)
- Modify: 43 files carrying an `Instrument { .. }` literal (mechanical, Step 1)

**Interfaces:**
- Consumes: `botcore::{Instrument, Side, Symbol, OrderState}` (Task 4 for `OrderState::is_terminal`).
- Produces:
  - `pairs::pricing::{floor_step, ceil_step, aggressive_limit_price, sized_qty}`
    - `aggressive_limit_price(side: Side, bid: Decimal, ask: Decimal, tick: Decimal, ticks_through: Decimal) -> Decimal`
    - `sized_qty(notional: Decimal, price: Decimal, instrument: &Instrument) -> Decimal`
  - `pairs::settle::{Leg, LegReport, Settlement, settle}`
    - `settle(a: LegReport, b: LegReport) -> Settlement`
    - `Settlement::{Opened { a: LegReport, b: LegReport }, Flat, Unwind(Vec<LegReport>)}`

**Context the implementer needs:** ports `floor_step`, `ceil_step`, `aggressive_limit_price` and `sized_qty` from [`scripts/pairs_bot.py:606-635`](../../../scripts/pairs_bot.py), and introduces the classification that Task 6's executor branches on.

`settle` is the answer to the Python's worst bug. In `place_pair_entry` a failure on leg B cancels leg A inside `except Exception: pass`; leg A is an aggressive limit already through the book, so it is usually *filled*, the cancel fails, the failure is swallowed, and the account carries a naked directional position while local state says flat. Making the classification a pure, exhaustively-tested function is what stops that case from being handled by an `except` clause again.

**The policy `settle` encodes:** a leg is judged by *executed quantity*, never by order status — a `Cancelled` order can carry a partial fill, and treating it as "not filled" is exactly how a leg goes unnoticed. And any pair that is not *fully* filled on *both* legs is unwound completely rather than carried. Carrying a lopsided pair (leg A 100%, leg B 10%) means running 90% naked directional risk on a strategy that has no directional edge. The cost of the strict rule is one round-turn of fees on a rare partial fill in a thin book; that is cheap insurance.

- [ ] **Step 1: Add `min_notional` to `Instrument`**

Bybit enforces a minimum order value per symbol (`lotSizeFilter.minNotionalValue`), and `sized_qty` must respect it or every order on a small account is rejected. It belongs on `Instrument` beside the other exchange-imposed constraints rather than duplicated into config, where it would drift from the exchange.

In `crates/botcore/src/symbol.rs`, add the field to `Instrument` immediately before `launch_time_ms`:

```rust
    /// Minimum order value in the quote currency. Zero when the exchange
    /// imposes none. Bybit sends this as `lotSizeFilter.minNotionalValue`.
    pub min_notional: Decimal,
```

In `crates/exchange/src/bybit/wire.rs`, add to `LotSizeFilter`:

```rust
    // Absent on some symbols, so it defaults rather than failing the decode:
    // a symbol with no minimum is a normal state, not a malformed response.
    #[serde(rename = "minNotionalValue", default)]
    pub min_notional_value: Option<String>,
```

and in the conversion that builds `Instrument`, add:

```rust
            min_notional: match self.lot_size_filter.min_notional_value.as_deref() {
                None | Some("") => Decimal::ZERO,
                Some(s) => parse(s, "minNotionalValue")?,
            },
```

Then fix the 43 test and helper files that build `Instrument` literally. The formatting is uniform — `min_order_qty:` on the line before `launch_time_ms:` — so this is mechanical:

```bash
grep -rl "launch_time_ms:" crates bot --include=*.rs \
  | grep -v "crates/botcore/src/symbol.rs" \
  | grep -v "crates/exchange/src/bybit/wire.rs" \
  | xargs sed -i -E 's/^(\s*)launch_time_ms:/\1min_notional: dec!(5),\n\1launch_time_ms:/'
cargo check --workspace --all-targets
```

`dec!(5)` is Bybit's uniform value for linear USDT perpetuals, so the fixtures stay realistic. Fix by hand any site the compiler still flags — a file that builds an `Instrument` without `dec!` in scope needs `Decimal::from(5)` instead.

Run: `cargo test --workspace`
Expected: PASS, all 540 pre-existing tests still green. If any test changed behaviour, the new floor is interacting with sizing and that is a real finding — stop and report it rather than adjusting the fixture.

- [ ] **Step 2: Write the failing pricing tests**

`crates/pairs/src/pricing.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use botcore::{Side, Symbol};
    use rust_decimal_macros::dec;

    fn instrument(qty_step: Decimal, min_qty: Decimal, min_notional: Decimal) -> Instrument {
        Instrument {
            symbol: Symbol::new("AAVEUSDT"),
            tick_size: dec!(0.01),
            qty_step,
            min_order_qty: min_qty,
            min_notional,
            launch_time_ms: 0,
        }
    }

    #[test]
    fn steps_round_toward_and_away_from_zero() {
        assert_eq!(floor_step(dec!(10.27), dec!(0.1)), dec!(10.2));
        assert_eq!(ceil_step(dec!(10.21), dec!(0.1)), dec!(10.3));
        assert_eq!(floor_step(dec!(10.2), dec!(0.1)), dec!(10.2));
        assert_eq!(ceil_step(dec!(10.2), dec!(0.1)), dec!(10.2));
    }

    #[test]
    fn a_buy_is_priced_through_the_ask_and_a_sell_through_the_bid() {
        // Crossing the book on purpose: the pair must fill on this bar, and a
        // resting order that misses the move leaves one leg naked.
        let buy = aggressive_limit_price(Side::Buy, dec!(299.90), dec!(300.00), dec!(0.01), dec!(5));
        assert_eq!(buy, dec!(300.05));
        let sell =
            aggressive_limit_price(Side::Sell, dec!(299.90), dec!(300.00), dec!(0.01), dec!(5));
        assert_eq!(sell, dec!(299.85));
    }

    #[test]
    fn a_wider_ladder_step_prices_further_through_the_book() {
        // What the unwind ladder in Task 6 escalates on.
        let sell =
            aggressive_limit_price(Side::Sell, dec!(299.90), dec!(300.00), dec!(0.01), dec!(25));
        assert_eq!(sell, dec!(299.65));
    }

    #[test]
    fn a_sell_price_can_never_be_driven_to_zero_or_below() {
        // A sub-penny asset with a wide ladder step would otherwise produce a
        // non-positive price, which the exchange rejects.
        let sell = aggressive_limit_price(Side::Sell, dec!(0.02), dec!(0.03), dec!(0.01), dec!(5));
        assert_eq!(sell, dec!(0.01));
        assert!(sell > Decimal::ZERO);
    }

    #[test]
    fn quantity_is_the_notional_over_the_price_rounded_up_to_the_step() {
        // Rounding up, matching the Python: an order rounded down below the
        // exchange minimum is rejected outright, which is worse than being a
        // fraction of a step large.
        let q = sized_qty(dec!(100), dec!(300), &instrument(dec!(0.01), dec!(0.01), dec!(0)));
        assert_eq!(q, dec!(0.34));
    }

    #[test]
    fn a_notional_below_the_exchange_minimum_is_raised_to_it() {
        let q = sized_qty(dec!(2), dec!(300), &instrument(dec!(0.01), dec!(0.01), dec!(5)));
        // 5 / 300 == 0.01666..., stepped up to 0.02.
        assert_eq!(q, dec!(0.02));
    }

    #[test]
    fn a_quantity_below_the_minimum_lot_is_raised_to_the_minimum_lot() {
        let q = sized_qty(dec!(1), dec!(300), &instrument(dec!(0.1), dec!(0.1), dec!(0)));
        assert_eq!(q, dec!(0.1));
    }
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p pairs pricing`
Expected: FAIL — `cannot find function aggressive_limit_price in this scope`.

- [ ] **Step 4: Write the pricing implementation**

Above the test module in `crates/pairs/src/pricing.rs`:

```rust
use botcore::{Instrument, Side};
use rust_decimal::Decimal;

/// Largest multiple of `step` not exceeding `x`.
pub fn floor_step(x: Decimal, step: Decimal) -> Decimal {
    (x / step).floor() * step
}

/// Smallest multiple of `step` not below `x`.
pub fn ceil_step(x: Decimal, step: Decimal) -> Decimal {
    (x / step).ceil() * step
}

/// A limit price placed `ticks_through` ticks on the far side of the book.
///
/// Deliberately crossing. Both legs of a pair must fill on the same bar or the
/// position is directional rather than market-neutral, so a resting price that
/// might miss is the wrong trade-off — the spread being captured is worth far
/// more than a few ticks of crossing cost. It is still a limit order: the
/// limit-only rule holds, and the price bounds the worst fill.
///
/// `ticks_through` is a parameter rather than a constant so the unwind ladder
/// can escalate it without a second pricing function that could drift.
pub fn aggressive_limit_price(
    side: Side,
    bid: Decimal,
    ask: Decimal,
    tick: Decimal,
    ticks_through: Decimal,
) -> Decimal {
    match side {
        Side::Buy => ceil_step(ask + tick * ticks_through, tick),
        Side::Sell => {
            let out = floor_step(bid - tick * ticks_through, tick);
            // A sub-penny asset with a wide ladder step can drive this to zero
            // or below, which the exchange rejects outright. One tick is the
            // lowest orderable price.
            if out <= Decimal::ZERO { tick } else { out }
        }
    }
}

/// Quantity for a target notional, lifted to satisfy every exchange floor.
///
/// Always rounds *up* to the quantity step, matching the Python. Rounding down
/// can land under `min_order_qty` and get the order rejected, and a rejected
/// leg is far more expensive than a fraction of a step of extra size.
///
/// The caller must know this: the two legs of a pair are rounded up
/// independently against their own steps and floors, so their notionals are
/// only approximately equal. On a small account the difference is material,
/// and it is why Task 7 journals the realised per-leg notionals rather than
/// assuming the requested ones.
pub fn sized_qty(notional: Decimal, price: Decimal, instrument: &Instrument) -> Decimal {
    let mut qty = notional / price;
    if instrument.min_notional > Decimal::ZERO && qty * price < instrument.min_notional {
        qty = instrument.min_notional / price;
    }
    qty = ceil_step(qty, instrument.qty_step);
    if qty < instrument.min_order_qty {
        qty = instrument.min_order_qty;
    }
    qty
}
```

- [ ] **Step 5: Write the failing settlement tests**

`crates/pairs/src/settle.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use botcore::{OrderState, Side, Symbol};
    use rust_decimal_macros::dec;

    fn report(leg: Leg, requested: Decimal, executed: Decimal, state: OrderState) -> LegReport {
        LegReport {
            leg,
            symbol: Symbol::new(if leg == Leg::A { "AAVEUSDT" } else { "ETHUSDT" }),
            side: if leg == Leg::A { Side::Buy } else { Side::Sell },
            requested_qty: requested,
            executed_qty: executed,
            avg_price: dec!(300),
            order_id: "oid".into(),
            state: Some(state),
        }
    }

    #[test]
    fn two_fully_filled_legs_open_the_position() {
        let s = settle(
            report(Leg::A, dec!(1), dec!(1), OrderState::Filled),
            report(Leg::B, dec!(2), dec!(2), OrderState::Filled),
        );
        assert!(matches!(s, Settlement::Opened { .. }));
    }

    #[test]
    fn two_untouched_legs_leave_the_account_flat() {
        let s = settle(
            report(Leg::A, dec!(1), dec!(0), OrderState::Cancelled),
            report(Leg::B, dec!(2), dec!(0), OrderState::Rejected),
        );
        assert_eq!(s, Settlement::Flat);
    }

    #[test]
    fn one_filled_leg_and_one_rejected_leg_is_the_orphan_case() {
        // The exact shape of the Python's worst bug: leg A fills, leg B is
        // rejected, and the account is left holding naked directional risk.
        let s = settle(
            report(Leg::A, dec!(1), dec!(1), OrderState::Filled),
            report(Leg::B, dec!(2), dec!(0), OrderState::Rejected),
        );
        match s {
            Settlement::Unwind(legs) => {
                assert_eq!(legs.len(), 1);
                assert_eq!(legs[0].leg, Leg::A);
                assert_eq!(legs[0].executed_qty, dec!(1));
            }
            other => panic!("expected Unwind, got {other:?}"),
        }
    }

    #[test]
    fn a_cancelled_order_that_partially_filled_still_counts_as_exposure() {
        // Judging by orderStatus rather than by executed quantity is how a
        // position goes unnoticed: Cancelled reads as "nothing happened" and
        // cumExecQty says otherwise.
        let s = settle(
            report(Leg::A, dec!(1), dec!(0.4), OrderState::Cancelled),
            report(Leg::B, dec!(2), dec!(0), OrderState::Cancelled),
        );
        match s {
            Settlement::Unwind(legs) => {
                assert_eq!(legs.len(), 1);
                assert_eq!(legs[0].executed_qty, dec!(0.4));
            }
            other => panic!("expected Unwind, got {other:?}"),
        }
    }

    #[test]
    fn a_lopsided_pair_is_unwound_entirely_rather_than_carried() {
        // Leg A full, leg B a tenth: carrying this is 90% naked directional
        // risk on a strategy with no directional edge. Both sides are flattened.
        let s = settle(
            report(Leg::A, dec!(1), dec!(1), OrderState::Filled),
            report(Leg::B, dec!(2), dec!(0.2), OrderState::Cancelled),
        );
        match s {
            Settlement::Unwind(legs) => assert_eq!(legs.len(), 2),
            other => panic!("expected Unwind, got {other:?}"),
        }
    }

    #[test]
    fn an_order_the_exchange_never_heard_of_carries_no_exposure() {
        let mut a = report(Leg::A, dec!(1), dec!(0), OrderState::New);
        a.state = None;
        let s = settle(a, report(Leg::B, dec!(2), dec!(0), OrderState::Cancelled));
        assert_eq!(s, Settlement::Flat);
    }
}
```

- [ ] **Step 6: Run the tests to verify they fail**

Run: `cargo test -p pairs settle`
Expected: FAIL — `cannot find type LegReport in this scope`.

- [ ] **Step 7: Write the settlement implementation**

Above the test module in `crates/pairs/src/settle.rs`:

```rust
use botcore::{OrderState, Side, Symbol};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// Which leg of the pair. `A` is the numerator of the log spread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Leg {
    A,
    B,
}

/// What one leg's order actually did, as the exchange reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegReport {
    pub leg: Leg,
    pub symbol: Symbol,
    pub side: Side,
    pub requested_qty: Decimal,
    pub executed_qty: Decimal,
    /// Volume-weighted fill price. Zero when nothing executed.
    pub avg_price: Decimal,
    pub order_id: String,
    /// `None` when the exchange has no record of the order at all — a
    /// placement that never landed, which is not the same as a rejection.
    pub state: Option<OrderState>,
}

impl LegReport {
    /// Did this leg get everything it asked for?
    pub fn is_fully_filled(&self) -> bool {
        self.executed_qty > Decimal::ZERO && self.executed_qty >= self.requested_qty
    }

    /// Is the account carrying a position because of this leg?
    ///
    /// Keyed on executed quantity and never on `state`, because a `Cancelled`
    /// order can carry a partial fill. Reading the status instead is how a
    /// live position becomes invisible to its own bot.
    pub fn has_exposure(&self) -> bool {
        self.executed_qty > Decimal::ZERO
    }
}

/// What the two leg reports add up to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Settlement {
    /// Both legs fully filled — the only state in which a pair position exists.
    Opened { a: LegReport, b: LegReport },
    /// Neither leg executed anything. Cancel any remnant and stay flat.
    Flat,
    /// The pair is not whole and at least one leg carries exposure. Every
    /// listed leg must be flattened before this bot does anything else.
    Unwind(Vec<LegReport>),
}

/// Classify a two-leg placement outcome.
///
/// Pure and total: every combination of leg reports maps to exactly one of
/// three states, and there is no path that returns success while leaving
/// exposure behind. That totality is the point — the Python handled this case
/// in an `except Exception: pass` and could leave a naked leg open indefinitely.
///
/// The strict rule (anything short of both-legs-full gets unwound) costs one
/// round-turn of fees on a rare partial fill in a thin book. Carrying a
/// lopsided pair instead would mean running directional risk on a strategy
/// that has no directional edge, which is a far worse trade.
pub fn settle(a: LegReport, b: LegReport) -> Settlement {
    if a.is_fully_filled() && b.is_fully_filled() {
        return Settlement::Opened { a, b };
    }
    let exposed: Vec<LegReport> = [a, b].into_iter().filter(LegReport::has_exposure).collect();
    if exposed.is_empty() {
        Settlement::Flat
    } else {
        Settlement::Unwind(exposed)
    }
}
```

Add to `crates/pairs/src/lib.rs`:

```rust
pub mod pricing;
pub mod settle;

pub use pricing::{aggressive_limit_price, ceil_step, floor_step, sized_qty};
pub use settle::{Leg, LegReport, Settlement, settle};
```

- [ ] **Step 8: Run the tests to verify they pass**

Run: `cargo test --workspace`
Expected: PASS — 41 tests in `pairs`, all pre-existing tests still green.

- [ ] **Step 9: Lint and commit**

```bash
cargo clippy --workspace --all-targets -- -D warnings
git add crates/pairs crates/botcore crates/exchange crates/engine crates/backtest crates/strategy crates/risk bot
git commit -m "feat(pairs): leg pricing, sizing and two-leg settlement classification"
```

---
### Task 6: `crates/pairs` — two-leg entry with a guaranteed unwind

**This is the task the whole port exists for.** Budget accordingly; it is the one place where getting it wrong loses real money.

**Files:**
- Create: `crates/pairs/src/executor.rs`
- Create: `crates/pairs/tests/support/mod.rs`
- Create: `crates/pairs/tests/entry_behaviour.rs`
- Modify: `crates/pairs/src/lib.rs`, `crates/pairs/Cargo.toml`
- Modify: `crates/exchange/src/traits.rs` (add `ticker`)
- Modify: `crates/exchange/src/bybit/rest.rs`, `crates/engine/src/mock.rs`, `crates/backtest/src/sim_exchange.rs`

**Interfaces:**
- Consumes: `pairs::signal::{PairParams, PairSide}` (Task 2), `pairs::pricing::*` and `pairs::settle::*` (Task 5), `botcore::{LimitLeg, OrderStatus}` and `ExchangeClient::{place_limit_leg, order_by_link_id}` (Task 4).
- Produces:
  - `ExchangeClient::ticker(&self, symbol: &Symbol) -> Result<Ticker, ExchangeError>`
  - `pairs::executor::{ExecutorConfig, LegQuote, ExecutionError, OpenedPair, open_pair, unwind_legs, poll_to_terminal}`
    - `open_pair(client: &dyn ExchangeClient, cfg: &ExecutorConfig, params: &PairParams, quotes: (&LegQuote, &LegQuote), side: PairSide, notional: Decimal, bar_ms: i64) -> Result<Option<OpenedPair>, ExecutionError>`
    - `OpenedPair { side, a: LegReport, b: LegReport, opened_at_ms: i64 }`

**Context the implementer needs — read this before writing code.**

The Python's entry is [`place_pair_entry`, `scripts/pairs_bot.py:772-836`](../../../scripts/pairs_bot.py). It places leg A, then leg B; on a leg-B failure it cancels leg A inside a bare `except Exception: pass`. Leg A is an aggressive limit already through the book, so by then it is usually *filled*: the cancel fails, the failure is swallowed, and the account holds naked directional risk while the state file says flat.

Three rules follow, and every one of them is load-bearing:

1. **Never trust a placement result.** A `place` call that returns `Err` may still have reached the exchange — a timeout after acceptance is the classic case. So after placing, *always* ask `order_by_link_id` what actually exists, and build the leg report from that answer. This is also why the `orderLinkId` is derived from the bar timestamp rather than the wall clock: a retry within the same bar reuses the id, and Bybit deduplicates instead of opening a second position. The Python used `int(time.time())`, which produced a fresh id on every retry.
2. **Exit through exactly one place.** `open_pair` returns `Ok(Some(pair))` only via `Settlement::Opened`. Every other path either leaves the account provably flat or returns `ExecutionError::UnwindExhausted`. There is no fourth outcome.
3. **The unwind is limit-only.** When exposure must be flattened, escalate the price further through the book on each attempt — 10, then 25, then 60 ticks. If the ladder is exhausted, return `UnwindExhausted` so the caller halts and alerts. **Do not add a market-order fallback.** That is a standing project rule; the mitigation for a limit stop that cannot fill is escalation plus a human, not a market order.

- [ ] **Step 1: Give `Ticker` a top of book, and add a single-symbol lookup**

`exchange::bybit::wire::Ticker` today carries only `symbol`, `turnover_24h` and
`last_price` — it was built for universe ranking, and it has **no bid or ask**.
The pair executor prices every leg off the book, so both must be added:

```rust
// in TickerRow
#[serde(rename = "bid1Price")]
pub bid1_price: String,
#[serde(rename = "ask1Price")]
pub ask1_price: String,
```

```rust
// in Ticker, with into_ticker parsing both
pub bid1: Decimal,
pub ask1: Decimal,
```

Ten `Ticker { .. }` literals across `crates/engine/src/universe.rs`,
`bot/tests/breakeven_behaviour.rs`, `bot/tests/protection_persistence.rs` and
`bot/tests/stop_escalation_behaviour.rs` will stop compiling; give each a
plausible bid/ask straddling its existing `last_price`.

The existing `tickers()` returns every linear symbol — 812 on testnet, measured at 4.5–10 s. That is fine for a daily universe re-rank and unusable inside an order path, where the unwind ladder needs a fresh quote between attempts.

In `crates/exchange/src/traits.rs`, inside `pub trait ExchangeClient`:

```rust
    /// One symbol's top of book.
    ///
    /// Distinct from [`ExchangeClient::tickers`], which returns every linear
    /// symbol and was measured at 4.5-10 s. That is fine for a daily universe
    /// re-rank and far too slow to sit inside an order path, where the unwind
    /// ladder needs a fresh quote between attempts.
    async fn ticker(&self, symbol: &Symbol) -> Result<Ticker, ExchangeError>;
```

In `crates/exchange/src/bybit/rest.rs`, inside `impl ExchangeClient for BybitRest`:

```rust
    async fn ticker(&self, symbol: &Symbol) -> Result<Ticker, ExchangeError> {
        let res: ListResult<TickerRow> = self
            .get(
                "/v5/market/tickers",
                &[
                    ("category", "linear".into()),
                    ("symbol", symbol.as_str().into()),
                ],
            )
            .await?;
        res.list
            .into_iter()
            .next()
            .ok_or_else(|| ExchangeError::Decode(format!("no ticker for {symbol}")))?
            .into_ticker()
    }
```

Use whatever the existing `tickers()` implementation names its row type and conversion — do not introduce a second decoder. Then implement `ticker` on `MockExchange` and `SimulatedExchange` (each can serve from the same data its `tickers()` already returns) until `cargo check --workspace --all-targets` is clean.

- [ ] **Step 2: Build the fault-injecting exchange double**

`crates/pairs/tests/support/mod.rs`:

```rust
//! A scriptable `ExchangeClient` for proving what the executor does when the
//! exchange misbehaves.
//!
//! Every interesting failure here is one that has actually cost someone money:
//! a leg that fills while its sibling is rejected, a placement that times out
//! after the exchange accepted it, an order that only partially fills. A mock
//! that always succeeds proves nothing about an executor whose entire job is
//! the failure path.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use async_trait::async_trait;
use botcore::{
    Balance, Candle, Instrument, LimitEntry, LimitLeg, OpenOrder, OrderAck, OrderState,
    OrderStatus, Position, Side, Symbol, Timeframe,
};
use exchange::ExchangeClient;
use exchange::bybit::transport::ExchangeError;
use exchange::bybit::wire::Ticker;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// What the exchange does with the next order placed on a given symbol.
#[derive(Debug, Clone)]
pub enum LegAction {
    /// Accepted and fully filled at `price`.
    Fills { price: Decimal },
    /// Accepted, then fills only `qty` before being cancelled.
    PartiallyFills { qty: Decimal, price: Decimal },
    /// Refused outright. `place` returns `Err`, and no order exists.
    Rejected,
    /// Accepted and left resting. `place` succeeds and the order never fills.
    Rests,
    /// `place` returns a transport error **but the exchange accepted and
    /// filled it anyway**. The executor must discover this by querying, not
    /// assume the leg is absent.
    TimesOutButFills { price: Decimal },
}

#[derive(Default)]
pub struct FaultExchange {
    /// Per-symbol script, consumed one action per placement, so the unwind
    /// ladder's successive attempts can behave differently.
    actions: Mutex<HashMap<String, VecDeque<LegAction>>>,
    orders: Mutex<HashMap<String, OrderStatus>>,
    pub placed: Mutex<Vec<LimitLeg>>,
    pub cancelled: Mutex<Vec<String>>,
    quotes: Mutex<HashMap<String, (Decimal, Decimal)>>,
}

impl FaultExchange {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn script(&self, symbol: &str, actions: Vec<LegAction>) -> &Self {
        self.actions
            .lock()
            .unwrap()
            .insert(symbol.into(), actions.into());
        self
    }

    pub fn quote(&self, symbol: &str, bid: Decimal, ask: Decimal) -> &Self {
        self.quotes.lock().unwrap().insert(symbol.into(), (bid, ask));
        self
    }

    /// Total executed quantity the account is left holding on `symbol`, signed
    /// by direction. The assertion every failure-path test ends with.
    pub fn net_exposure(&self, symbol: &str) -> Decimal {
        self.orders
            .lock()
            .unwrap()
            .values()
            .filter(|o| o.symbol.as_str() == symbol)
            .map(|o| match o.side {
                Side::Buy => o.cum_exec_qty,
                Side::Sell => -o.cum_exec_qty,
            })
            .sum()
    }

    fn next_action(&self, symbol: &str) -> LegAction {
        self.actions
            .lock()
            .unwrap()
            .get_mut(symbol)
            .and_then(|q| q.pop_front())
            .unwrap_or(LegAction::Fills { price: dec!(100) })
    }
}

#[async_trait]
impl ExchangeClient for FaultExchange {
    async fn place_limit_leg(&self, req: LimitLeg) -> Result<OrderAck, ExchangeError> {
        self.placed.lock().unwrap().push(req.clone());
        let action = self.next_action(req.symbol.as_str());

        let record = |state, exec: Decimal, price: Decimal| OrderStatus {
            symbol: req.symbol.clone(),
            order_id: format!("oid-{}", req.order_link_id),
            order_link_id: req.order_link_id.clone(),
            side: req.side,
            state,
            qty: req.qty,
            cum_exec_qty: exec,
            avg_price: price,
            updated_time_ms: 1_700_000_000_000,
        };

        let (status, result) = match action {
            LegAction::Fills { price } => (
                Some(record(OrderState::Filled, req.qty, price)),
                Ok(OrderAck {
                    order_id: format!("oid-{}", req.order_link_id),
                    order_link_id: req.order_link_id.clone(),
                }),
            ),
            LegAction::PartiallyFills { qty, price } => (
                Some(record(OrderState::Cancelled, qty, price)),
                Ok(OrderAck {
                    order_id: format!("oid-{}", req.order_link_id),
                    order_link_id: req.order_link_id.clone(),
                }),
            ),
            LegAction::Rests => (
                Some(record(OrderState::New, Decimal::ZERO, Decimal::ZERO)),
                Ok(OrderAck {
                    order_id: format!("oid-{}", req.order_link_id),
                    order_link_id: req.order_link_id.clone(),
                }),
            ),
            LegAction::Rejected => (
                None,
                Err(ExchangeError::Api {
                    code: 110007,
                    msg: "insufficient balance".into(),
                }),
            ),
            LegAction::TimesOutButFills { price } => (
                Some(record(OrderState::Filled, req.qty, price)),
                Err(ExchangeError::Decode("simulated transport timeout".into())),
            ),
        };
        if let Some(s) = status {
            self.orders.lock().unwrap().insert(req.order_link_id, s);
        }
        result
    }

    async fn order_by_link_id(
        &self,
        _symbol: &Symbol,
        link_id: &str,
    ) -> Result<Option<OrderStatus>, ExchangeError> {
        Ok(self.orders.lock().unwrap().get(link_id).cloned())
    }

    async fn cancel_order(&self, _symbol: &Symbol, link_id: &str) -> Result<(), ExchangeError> {
        self.cancelled.lock().unwrap().push(link_id.into());
        if let Some(o) = self.orders.lock().unwrap().get_mut(link_id)
            && o.state == OrderState::New
        {
            o.state = OrderState::Cancelled;
        }
        Ok(())
    }

    async fn ticker(&self, symbol: &Symbol) -> Result<Ticker, ExchangeError> {
        let (bid, ask) = self
            .quotes
            .lock()
            .unwrap()
            .get(symbol.as_str())
            .copied()
            .unwrap_or((dec!(99.9), dec!(100.1)));
        Ok(Ticker {
            symbol: symbol.clone(),
            turnover_24h: dec!(0),
            last_price: (bid + ask) / dec!(2),
            bid1: bid,
            ask1: ask,
        })
    }

    async fn positions(&self) -> Result<Vec<Position>, ExchangeError> {
        Ok(Vec::new())
    }

    // Not modelled. The executor never calls these, and a panicking stub is
    // better than a plausible-looking lie that hides a new dependency.
    async fn instruments(&self) -> Result<Vec<Instrument>, ExchangeError> {
        unimplemented!("FaultExchange does not model instruments")
    }
    async fn tickers(&self) -> Result<Vec<Ticker>, ExchangeError> {
        unimplemented!("FaultExchange does not model the all-symbols ticker")
    }
    async fn klines(
        &self,
        _s: &Symbol,
        _tf: Timeframe,
        _l: u16,
    ) -> Result<Vec<Candle>, ExchangeError> {
        unimplemented!("FaultExchange does not model klines")
    }
    async fn place_limit_entry(&self, _r: LimitEntry) -> Result<OrderAck, ExchangeError> {
        unimplemented!("pairs never places a directional LimitEntry")
    }
    async fn amend_stop(
        &self,
        _s: &Symbol,
        _t: Decimal,
        _l: Decimal,
    ) -> Result<(), ExchangeError> {
        unimplemented!("a pair's stop is a z-score, not an exchange stop")
    }
    async fn open_orders(&self) -> Result<Vec<OpenOrder>, ExchangeError> {
        Ok(Vec::new())
    }
    async fn set_leverage(&self, _s: &Symbol, _l: Decimal) -> Result<(), ExchangeError> {
        Ok(())
    }
    async fn balance(&self) -> Result<Balance, ExchangeError> {
        Ok(Balance {
            equity: dec!(10000),
            available: dec!(10000),
        })
    }
}
```

Add to `crates/pairs/Cargo.toml`:

```toml
[dependencies]
async-trait = "0.1"
exchange = { path = "../exchange" }
tokio.workspace = true
tracing.workspace = true

[dev-dependencies]
tokio = { workspace = true }
```

- [ ] **Step 3: Write the failing entry tests**

`crates/pairs/tests/entry_behaviour.rs`:

```rust
mod support;

use botcore::{Instrument, Symbol, Timeframe};
use pairs::executor::{ExecutionError, ExecutorConfig, LegQuote, open_pair};
use pairs::{PairParams, PairSide};
use rust_decimal_macros::dec;
use support::{FaultExchange, LegAction};

fn instrument(symbol: &str) -> Instrument {
    Instrument {
        symbol: Symbol::new(symbol),
        tick_size: dec!(0.01),
        qty_step: dec!(0.01),
        min_order_qty: dec!(0.01),
        min_notional: dec!(5),
        launch_time_ms: 0,
    }
}

fn quote(symbol: &str) -> LegQuote {
    LegQuote {
        instrument: instrument(symbol),
        bid: dec!(99.90),
        ask: dec!(100.10),
        last: dec!(100.00),
    }
}

fn params() -> PairParams {
    PairParams {
        leg_a: Symbol::new("AAVEUSDT"),
        leg_b: Symbol::new("ETHUSDT"),
        timeframe: Timeframe::H1,
        rolling_window: 180,
        entry_z: 3.0,
        stop_z: 4.0,
        target_z: 0.0,
        max_hold_bars: 48,
        fee_per_leg: dec!(0.0002),
        per_leg_notional_usdt: dec!(25),
        risk_pct_of_equity: dec!(0.03),
        max_notional_multiple_of_equity: dec!(1),
        enable_breakeven: false,
        breakeven_r_multiple: dec!(2),
    }
}

/// Zero delays so the tests exercise the logic, not the clock.
fn cfg() -> ExecutorConfig {
    ExecutorConfig {
        ticks_through: dec!(5),
        fill_timeout: std::time::Duration::ZERO,
        poll_interval: std::time::Duration::ZERO,
        unwind_ladder: vec![dec!(10), dec!(25), dec!(60)],
    }
}

#[tokio::test]
async fn both_legs_filling_opens_the_position() {
    let ex = FaultExchange::new();
    ex.script("AAVEUSDT", vec![LegAction::Fills { price: dec!(100.10) }]);
    ex.script("ETHUSDT", vec![LegAction::Fills { price: dec!(99.90) }]);

    let opened = open_pair(
        &ex,
        &cfg(),
        &params(),
        (&quote("AAVEUSDT"), &quote("ETHUSDT")),
        PairSide::LongSpread,
        dec!(1000),
        1_700_000_000_000,
    )
    .await
    .expect("no execution error")
    .expect("a position was opened");

    assert_eq!(opened.side, PairSide::LongSpread);
    assert_eq!(opened.a.avg_price, dec!(100.10));
    assert_eq!(opened.b.avg_price, dec!(99.90));
}

#[tokio::test]
async fn a_rejected_second_leg_flattens_the_first_and_opens_nothing() {
    // The Python's worst bug, asserted against directly: leg A fills, leg B is
    // rejected, and the account must end flat rather than holding naked risk.
    let ex = FaultExchange::new();
    ex.script(
        "AAVEUSDT",
        vec![
            LegAction::Fills { price: dec!(100.10) },
            // The unwind order.
            LegAction::Fills { price: dec!(99.80) },
        ],
    );
    ex.script("ETHUSDT", vec![LegAction::Rejected]);

    let got = open_pair(
        &ex,
        &cfg(),
        &params(),
        (&quote("AAVEUSDT"), &quote("ETHUSDT")),
        PairSide::LongSpread,
        dec!(1000),
        1_700_000_000_000,
    )
    .await
    .expect("the unwind succeeded, so this is not an error");

    assert!(got.is_none(), "no position may be reported");
    assert_eq!(
        ex.net_exposure("AAVEUSDT"),
        dec!(0),
        "leg A must have been flattened"
    );
    let placed = ex.placed.lock().unwrap();
    let unwind = placed.last().expect("an unwind order was placed");
    assert!(unwind.reduce_only, "an unwind must be reduce-only");
}

#[tokio::test]
async fn a_placement_that_times_out_after_the_exchange_filled_it_is_discovered() {
    // `place` returns Err while the order exists and is filled. Trusting the
    // Err and moving on would leave a naked leg with nothing recording it.
    let ex = FaultExchange::new();
    ex.script(
        "AAVEUSDT",
        vec![
            LegAction::TimesOutButFills { price: dec!(100.10) },
            LegAction::Fills { price: dec!(99.80) },
        ],
    );
    ex.script("ETHUSDT", vec![LegAction::Rejected]);

    let got = open_pair(
        &ex,
        &cfg(),
        &params(),
        (&quote("AAVEUSDT"), &quote("ETHUSDT")),
        PairSide::LongSpread,
        dec!(1000),
        1_700_000_000_000,
    )
    .await
    .expect("the unwind succeeded");

    assert!(got.is_none());
    assert_eq!(ex.net_exposure("AAVEUSDT"), dec!(0));
}

#[tokio::test]
async fn a_lopsided_fill_unwinds_both_legs() {
    let ex = FaultExchange::new();
    ex.script(
        "AAVEUSDT",
        vec![
            LegAction::Fills { price: dec!(100.10) },
            LegAction::Fills { price: dec!(99.80) },
        ],
    );
    ex.script(
        "ETHUSDT",
        vec![
            LegAction::PartiallyFills { qty: dec!(0.5), price: dec!(99.90) },
            LegAction::Fills { price: dec!(100.20) },
        ],
    );

    let got = open_pair(
        &ex,
        &cfg(),
        &params(),
        (&quote("AAVEUSDT"), &quote("ETHUSDT")),
        PairSide::LongSpread,
        dec!(1000),
        1_700_000_000_000,
    )
    .await
    .expect("the unwind succeeded");

    assert!(got.is_none());
    assert_eq!(ex.net_exposure("AAVEUSDT"), dec!(0));
    assert_eq!(ex.net_exposure("ETHUSDT"), dec!(0));
}

#[tokio::test]
async fn neither_leg_filling_leaves_the_account_flat_with_no_unwind() {
    let ex = FaultExchange::new();
    ex.script("AAVEUSDT", vec![LegAction::Rejected]);
    ex.script("ETHUSDT", vec![LegAction::Rejected]);

    let got = open_pair(
        &ex,
        &cfg(),
        &params(),
        (&quote("AAVEUSDT"), &quote("ETHUSDT")),
        PairSide::ShortSpread,
        dec!(1000),
        1_700_000_000_000,
    )
    .await
    .expect("nothing to unwind is not an error");

    assert!(got.is_none());
    assert!(
        ex.placed.lock().unwrap().iter().all(|o| !o.reduce_only),
        "no unwind order should have been placed"
    );
}

#[tokio::test]
async fn an_unwind_escalates_further_through_the_book_on_each_attempt() {
    let ex = FaultExchange::new();
    ex.quote("AAVEUSDT", dec!(99.90), dec!(100.10));
    ex.script(
        "AAVEUSDT",
        vec![
            LegAction::Fills { price: dec!(100.10) },
            LegAction::Rests, // first unwind attempt does not fill
            LegAction::Rests, // second does not either
            LegAction::Fills { price: dec!(99.30) }, // third gets there
        ],
    );
    ex.script("ETHUSDT", vec![LegAction::Rejected]);

    open_pair(
        &ex,
        &cfg(),
        &params(),
        (&quote("AAVEUSDT"), &quote("ETHUSDT")),
        PairSide::LongSpread,
        dec!(1000),
        1_700_000_000_000,
    )
    .await
    .expect("the third rung filled");

    let placed = ex.placed.lock().unwrap();
    let unwinds: Vec<_> = placed.iter().filter(|o| o.reduce_only).collect();
    assert_eq!(unwinds.len(), 3);
    // Selling out of a long: each rung is priced further below the bid.
    assert!(
        unwinds[0].price > unwinds[1].price && unwinds[1].price > unwinds[2].price,
        "prices were {:?}",
        unwinds.iter().map(|o| o.price).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn an_exhausted_unwind_ladder_reports_the_stranded_exposure_and_never_sends_a_market_order() {
    let ex = FaultExchange::new();
    ex.script(
        "AAVEUSDT",
        vec![
            LegAction::Fills { price: dec!(100.10) },
            LegAction::Rests,
            LegAction::Rests,
            LegAction::Rests,
        ],
    );
    ex.script("ETHUSDT", vec![LegAction::Rejected]);

    let err = open_pair(
        &ex,
        &cfg(),
        &params(),
        (&quote("AAVEUSDT"), &quote("ETHUSDT")),
        PairSide::LongSpread,
        dec!(1000),
        1_700_000_000_000,
    )
    .await
    .expect_err("stranded exposure must be an error, not a silent success");

    match err {
        ExecutionError::UnwindExhausted { legs } => {
            assert_eq!(legs.len(), 1);
            assert_eq!(legs[0].symbol.as_str(), "AAVEUSDT");
        }
        other => panic!("expected UnwindExhausted, got {other:?}"),
    }
    // The standing rule: no market-order fallback exists, so there is nothing
    // in `placed` that is not a limit. `LimitLeg` makes this structural, and
    // the assertion documents it.
    assert_eq!(ex.placed.lock().unwrap().len(), 5);
}

#[tokio::test]
async fn order_link_ids_are_derived_from_the_bar_so_a_retry_cannot_double_up() {
    // The Python used int(time.time()), so a retry inside the same bar minted
    // a fresh id and Bybit had no way to deduplicate it.
    let ex = FaultExchange::new();
    ex.script("AAVEUSDT", vec![LegAction::Fills { price: dec!(100.10) }]);
    ex.script("ETHUSDT", vec![LegAction::Fills { price: dec!(99.90) }]);
    open_pair(
        &ex,
        &cfg(),
        &params(),
        (&quote("AAVEUSDT"), &quote("ETHUSDT")),
        PairSide::LongSpread,
        dec!(1000),
        1_700_000_000_000,
    )
    .await
    .unwrap();

    let placed = ex.placed.lock().unwrap();
    assert_eq!(placed[0].order_link_id, "aave_eth-a-1700000000000");
    assert_eq!(placed[1].order_link_id, "aave_eth-b-1700000000000");
    assert!(placed.iter().all(|o| o.order_link_id.len() <= 36));
}
```

- [ ] **Step 4: Run the tests to verify they fail**

Run: `cargo test -p pairs --test entry_behaviour`
Expected: FAIL — `unresolved import pairs::executor`.

- [ ] **Step 5: Write the executor**

`crates/pairs/src/executor.rs`:

```rust
use std::time::Duration;

use botcore::{Instrument, LimitLeg, OrderStatus, Side, Symbol};
use exchange::ExchangeClient;
use exchange::bybit::transport::ExchangeError;
use rust_decimal::Decimal;
use tracing::{error, info, warn};

use crate::pricing::{aggressive_limit_price, sized_qty};
use crate::settle::{Leg, LegReport, Settlement, settle};
use crate::signal::{PairParams, PairSide};

/// Top of book plus the trading constraints for one leg.
///
/// Passed in rather than fetched inside the executor so the supervisor can
/// fetch once per bar and share across pairs, and so tests can drive the
/// executor without a clock.
#[derive(Debug, Clone)]
pub struct LegQuote {
    pub instrument: Instrument,
    pub bid: Decimal,
    pub ask: Decimal,
    pub last: Decimal,
}

#[derive(Debug, Clone)]
pub struct ExecutorConfig {
    /// How far through the book an entry or exit leg is priced.
    pub ticks_through: Decimal,
    /// How long to wait for a leg to reach a terminal state before cancelling.
    pub fill_timeout: Duration,
    pub poll_interval: Duration,
    /// Ticks-through for each successive unwind attempt. Its length is the
    /// attempt limit; an empty ladder disables unwinding and is a config error.
    pub unwind_ladder: Vec<Decimal>,
}

#[derive(Debug, thiserror::Error)]
pub enum ExecutionError {
    #[error("exchange error: {0}")]
    Exchange(#[from] ExchangeError),

    /// Exposure could not be flattened within the ladder.
    ///
    /// The caller must halt this pair and alert a human. It must **not**
    /// retry blindly and must **not** fall back to a market order: the
    /// limit-only rule holds on the unwind path exactly as it does everywhere
    /// else, and the agreed mitigation for a limit that cannot fill is
    /// escalation plus a person.
    #[error("could not flatten {} stranded leg(s); halting", legs.len())]
    UnwindExhausted { legs: Vec<LegReport> },
}

/// A pair position that actually exists on the exchange.
#[derive(Debug, Clone)]
pub struct OpenedPair {
    pub side: PairSide,
    pub a: LegReport,
    pub b: LegReport,
    pub opened_at_ms: i64,
}

/// Which way each leg trades for a given spread side.
fn leg_sides(side: PairSide) -> (Side, Side) {
    match side {
        PairSide::LongSpread => (Side::Buy, Side::Sell),
        PairSide::ShortSpread => (Side::Sell, Side::Buy),
    }
}

/// Deterministic, bar-derived order id.
///
/// Derived from the bar rather than the wall clock so a retry within the same
/// bar reuses the id and Bybit deduplicates it. The Python used
/// `int(time.time())`, which minted a fresh id per retry and made a duplicate
/// position possible. The readable `slug` prefix is kept because the dashboard
/// attributes orders by it.
fn leg_link_id(params: &PairParams, tag: &str, bar_ms: i64) -> String {
    format!("{}-{tag}-{bar_ms}", params.slug())
}

/// Open both legs, or leave the account flat.
///
/// Returns `Ok(Some(_))` only when both legs filled in full. Every other path
/// either provably flattens whatever executed or returns
/// [`ExecutionError::UnwindExhausted`]. There is no third outcome, and that
/// totality is the point of this function.
pub async fn open_pair(
    client: &dyn ExchangeClient,
    cfg: &ExecutorConfig,
    params: &PairParams,
    quotes: (&LegQuote, &LegQuote),
    side: PairSide,
    notional: Decimal,
    bar_ms: i64,
) -> Result<Option<OpenedPair>, ExecutionError> {
    let (aq, bq) = quotes;
    let (a_side, b_side) = leg_sides(side);

    let a_price = aggressive_limit_price(
        a_side,
        aq.bid,
        aq.ask,
        aq.instrument.tick_size,
        cfg.ticks_through,
    );
    let b_price = aggressive_limit_price(
        b_side,
        bq.bid,
        bq.ask,
        bq.instrument.tick_size,
        cfg.ticks_through,
    );
    let a_qty = sized_qty(notional, aq.last, &aq.instrument);
    let b_qty = sized_qty(notional, bq.last, &bq.instrument);

    let a_leg = LimitLeg {
        symbol: params.leg_a.clone(),
        side: a_side,
        qty: a_qty,
        price: a_price,
        order_link_id: leg_link_id(params, "a", bar_ms),
        reduce_only: false,
    };
    let b_leg = LimitLeg {
        symbol: params.leg_b.clone(),
        side: b_side,
        qty: b_qty,
        price: b_price,
        order_link_id: leg_link_id(params, "b", bar_ms),
        reduce_only: false,
    };

    info!(
        pair = %params.display_pair(),
        side = side.as_str(),
        notional = %notional,
        a_qty = %a_qty,
        b_qty = %b_qty,
        "placing pair entry"
    );

    // Both legs go out together: sequencing them widens the window in which
    // the spread can move between fills, and it would not remove the unwind
    // path anyway (a second leg can be rejected regardless of ordering).
    // `join!` rather than `try_join!` because a failure on one leg must not
    // abandon the other — the whole problem is knowing what happened to both.
    let (a_res, b_res) = tokio::join!(
        client.place_limit_leg(a_leg.clone()),
        client.place_limit_leg(b_leg.clone())
    );
    if let Err(e) = &a_res {
        warn!(leg = "a", error = %e, "leg placement returned an error; querying for the truth");
    }
    if let Err(e) = &b_res {
        warn!(leg = "b", error = %e, "leg placement returned an error; querying for the truth");
    }

    // The placement result is never trusted. A call that returned `Err` may
    // still have reached the exchange, so the exchange is asked what exists.
    let (a_report, b_report) = tokio::join!(
        resolve_leg(client, cfg, Leg::A, &a_leg),
        resolve_leg(client, cfg, Leg::B, &b_leg)
    );
    let a_report = a_report?;
    let b_report = b_report?;

    match settle(a_report, b_report) {
        Settlement::Opened { a, b } => {
            info!(
                pair = %params.display_pair(),
                a_price = %a.avg_price,
                b_price = %b.avg_price,
                "pair entry filled"
            );
            Ok(Some(OpenedPair {
                side,
                a,
                b,
                opened_at_ms: bar_ms,
            }))
        }
        Settlement::Flat => {
            info!(pair = %params.display_pair(), "pair entry did not fill; account is flat");
            Ok(None)
        }
        Settlement::Unwind(legs) => {
            warn!(
                pair = %params.display_pair(),
                legs = legs.len(),
                "pair entry left exposure; unwinding"
            );
            unwind_legs(client, cfg, params, &legs, bar_ms, quotes, "uw").await?;
            Ok(None)
        }
    }
}

/// Poll one leg to a terminal state, cancelling it if it overruns, and report
/// what it actually executed.
async fn resolve_leg(
    client: &dyn ExchangeClient,
    cfg: &ExecutorConfig,
    leg: Leg,
    order: &LimitLeg,
) -> Result<LegReport, ExecutionError> {
    let status = poll_to_terminal(client, cfg, &order.symbol, &order.order_link_id).await?;

    let status = match status {
        Some(s) if s.state.is_terminal() => Some(s),
        Some(_) => {
            // Still working past the deadline. Cancel, then re-read: the
            // cancel races the fill, and only the re-read knows which won.
            client
                .cancel_order(&order.symbol, &order.order_link_id)
                .await?;
            client
                .order_by_link_id(&order.symbol, &order.order_link_id)
                .await?
        }
        None => None,
    };

    Ok(build_report(leg, order, status.as_ref()))
}

fn build_report(leg: Leg, order: &LimitLeg, status: Option<&OrderStatus>) -> LegReport {
    LegReport {
        leg,
        symbol: order.symbol.clone(),
        side: order.side,
        requested_qty: order.qty,
        executed_qty: status.map(|s| s.cum_exec_qty).unwrap_or(Decimal::ZERO),
        avg_price: status.map(|s| s.avg_price).unwrap_or(Decimal::ZERO),
        order_id: status.map(|s| s.order_id.clone()).unwrap_or_default(),
        state: status.map(|s| s.state),
    }
}

/// Poll until the order reaches a terminal state or `fill_timeout` elapses.
///
/// Returns the last status seen, terminal or not, so the caller can tell
/// "still working" from "never existed".
pub async fn poll_to_terminal(
    client: &dyn ExchangeClient,
    cfg: &ExecutorConfig,
    symbol: &Symbol,
    link_id: &str,
) -> Result<Option<OrderStatus>, ExecutionError> {
    let deadline = tokio::time::Instant::now() + cfg.fill_timeout;
    loop {
        let status = client.order_by_link_id(symbol, link_id).await?;
        if let Some(s) = &status
            && s.state.is_terminal()
        {
            return Ok(status);
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(status);
        }
        tokio::time::sleep(cfg.poll_interval).await;
    }
}

/// Flatten every listed leg with reduce-only limits, escalating the price
/// through the book on each attempt.
///
/// **There is deliberately no market-order fallback.** The limit-only rule
/// holds here exactly as everywhere else; when the ladder is exhausted this
/// returns [`ExecutionError::UnwindExhausted`] so a human is brought in.
///
/// `tag` distinguishes an entry unwind (`"uw"`) from a close (`"cl"`) in the
/// `orderLinkId`, so the two are attributable apart in Bybit's UI.
pub async fn unwind_legs(
    client: &dyn ExchangeClient,
    cfg: &ExecutorConfig,
    params: &PairParams,
    legs: &[LegReport],
    bar_ms: i64,
    quotes: (&LegQuote, &LegQuote),
    tag: &str,
) -> Result<(), ExecutionError> {
    let mut stranded = Vec::new();

    for leg in legs {
        let mut flattened = false;
        for (attempt, ticks) in cfg.unwind_ladder.iter().enumerate() {
            // Re-quote each attempt: a rung that failed did so because the
            // book moved, and pricing the next rung off a stale quote is how a
            // ladder walks in the wrong direction.
            let t = client.ticker(&leg.symbol).await?;
            let side = leg.side.opposite();
            // Tick size comes from the quote already in hand. Re-reading
            // `instruments()` here would pull all 800-odd linear symbols on a
            // path that is already going badly.
            let (aq, bq) = quotes;
            let tick = if leg.symbol == aq.instrument.symbol {
                aq.instrument.tick_size
            } else {
                bq.instrument.tick_size
            };
            let price = aggressive_limit_price(side, t.bid1, t.ask1, tick, *ticks);
            let link_id = format!("{}-{tag}{attempt}-{bar_ms}", params.slug());

            warn!(
                symbol = %leg.symbol,
                attempt,
                ticks_through = %ticks,
                price = %price,
                qty = %leg.executed_qty,
                "unwinding stranded leg with a reduce-only limit"
            );

            let order = LimitLeg {
                symbol: leg.symbol.clone(),
                side,
                qty: leg.executed_qty,
                price,
                order_link_id: link_id,
                reduce_only: true,
            };
            // An error here is not fatal on its own — the next rung may work,
            // and the query below establishes what actually happened.
            let _ = client.place_limit_leg(order.clone()).await;
            let report = resolve_leg(client, cfg, leg.leg, &order).await?;
            if report.is_fully_filled() {
                flattened = true;
                break;
            }
        }
        if !flattened {
            error!(
                symbol = %leg.symbol,
                qty = %leg.executed_qty,
                "unwind ladder exhausted; leg is still open and needs a human"
            );
            stranded.push(leg.clone());
        }
    }

    if stranded.is_empty() {
        Ok(())
    } else {
        Err(ExecutionError::UnwindExhausted { legs: stranded })
    }
}
```

Add to `crates/pairs/src/lib.rs`:

```rust
pub mod executor;

pub use executor::{ExecutionError, ExecutorConfig, LegQuote, OpenedPair, open_pair};
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p pairs`
Expected: PASS — 8 tests in `entry_behaviour`, 41 unit tests.

- [ ] **Step 7: Lint and commit**

```bash
cargo clippy --workspace --all-targets -- -D warnings
git add crates/pairs crates/exchange crates/engine crates/backtest
git commit -m "feat(pairs): two-leg entry that cannot leave a naked leg behind"
```

---
### Task 7: `crates/pairs` — a close that converges

**Files:**
- Modify: `crates/pairs/src/executor.rs` (add `close_pair`)
- Modify: `crates/pairs/tests/support/mod.rs` (make `positions()` scriptable)
- Create: `crates/pairs/tests/close_behaviour.rs`

**Interfaces:**
- Consumes: everything from Task 6.
- Produces: `pairs::executor::close_pair(client, cfg, params, quotes: (&LegQuote, &LegQuote), position: &OpenedPair, bar_ms: i64) -> Result<(), ExecutionError>`

**Context the implementer needs:** [`close_pair_position`, `scripts/pairs_bot.py:840-869`](../../../scripts/pairs_bot.py) sends a reduce-only order on **both** legs unconditionally, then raises if either did not fill — *before* clearing `state.position`. So when leg A's close fills and leg B's does not, the next bar retries both: leg A is already flat, Bybit rejects a reduce-only order against no position, the exception propagates, and the loop never converges. The position stays in the state file forever while the account holds only one leg.

The fix has two halves, and both are required:

1. **Read positions first, and only send orders for legs that actually carry size.** A leg already flat is a no-op, not an error. This is what makes a retry converge instead of failing on the half that already worked.
2. **Escalate the residual through the same unwind ladder** rather than raising. A close that will not fill at 5 ticks through is the same problem as a stranded entry leg, and it gets the same answer — wider limits, then a human. Still no market order.

- [ ] **Step 1: Make `positions()` scriptable on the double**

In `crates/pairs/tests/support/mod.rs`, add a field and a setter, and replace the `positions` stub:

```rust
    /// What the exchange reports as open, independent of the order log — this
    /// is how a test stages "leg A is already flat and leg B is not".
    positions: Mutex<Vec<Position>>,
```

```rust
    pub fn position(&self, symbol: &str, side: Side, size: Decimal) -> &Self {
        self.positions.lock().unwrap().push(Position {
            symbol: Symbol::new(symbol),
            side,
            size,
            entry_price: dec!(100),
            liq_price: None,
            unrealized_pnl: dec!(0),
        });
        self
    }
```

```rust
    async fn positions(&self) -> Result<Vec<Position>, ExchangeError> {
        Ok(self.positions.lock().unwrap().clone())
    }
```

- [ ] **Step 2: Write the failing close tests**

`crates/pairs/tests/close_behaviour.rs` (reuse the `instrument`, `quote`, `params` and `cfg` helpers from `entry_behaviour.rs` by copying them in — these are test fixtures, and a shared module that every test file must agree on is worse than four short duplicated functions):

```rust
mod support;

use botcore::{Instrument, Side, Symbol, Timeframe};
use pairs::executor::{ExecutionError, ExecutorConfig, LegQuote, OpenedPair, close_pair};
use pairs::settle::{Leg, LegReport};
use pairs::{PairParams, PairSide};
use rust_decimal_macros::dec;
use support::{FaultExchange, LegAction};

// ... instrument(), quote(), params(), cfg() exactly as in entry_behaviour.rs ...

fn open_position() -> OpenedPair {
    OpenedPair {
        side: PairSide::LongSpread,
        a: LegReport {
            leg: Leg::A,
            symbol: Symbol::new("AAVEUSDT"),
            side: Side::Buy,
            requested_qty: dec!(1),
            executed_qty: dec!(1),
            avg_price: dec!(100.10),
            order_id: "oid-a".into(),
            state: Some(botcore::OrderState::Filled),
        },
        b: LegReport {
            leg: Leg::B,
            symbol: Symbol::new("ETHUSDT"),
            side: Side::Sell,
            requested_qty: dec!(2),
            executed_qty: dec!(2),
            avg_price: dec!(99.90),
            order_id: "oid-b".into(),
            state: Some(botcore::OrderState::Filled),
        },
        opened_at_ms: 1_700_000_000_000,
    }
}

#[tokio::test]
async fn both_open_legs_are_closed_with_reduce_only_orders() {
    let ex = FaultExchange::new();
    ex.position("AAVEUSDT", Side::Buy, dec!(1));
    ex.position("ETHUSDT", Side::Sell, dec!(2));
    ex.script("AAVEUSDT", vec![LegAction::Fills { price: dec!(99.85) }]);
    ex.script("ETHUSDT", vec![LegAction::Fills { price: dec!(100.15) }]);

    close_pair(
        &ex,
        &cfg(),
        &params(),
        (&quote("AAVEUSDT"), &quote("ETHUSDT")),
        &open_position(),
        1_700_003_600_000,
    )
    .await
    .expect("both legs closed");

    let placed = ex.placed.lock().unwrap();
    assert_eq!(placed.len(), 2);
    assert!(placed.iter().all(|o| o.reduce_only));
    // Closing a long leg sells; closing a short leg buys.
    assert_eq!(placed[0].side, Side::Sell);
    assert_eq!(placed[1].side, Side::Buy);
}

#[tokio::test]
async fn a_half_closed_pair_converges_on_the_next_attempt() {
    // The Python's non-converging close, asserted against directly. Leg A is
    // already flat from a previous attempt; retrying must close only leg B and
    // succeed, not reject on a reduce-only order against no position.
    let ex = FaultExchange::new();
    ex.position("ETHUSDT", Side::Sell, dec!(2));
    ex.script("ETHUSDT", vec![LegAction::Fills { price: dec!(100.15) }]);

    close_pair(
        &ex,
        &cfg(),
        &params(),
        (&quote("AAVEUSDT"), &quote("ETHUSDT")),
        &open_position(),
        1_700_003_600_000,
    )
    .await
    .expect("closing the remaining leg succeeds");

    let placed = ex.placed.lock().unwrap();
    assert_eq!(placed.len(), 1, "only the leg with size may be ordered");
    assert_eq!(placed[0].symbol.as_str(), "ETHUSDT");
}

#[tokio::test]
async fn a_pair_already_flat_on_both_legs_closes_without_sending_anything() {
    let ex = FaultExchange::new();

    close_pair(
        &ex,
        &cfg(),
        &params(),
        (&quote("AAVEUSDT"), &quote("ETHUSDT")),
        &open_position(),
        1_700_003_600_000,
    )
    .await
    .expect("nothing to do is success");

    assert!(ex.placed.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_close_that_will_not_fill_escalates_through_the_ladder() {
    let ex = FaultExchange::new();
    ex.position("AAVEUSDT", Side::Buy, dec!(1));
    ex.script(
        "AAVEUSDT",
        vec![
            LegAction::Rests,
            LegAction::Rests,
            LegAction::Fills { price: dec!(99.30) },
        ],
    );

    close_pair(
        &ex,
        &cfg(),
        &params(),
        (&quote("AAVEUSDT"), &quote("ETHUSDT")),
        &open_position(),
        1_700_003_600_000,
    )
    .await
    .expect("the third rung filled");

    let placed = ex.placed.lock().unwrap();
    assert!(placed.len() >= 3);
    assert!(placed.iter().all(|o| o.reduce_only));
    assert!(
        placed[0].price > placed[placed.len() - 1].price,
        "each rung must price further through the book"
    );
}

#[tokio::test]
async fn a_close_that_exhausts_the_ladder_reports_the_stranded_leg() {
    let ex = FaultExchange::new();
    ex.position("AAVEUSDT", Side::Buy, dec!(1));
    ex.script(
        "AAVEUSDT",
        vec![LegAction::Rests, LegAction::Rests, LegAction::Rests, LegAction::Rests],
    );

    let err = close_pair(
        &ex,
        &cfg(),
        &params(),
        (&quote("AAVEUSDT"), &quote("ETHUSDT")),
        &open_position(),
        1_700_003_600_000,
    )
    .await
    .expect_err("a leg that will not close must surface, not be swallowed");

    assert!(matches!(err, ExecutionError::UnwindExhausted { .. }));
}
```

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test -p pairs --test close_behaviour`
Expected: FAIL — `cannot find function close_pair`.

- [ ] **Step 4: Write `close_pair`**

Append to `crates/pairs/src/executor.rs`:

```rust
/// Close both legs of an open pair, tolerating either being flat already.
///
/// Reads the exchange's positions first and orders only the legs that actually
/// carry size. That is what makes a retry converge: the Python sent reduce-only
/// orders on both legs unconditionally, so a second attempt after a half-close
/// was rejected on the leg that had already worked, and the position never
/// cleared.
///
/// A residual that will not fill at the base price escalates through the same
/// ladder an entry unwind uses, for the same reason and with the same
/// limit-only rule: wider limits, then a human, never a market order.
///
/// The caller must clear its stored position only on `Ok(())`.
pub async fn close_pair(
    client: &dyn ExchangeClient,
    cfg: &ExecutorConfig,
    params: &PairParams,
    quotes: (&LegQuote, &LegQuote),
    position: &OpenedPair,
    bar_ms: i64,
) -> Result<(), ExecutionError> {
    let open: std::collections::HashMap<Symbol, Decimal> = client
        .positions()
        .await?
        .into_iter()
        .map(|p| (p.symbol, p.size))
        .collect();

    let mut to_close = Vec::new();
    for leg in [&position.a, &position.b] {
        match open.get(&leg.symbol) {
            Some(size) if *size > Decimal::ZERO => {
                // Close what the exchange says is there, not what the journal
                // remembers. A partial close from an earlier attempt makes
                // those two numbers differ, and the exchange is the one that
                // can reject an order.
                let mut leg = leg.clone();
                leg.executed_qty = *size;
                to_close.push(leg);
            }
            _ => {
                info!(
                    symbol = %leg.symbol,
                    "leg is already flat; skipping its close order"
                );
            }
        }
    }

    if to_close.is_empty() {
        info!(pair = %params.display_pair(), "pair is already flat on both legs");
        return Ok(());
    }

    info!(
        pair = %params.display_pair(),
        legs = to_close.len(),
        "closing pair position"
    );
    unwind_legs(client, cfg, params, &to_close, bar_ms, quotes, "cl").await
}
```

`unwind_legs` already takes `quotes` and a `tag` from Task 6, so a close reuses it unchanged — the `"cl"` tag is what keeps a close's `orderLinkId` attributable apart from an entry unwind's in Bybit's UI.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p pairs`
Expected: PASS — 5 new tests in `close_behaviour`, all earlier tests still green.

- [ ] **Step 6: Lint and commit**

```bash
cargo clippy --workspace --all-targets -- -D warnings
git add crates/pairs
git commit -m "feat(pairs): position-aware close that converges after a partial"
```

---

### Task 8: Durable pair state in the journal

**Files:**
- Modify: `crates/persistence/src/journal.rs` (`MIGRATIONS` + accessors)
- Create: `crates/pairs/src/state.rs`
- Create: `crates/pairs/tests/reconcile_behaviour.rs`
- Modify: `crates/pairs/Cargo.toml` (add `persistence`)

**Interfaces:**
- Consumes: `pairs::executor::OpenedPair` (Task 6), `pairs::settle::LegReport` (Task 5).
- Produces:
  - `persistence::Journal::{upsert_pair_position, pair_position, clear_pair_position, record_pair_event, upsert_pair_heartbeat, pair_heartbeat}`
  - `persistence::{PairPositionRecord, PairEvent, PairEventKind, PairHeartbeat}`
  - `pairs::state::{reconcile_pair, Reconciliation}`
    - `reconcile_pair(client: &dyn ExchangeClient, journal_position: Option<&PairPositionRecord>, params: &PairParams) -> Result<Reconciliation, ExecutionError>`
    - `Reconciliation::{Flat, Holding(PairPositionRecord), Halt { reason: String }}`

**Context the implementer needs:** the Python persists to a per-bot JSON file with `path.write_text(json.dumps(...))` ([`scripts/pairs_bot.py:210-220`](../../../scripts/pairs_bot.py)). That is not atomic: a crash mid-write truncates the file, `RuntimeState.from_file` then raises `JSONDecodeError` on the next start, and systemd's `Restart=always` turns that into a permanent crash loop. `crates/persistence` already solves this — it is transactional, already backs `bot/tests/protection_persistence.rs`, and already syncs to Turso off the order path.

Store `Decimal` as `TEXT`. That is this journal's existing convention and `scripts/checkup.sh` calls out why: *"Decimals are TEXT: never ORDER BY them. Order by the integer timestamp."*

The reconciliation rule is the one the Python got right and must be preserved: **when the journal and the exchange disagree, halt and report — never guess.** The Python logs "exchange shows open positions but local state is empty; refusing to proceed until human review" and stops. Keep that behaviour, and extend it to the mirror case (journal holds a position the exchange does not).

- [ ] **Step 1: Write the failing journal tests**

Append to `crates/persistence/src/journal.rs`'s test module:

```rust
    #[tokio::test]
    async fn a_pair_position_round_trips_through_a_reopened_journal() {
        // The property the JSON state file lacked: a process that dies between
        // writing and exiting must find its position intact on the next boot.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("j.db");
        let path = path.to_str().unwrap();

        let record = PairPositionRecord {
            bot_id: "aave_eth".into(),
            side: "long_spread".into(),
            opened_at_ms: 1_700_000_000_000,
            entry_z: dec!(-3.21),
            a_symbol: Symbol::new("AAVEUSDT"),
            a_qty: dec!(1.5),
            a_entry: dec!(300.25),
            a_order_id: "oid-a".into(),
            b_symbol: Symbol::new("ETHUSDT"),
            b_qty: dec!(0.4),
            b_entry: dec!(3000.5),
            b_order_id: "oid-b".into(),
            breakeven_armed: false,
            per_leg_notional: dec!(450),
            capped_by: Some("available_equity".into()),
        };

        {
            let j = Journal::open_local(path).await.unwrap();
            j.upsert_pair_position(&record).await.unwrap();
        }
        let j = Journal::open_local(path).await.unwrap();
        assert_eq!(j.pair_position("aave_eth").await.unwrap(), Some(record));
    }

    #[tokio::test]
    async fn upserting_a_pair_position_replaces_rather_than_duplicating() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("j.db");
        let j = Journal::open_local(path.to_str().unwrap()).await.unwrap();
        let mut r = sample_pair_position();
        j.upsert_pair_position(&r).await.unwrap();
        r.breakeven_armed = true;
        j.upsert_pair_position(&r).await.unwrap();
        assert_eq!(
            j.pair_position("aave_eth").await.unwrap().unwrap().breakeven_armed,
            true
        );
    }

    #[tokio::test]
    async fn clearing_a_pair_position_leaves_the_bot_flat() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("j.db");
        let j = Journal::open_local(path.to_str().unwrap()).await.unwrap();
        j.upsert_pair_position(&sample_pair_position()).await.unwrap();
        j.clear_pair_position("aave_eth").await.unwrap();
        assert_eq!(j.pair_position("aave_eth").await.unwrap(), None);
    }

    #[tokio::test]
    async fn two_bots_keep_separate_positions() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("j.db");
        let j = Journal::open_local(path.to_str().unwrap()).await.unwrap();
        let a = sample_pair_position();
        let mut b = sample_pair_position();
        b.bot_id = "ena_xrp".into();
        j.upsert_pair_position(&a).await.unwrap();
        j.upsert_pair_position(&b).await.unwrap();
        j.clear_pair_position("aave_eth").await.unwrap();
        assert_eq!(j.pair_position("aave_eth").await.unwrap(), None);
        assert!(j.pair_position("ena_xrp").await.unwrap().is_some());
    }

    #[tokio::test]
    async fn a_heartbeat_survives_a_restart_so_a_stalled_bot_is_visible() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("j.db");
        let path = path.to_str().unwrap();
        let hb = PairHeartbeat {
            bot_id: "aave_eth".into(),
            last_bar_ms: Some(1_700_000_000_000),
            last_loop_ms: Some(1_700_000_060_000),
            last_z: Some(dec!(-1.25)),
            last_signal: None,
            last_guard_reason: None,
        };
        {
            let j = Journal::open_local(path).await.unwrap();
            j.upsert_pair_heartbeat(&hb).await.unwrap();
        }
        let j = Journal::open_local(path).await.unwrap();
        assert_eq!(j.pair_heartbeat("aave_eth").await.unwrap(), Some(hb));
    }
```

Add the `sample_pair_position()` helper beside them, returning the same record as the first test.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p persistence`
Expected: FAIL — `cannot find type PairPositionRecord`.

- [ ] **Step 3: Add the schema and accessors**

Append to the `MIGRATIONS` array at the top of `crates/persistence/src/journal.rs`:

```rust
    // One row per bot: a pair is flat or it is not, so there is nothing to
    // accumulate. Decimals are TEXT, matching every other table here — never
    // ORDER BY them; order by the integer timestamp.
    "CREATE TABLE IF NOT EXISTS pair_positions (
        bot_id TEXT PRIMARY KEY,
        side TEXT NOT NULL,
        opened_at_ms INTEGER NOT NULL,
        entry_z TEXT NOT NULL,
        a_symbol TEXT NOT NULL,
        a_qty TEXT NOT NULL,
        a_entry TEXT NOT NULL,
        a_order_id TEXT NOT NULL,
        b_symbol TEXT NOT NULL,
        b_qty TEXT NOT NULL,
        b_entry TEXT NOT NULL,
        b_order_id TEXT NOT NULL,
        breakeven_armed INTEGER NOT NULL,
        per_leg_notional TEXT NOT NULL,
        capped_by TEXT
    )",
    // Append-only narrative: entries, exits, guard deferrals, unwinds, halts.
    "CREATE TABLE IF NOT EXISTS pair_events (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        bot_id TEXT NOT NULL,
        at_ms INTEGER NOT NULL,
        kind TEXT NOT NULL,
        detail TEXT NOT NULL
    )",
    "CREATE INDEX IF NOT EXISTS pair_events_bot_at ON pair_events (bot_id, at_ms)",
    // Liveness, replacing the JSON state file's heartbeat fields. Separate
    // from pair_positions because it is written every loop while a position
    // changes rarely, and because a heartbeat write must never risk a
    // position row.
    "CREATE TABLE IF NOT EXISTS pair_heartbeats (
        bot_id TEXT PRIMARY KEY,
        last_bar_ms INTEGER,
        last_loop_ms INTEGER,
        last_z TEXT,
        last_signal TEXT,
        last_guard_reason TEXT
    )",
```

Add the record types beside the existing `OrderRecord` / `TradeEvent` definitions, deriving `Debug, Clone, PartialEq, Eq`, and write the six accessors following the exact shape of `upsert_protection` / `load_protections` / `delete_protection` already in the file: `INSERT OR REPLACE` for the upserts, `parse_dec` for every `TEXT` decimal, `JournalError::Decode` for anything unparseable. `PairEventKind` gets `as_str` and a `parse` mirroring `TradeEventKind`, with variants `Entry`, `Exit`, `EntrySkipped`, `Unwound`, `UnwindFailed`, `Reconciled`, `Halted`.

- [ ] **Step 4: Run the journal tests**

Run: `cargo test -p persistence`
Expected: PASS.

- [ ] **Step 5: Write the failing reconciliation tests**

`crates/pairs/tests/reconcile_behaviour.rs`:

```rust
mod support;

use botcore::Side;
use pairs::state::{Reconciliation, reconcile_pair};
use rust_decimal_macros::dec;
use support::FaultExchange;

// ... params() as in entry_behaviour.rs, and a sample_record() building a
// PairPositionRecord for AAVEUSDT/ETHUSDT ...

#[tokio::test]
async fn a_flat_journal_and_a_flat_exchange_agree() {
    let ex = FaultExchange::new();
    let got = reconcile_pair(&ex, None, &params()).await.unwrap();
    assert_eq!(got, Reconciliation::Flat);
}

#[tokio::test]
async fn a_journalled_position_matching_the_exchange_is_adopted() {
    let ex = FaultExchange::new();
    ex.position("AAVEUSDT", Side::Buy, dec!(1.5));
    ex.position("ETHUSDT", Side::Sell, dec!(0.4));
    let record = sample_record();
    let got = reconcile_pair(&ex, Some(&record), &params()).await.unwrap();
    assert_eq!(got, Reconciliation::Holding(record));
}

#[tokio::test]
async fn exchange_positions_with_an_empty_journal_halt_for_human_review() {
    // The Python's one genuinely good safety behaviour, kept verbatim: it
    // refuses to trade rather than guessing what the open position means.
    let ex = FaultExchange::new();
    ex.position("AAVEUSDT", Side::Buy, dec!(1.5));
    match reconcile_pair(&ex, None, &params()).await.unwrap() {
        Reconciliation::Halt { reason } => {
            assert!(reason.contains("AAVEUSDT"), "reason was {reason}");
        }
        other => panic!("expected Halt, got {other:?}"),
    }
}

#[tokio::test]
async fn a_journalled_position_the_exchange_does_not_have_halts_too() {
    // The mirror case the Python never checked: the journal believes it holds
    // a pair that was liquidated or closed by hand. Trading on that belief
    // would send reduce-only orders against nothing, forever.
    let ex = FaultExchange::new();
    match reconcile_pair(&ex, Some(&sample_record()), &params()).await.unwrap() {
        Reconciliation::Halt { reason } => assert!(reason.contains("journal")),
        other => panic!("expected Halt, got {other:?}"),
    }
}

#[tokio::test]
async fn only_one_of_the_two_legs_being_open_halts() {
    let ex = FaultExchange::new();
    ex.position("AAVEUSDT", Side::Buy, dec!(1.5));
    match reconcile_pair(&ex, Some(&sample_record()), &params()).await.unwrap() {
        Reconciliation::Halt { .. } => {}
        other => panic!("expected Halt, got {other:?}"),
    }
}

#[tokio::test]
async fn positions_on_other_bots_symbols_are_ignored() {
    // Three pairs share one account. A position on someone else's symbol is
    // not this bot's business and must not halt it.
    let ex = FaultExchange::new();
    ex.position("BNBUSDT", Side::Buy, dec!(5));
    assert_eq!(
        reconcile_pair(&ex, None, &params()).await.unwrap(),
        Reconciliation::Flat
    );
}
```

- [ ] **Step 6: Write `reconcile_pair`**

`crates/pairs/src/state.rs`:

```rust
use exchange::ExchangeClient;
use persistence::PairPositionRecord;
use rust_decimal::Decimal;

use crate::executor::ExecutionError;
use crate::signal::PairParams;

/// What the journal and the exchange, taken together, say this bot is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reconciliation {
    Flat,
    Holding(PairPositionRecord),
    /// The two sources disagree. Trading must stop for this pair until a human
    /// looks. Guessing which one is right is how a bot compounds a mistake it
    /// did not make.
    Halt { reason: String },
}

/// Compare the journal against the exchange before any strategy evaluation.
///
/// The exchange is the source of truth about what exists; the journal is the
/// only source of truth about *why*. When they disagree neither can be
/// reconstructed from the other, so this halts and reports rather than
/// choosing. That is the Python's one genuinely good safety behaviour
/// ("refusing to proceed until human review"), kept — and extended to the
/// mirror case it never checked, where the journal holds a pair the exchange
/// has already closed.
pub async fn reconcile_pair(
    client: &dyn ExchangeClient,
    journal_position: Option<&PairPositionRecord>,
    params: &PairParams,
) -> Result<Reconciliation, ExecutionError> {
    let ours: Vec<_> = client
        .positions()
        .await?
        .into_iter()
        .filter(|p| p.symbol == params.leg_a || p.symbol == params.leg_b)
        .filter(|p| p.size > Decimal::ZERO)
        .collect();

    match (journal_position, ours.len()) {
        (None, 0) => Ok(Reconciliation::Flat),
        (Some(rec), 2) => Ok(Reconciliation::Holding(rec.clone())),
        (None, _) => Ok(Reconciliation::Halt {
            reason: format!(
                "exchange holds {} on {} with no journalled pair position; refusing to trade until reviewed",
                ours.len(),
                ours.iter()
                    .map(|p| p.symbol.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }),
        (Some(_), n) => Ok(Reconciliation::Halt {
            reason: format!(
                "journal holds a pair position but the exchange shows {n} of 2 legs open on {}; refusing to trade until reviewed",
                params.display_pair()
            ),
        }),
    }
}
```

Add `pub mod state;` to `crates/pairs/src/lib.rs` and `persistence = { path = "../persistence" }` to its `[dependencies]`.

- [ ] **Step 7: Run everything**

Run: `cargo test --workspace`
Expected: PASS.

- [ ] **Step 8: Lint and commit**

```bash
cargo clippy --workspace --all-targets -- -D warnings
git add crates/persistence crates/pairs
git commit -m "feat(pairs): durable pair state and boot reconciliation"
```

---
### Task 9: Config and a priority guard that actually fires

**Files:**
- Create: `crates/pairs/src/supervisor.rs` (`PortfolioGuard` only; the loop is Task 10)
- Create: `bot/src/pairs_config.rs`
- Create: `config/pairs-testnet.toml`
- Create: `config/pairs-mainnet.toml`
- Create: `bot/tests/pairs_config_matches_portfolio.rs`
- Modify: `bot/src/lib.rs`, `bot/Cargo.toml`

**Interfaces:**
- Consumes: `pairs::signal::{PairParams, PairSide}` (Task 2).
- Produces:
  - `pairs::supervisor::{BotSnapshot, PortfolioGuard}`
    - `PortfolioGuard::update(&mut self, snapshot: BotSnapshot)`
    - `PortfolioGuard::defer_reason(&self, bot_id: &str, signal: PairSide, latest_bar_ms: i64) -> Option<String>`
  - `bot::pairs_config::{PairsConfig, BotConfig, ExecutorSettings, RuntimeSettings, load_pairs_config}`
    - `load_pairs_config(profile: Profile) -> Result<PairsConfig, ConfigError>`
    - `BotConfig::{id, name, priority, params: PairParams, symbols() -> HashSet<Symbol>}`

**Context the implementer needs:** two problems, one task.

**The guard is currently dead code.** `BOT_PROFILES[*]['higher_priority_peers']` is built as `[]` in [`scripts/pairs_bot.py:641-651`](../../../scripts/pairs_bot.py) and never populated, so `should_defer_to_higher_priority` cannot fire in production — verified by inspection: `{'aave_eth': [], 'ena_xrp': [], 'bnb_xaut': []}`. Its unit tests pass because they hand-build peer dicts and call the pure function directly. Running all pairs in one process makes peer state an in-process structure rather than three processes trying to read each other's files, which is what makes the guard real. Today's portfolio is symbol-disjoint so the guard is inert either way — which is exactly why it must be correct *before* an overlapping pair is added.

**The configuration lives in argv.** Each systemd unit carries a ~400-character `ExecStart` line duplicating the parameters already hardcoded in `ACTIVE_BOT_PROFILES`, so there are two sources of truth for every threshold. The repo already has the right pattern in `bot/src/config.rs` + `config/testnet.toml`, including the `BYBIT_ALLOW_MAINNET` second gate. Follow it.

- [ ] **Step 1: Write the failing guard tests**

`crates/pairs/src/supervisor.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use botcore::Symbol;

    fn snapshot(id: &str, priority: u32, symbols: &[&str]) -> BotSnapshot {
        BotSnapshot {
            bot_id: id.into(),
            display_name: id.to_uppercase(),
            priority,
            symbols: symbols.iter().map(|s| Symbol::new(*s)).collect(),
            has_position: false,
            latest_bar_ms: Some(100),
            signal: None,
        }
    }

    fn guard(snapshots: Vec<BotSnapshot>) -> PortfolioGuard {
        let mut g = PortfolioGuard::default();
        for s in snapshots {
            g.update(s);
        }
        g
    }

    #[test]
    fn a_lower_priority_bot_defers_to_a_higher_one_holding_a_shared_symbol() {
        let mut high = snapshot("doge_xrp", 100, &["DOGEUSDT", "XRPUSDT"]);
        high.has_position = true;
        let g = guard(vec![high, snapshot("link_xrp", 90, &["LINKUSDT", "XRPUSDT"])]);

        let reason = g
            .defer_reason("link_xrp", PairSide::ShortSpread, 100)
            .expect("must defer");
        assert!(reason.contains("DOGE_XRP"), "reason was {reason}");
        assert!(reason.contains("position"), "reason was {reason}");
    }

    #[test]
    fn a_lower_priority_bot_defers_to_a_higher_ones_signal_on_the_same_bar() {
        let mut high = snapshot("doge_xrp", 100, &["DOGEUSDT", "XRPUSDT"]);
        high.signal = Some(PairSide::ShortSpread);
        let g = guard(vec![high, snapshot("link_xrp", 90, &["LINKUSDT", "XRPUSDT"])]);

        let reason = g
            .defer_reason("link_xrp", PairSide::LongSpread, 100)
            .expect("must defer");
        assert!(reason.contains("same bar"), "reason was {reason}");
    }

    #[test]
    fn a_stale_peer_signal_from_an_earlier_bar_does_not_block() {
        // Deferring on a signal the peer had an hour ago would stall this bot
        // indefinitely whenever the peer's feed lags.
        let mut high = snapshot("doge_xrp", 100, &["DOGEUSDT", "XRPUSDT"]);
        high.signal = Some(PairSide::ShortSpread);
        high.latest_bar_ms = Some(99);
        let g = guard(vec![high, snapshot("link_xrp", 90, &["LINKUSDT", "XRPUSDT"])]);
        assert_eq!(g.defer_reason("link_xrp", PairSide::LongSpread, 100), None);
    }

    #[test]
    fn bots_that_share_no_symbol_never_block_each_other() {
        let mut high = snapshot("aave_eth", 100, &["AAVEUSDT", "ETHUSDT"]);
        high.has_position = true;
        high.signal = Some(PairSide::ShortSpread);
        let g = guard(vec![high, snapshot("bnb_xaut", 80, &["BNBUSDT", "XAUTUSDT"])]);
        assert_eq!(g.defer_reason("bnb_xaut", PairSide::LongSpread, 100), None);
    }

    #[test]
    fn a_higher_priority_bot_never_defers_to_a_lower_one() {
        let mut low = snapshot("bnb_xaut", 80, &["XRPUSDT", "XAUTUSDT"]);
        low.has_position = true;
        let g = guard(vec![snapshot("aave_eth", 100, &["AAVEUSDT", "XRPUSDT"]), low]);
        assert_eq!(g.defer_reason("aave_eth", PairSide::LongSpread, 100), None);
    }

    #[test]
    fn equal_priorities_do_not_block_each_other() {
        // Strictly-greater, not greater-or-equal: two bots at the same priority
        // sharing a symbol would otherwise each wait for the other forever.
        let mut peer = snapshot("b", 100, &["XRPUSDT", "SOLUSDT"]);
        peer.has_position = true;
        let g = guard(vec![snapshot("a", 100, &["AAVEUSDT", "XRPUSDT"]), peer]);
        assert_eq!(g.defer_reason("a", PairSide::LongSpread, 100), None);
    }

    #[test]
    fn an_unknown_bot_id_does_not_defer() {
        let g = guard(vec![snapshot("a", 100, &["AAVEUSDT", "ETHUSDT"])]);
        assert_eq!(g.defer_reason("nobody", PairSide::LongSpread, 100), None);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p pairs supervisor`
Expected: FAIL — `cannot find type PortfolioGuard`.

- [ ] **Step 3: Write `PortfolioGuard`**

Above the test module in `crates/pairs/src/supervisor.rs`:

```rust
use std::collections::{HashMap, HashSet};

use botcore::Symbol;

use crate::signal::PairSide;

/// One bot's current view of itself, as its peers need to see it.
#[derive(Debug, Clone)]
pub struct BotSnapshot {
    pub bot_id: String,
    pub display_name: String,
    /// Higher wins. Strictly greater, so equal priorities never block each
    /// other — two bots waiting on each other is a deadlock, not a guard.
    pub priority: u32,
    pub symbols: HashSet<Symbol>,
    pub has_position: bool,
    pub latest_bar_ms: Option<i64>,
    pub signal: Option<PairSide>,
}

/// Cross-bot arbitration for symbols more than one pair trades.
///
/// Bybit nets positions by symbol, so two bots trading the same symbol on one
/// account interfere: one bot's entry can reduce or reverse the other's leg
/// without either knowing. The guard makes the higher-priority bot win.
///
/// The Python had this logic and could never run it — its peer list was built
/// empty and never populated, so the check was dead code in production while
/// its unit tests passed against hand-built inputs. Running every pair in one
/// process is what makes peer state observable, and therefore what makes this
/// real.
///
/// Today's portfolio is symbol-disjoint, so nothing here fires. That is the
/// reason to get it right now rather than when an overlapping pair is added.
#[derive(Debug, Default)]
pub struct PortfolioGuard {
    snapshots: HashMap<String, BotSnapshot>,
}

impl PortfolioGuard {
    pub fn update(&mut self, snapshot: BotSnapshot) {
        self.snapshots.insert(snapshot.bot_id.clone(), snapshot);
    }

    /// Why `bot_id` should skip acting on `signal` this bar, if it should.
    ///
    /// Only a *strictly* higher-priority peer sharing at least one symbol can
    /// block, and only for a position it holds now or a signal it has on this
    /// same bar. A peer's stale signal from an earlier bar does not block:
    /// deferring to it would stall this bot for as long as the peer's feed
    /// lags.
    pub fn defer_reason(
        &self,
        bot_id: &str,
        _signal: PairSide,
        latest_bar_ms: i64,
    ) -> Option<String> {
        let me = self.snapshots.get(bot_id)?;
        for peer in self.snapshots.values() {
            if peer.bot_id == me.bot_id || peer.priority <= me.priority {
                continue;
            }
            if me.symbols.is_disjoint(&peer.symbols) {
                continue;
            }
            if peer.has_position {
                return Some(format!(
                    "deferred to higher-priority {} position on shared symbol",
                    peer.display_name
                ));
            }
            if let (Some(peer_bar), Some(peer_signal)) = (peer.latest_bar_ms, peer.signal)
                && peer_bar == latest_bar_ms
            {
                return Some(format!(
                    "deferred to higher-priority {} same bar signal={}",
                    peer.display_name,
                    peer_signal.as_str()
                ));
            }
        }
        None
    }
}
```

Add `pub mod supervisor;` to `crates/pairs/src/lib.rs`.

- [ ] **Step 4: Write the config file**

`config/pairs-testnet.toml` — the values come from `ACTIVE_BOT_PROFILES` in `scripts/pairs_bot.py` and must match `README.md`'s documented portfolio exactly:

```toml
[runtime]
loop_seconds = 60
journal_path = "data/bot.db"
# Bars fetched beyond the rolling window. The Python asked for window + 5 and
# dropped the forming candle, leaving three bars of slack before the bot would
# stop signalling entirely — too thin for a market as illiquid as XAUT, where a
# handful of missing candles in the intersection is normal.
kline_margin_bars = 25

[executor]
ticks_through = 5
fill_timeout_secs = 20
poll_interval_secs = 1
# Ticks through the book for each successive unwind attempt. Widening rather
# than repeating, because a rung that failed did so because the book moved.
unwind_ladder = [10, 25, 60]

[[bot]]
id = "aave_eth"
name = "AAVE/ETH"
priority = 100
leg_a = "AAVEUSDT"
leg_b = "ETHUSDT"
timeframe = "H1"
rolling_window = 180
entry_z = 3.0
stop_z = 4.0
target_z = 0.0
max_hold_bars = 48
fee_per_leg = 0.0002
per_leg_notional_usdt = 25
risk_pct_of_equity = 0.03
max_notional_multiple_of_equity = 1.0
enable_breakeven = false
breakeven_r_multiple = 2.0

[[bot]]
id = "ena_xrp"
name = "ENA/XRP"
priority = 90
leg_a = "ENAUSDT"
leg_b = "XRPUSDT"
timeframe = "H1"
rolling_window = 336
entry_z = 3.25
stop_z = 4.5
target_z = -0.5
max_hold_bars = 96
fee_per_leg = 0.0002
per_leg_notional_usdt = 25
risk_pct_of_equity = 0.03
max_notional_multiple_of_equity = 1.0
enable_breakeven = false
breakeven_r_multiple = 2.0

# target_z = -4.5 is not a sign error. It puts the long-spread target at
# z >= +4.5 — hold until the spread overshoots to the opposite extreme — so in
# practice this pair exits on time or stop far more often than on target.
[[bot]]
id = "bnb_xaut"
name = "BNB/XAUT"
priority = 80
leg_a = "BNBUSDT"
leg_b = "XAUTUSDT"
timeframe = "H1"
rolling_window = 240
entry_z = 3.0
stop_z = 4.5
target_z = -4.5
max_hold_bars = 96
fee_per_leg = 0.0002
per_leg_notional_usdt = 25
risk_pct_of_equity = 0.03
max_notional_multiple_of_equity = 1.0
enable_breakeven = false
breakeven_r_multiple = 2.0
```

Copy to `config/pairs-mainnet.toml` unchanged for now; the profile gate, not the file, is what keeps mainnet out of reach.

- [ ] **Step 5: Write the failing config tests**

`bot/tests/pairs_config_matches_portfolio.rs`:

```rust
use bot::config::Profile;
use bot::pairs_config::load_pairs_config;
use rust_decimal_macros::dec;

#[test]
fn the_testnet_config_is_the_portfolio_the_readme_documents() {
    // Pinned so a config edit and a README edit cannot drift apart silently.
    let cfg = load_pairs_config(Profile::Testnet).expect("config loads");
    let ids: Vec<_> = cfg.bots.iter().map(|b| b.id.as_str()).collect();
    assert_eq!(ids, ["aave_eth", "ena_xrp", "bnb_xaut"]);

    let aave = &cfg.bots[0];
    assert_eq!(aave.params.rolling_window, 180);
    assert_eq!(aave.params.entry_z, 3.0);
    assert_eq!(aave.params.stop_z, 4.0);
    assert_eq!(aave.params.target_z, 0.0);
    assert_eq!(aave.params.max_hold_bars, 48);
    assert_eq!(aave.params.risk_pct_of_equity, dec!(0.03));
    assert!(!aave.params.enable_breakeven);
}

#[test]
fn every_bot_runs_three_percent_risk() {
    let cfg = load_pairs_config(Profile::Testnet).unwrap();
    assert!(cfg.bots.iter().all(|b| b.params.risk_pct_of_equity == dec!(0.03)));
}

#[test]
fn the_configured_portfolio_shares_no_symbol_between_bots() {
    // Not an invariant of the config format — the guard exists precisely for
    // when it stops being true — but it is a property of *this* portfolio, and
    // it changing should be a deliberate, visible act.
    let cfg = load_pairs_config(Profile::Testnet).unwrap();
    let mut all = Vec::new();
    for b in &cfg.bots {
        all.push(b.params.leg_a.clone());
        all.push(b.params.leg_b.clone());
    }
    let unique: std::collections::HashSet<_> = all.iter().cloned().collect();
    assert_eq!(all.len(), unique.len(), "symbols overlap: {all:?}");
}

#[test]
fn duplicate_priorities_are_rejected_at_load() {
    // Two bots at the same priority sharing a symbol would each decline to
    // trade forever, so ambiguity is refused at startup rather than debugged
    // at 3am.
    let err = bot::pairs_config::parse_pairs_config(
        r#"
        [runtime]
        loop_seconds = 60
        journal_path = "x.db"
        kline_margin_bars = 25
        [executor]
        ticks_through = 5
        fill_timeout_secs = 20
        poll_interval_secs = 1
        unwind_ladder = [10]
        [[bot]]
        id = "a"
        name = "A"
        priority = 100
        leg_a = "AAUSDT"
        leg_b = "BBUSDT"
        timeframe = "H1"
        rolling_window = 10
        entry_z = 3.0
        stop_z = 4.0
        target_z = 0.0
        max_hold_bars = 10
        fee_per_leg = 0.0002
        per_leg_notional_usdt = 25
        risk_pct_of_equity = 0.03
        max_notional_multiple_of_equity = 1.0
        enable_breakeven = false
        breakeven_r_multiple = 2.0
        [[bot]]
        id = "b"
        name = "B"
        priority = 100
        leg_a = "CCUSDT"
        leg_b = "DDUSDT"
        timeframe = "H1"
        rolling_window = 10
        entry_z = 3.0
        stop_z = 4.0
        target_z = 0.0
        max_hold_bars = 10
        fee_per_leg = 0.0002
        per_leg_notional_usdt = 25
        risk_pct_of_equity = 0.03
        max_notional_multiple_of_equity = 1.0
        enable_breakeven = false
        breakeven_r_multiple = 2.0
        "#,
    )
    .expect_err("duplicate priorities must be refused");
    assert!(format!("{err}").contains("priority"));
}

#[test]
fn a_stop_inside_the_entry_band_is_rejected_at_load() {
    // stop_z <= entry_z means the position is stopped out the instant it opens.
    let src = std::fs::read_to_string("../config/pairs-testnet.toml").unwrap();
    let broken = src.replace("stop_z = 4.0", "stop_z = 2.0");
    let err = bot::pairs_config::parse_pairs_config(&broken)
        .expect_err("an inverted band must be refused");
    assert!(format!("{err}").contains("stop_z"));
}
```

- [ ] **Step 6: Write `pairs_config.rs`**

Follow `bot/src/config.rs` exactly: a `thiserror` `ConfigError`, serde structs mirroring the TOML, `Profile::from_name` reused unchanged for the mainnet gate, and `config_decimal`-style refusal of non-finite `f64` knobs. Split it as:

- `parse_pairs_config(src: &str) -> Result<PairsConfig, ConfigError>` — parse **and validate**; this is what the tests drive.
- `load_pairs_config(profile: Profile) -> Result<PairsConfig, ConfigError>` — read `config/pairs-{profile}.toml` and delegate.

Validation, all of it returning `ConfigError::Invalid(String)`:

- at least one bot;
- `id` unique, `priority` unique;
- `rolling_window >= 2`;
- `entry_z > 0` and `stop_z > entry_z` (an inverted band stops the position out the instant it opens);
- `max_hold_bars >= 1`;
- `risk_pct_of_equity > 0` and `<= 0.10` (a typo'd `3` instead of `0.03` must not reach the exchange);
- `max_notional_multiple_of_equity > 0`;
- `unwind_ladder` non-empty and strictly increasing (a ladder that does not widen is a retry loop, not an escalation);
- `leg_a != leg_b`;
- every `f64` knob finite. `fee_per_leg` is parsed from its TOML float and converted to `Decimal` at load via the string form (`Decimal::from_str(&v.to_string())`), the same conversion `sizing` uses for sigma — never `Decimal::try_from(f64)`, which rounds differently.

Log a `warn!` — not an error — when two bots share a symbol: that is legal and is what `PortfolioGuard` exists for, but it should never happen silently.

Add `pub mod pairs_config;` to `bot/src/lib.rs` and `pairs = { path = "../crates/pairs" }` to `bot/Cargo.toml`.

- [ ] **Step 7: Run the tests, lint, commit**

```bash
cargo test -p pairs -p bot
cargo clippy --workspace --all-targets -- -D warnings
git add crates/pairs bot config
git commit -m "feat(pairs): TOML config and a portfolio guard that can actually fire"
```

---

### Task 10: The pair loop and the `pairs` binary

**Files:**
- Modify: `crates/pairs/src/supervisor.rs` (add `run_pair`)
- Create: `bot/src/bin/pairs.rs`
- Create: `bot/tests/pairs_loop_behaviour.rs`
- Modify: `crates/pairs/tests/support/mod.rs` (scriptable `klines`)
- Modify: `bot/Cargo.toml` (declare the bin)

**Interfaces:**
- Consumes: everything from Tasks 1–9.
- Produces:
  - `pairs::supervisor::run_pair(ctx: PairContext) -> Result<(), ExecutionError>`
  - `pairs::supervisor::PairContext { client: Arc<dyn ExchangeClient>, journal: Arc<Journal>, guard: Arc<RwLock<PortfolioGuard>>, bot_id: String, display_name: String, priority: u32, params: PairParams, exec: ExecutorConfig, loop_period: Duration, kline_margin_bars: u16, shadow: bool }`
  - `pairs::supervisor::evaluate_bar(...)` — the pure decision step, split out so the loop test does not need a clock.

**Context the implementer needs:** ports `run_live` from [`scripts/pairs_bot.py:869-963`](../../../scripts/pairs_bot.py). Five things change, and each is a defect being fixed rather than a preference:

1. **One process, N tasks, one REST client.** Three processes each fetched their own klines and each paid their own rate-limit budget; sharing `BybitRest` shares its limiter and clock-offset correction, and is what makes `PortfolioGuard` observable.
2. **`kline_margin_bars = 25` instead of the Python's `+5`.** The Python requested `window + 5`, dropped the forming candle, and needed `window + 1` — three bars of slack before the bot silently stops signalling. XAUT is thin enough for that to happen.
3. **State goes to the journal, written before the loop sleeps and after every decision**, not to a non-atomic `write_text`.
4. **`ExecutionError::UnwindExhausted` halts this pair and only this pair** — journal `UnwindFailed`, set the halt flag, log at `error`, return. It must not be swallowed by a catch-all and retried, which is what the Python's `except Exception: logging.exception(...)` did with every failure indiscriminately.
5. **Shadow mode.** `--shadow` runs the entire loop — klines, z, signal, guard, sizing, reconciliation — and journals `Entry`/`Exit` events describing what it *would* have done, without calling `place_limit_leg`. This is what Task 13's cutover comparison reads.

- [ ] **Step 1: Make `klines` scriptable on the double**

In `crates/pairs/tests/support/mod.rs`, replace the `klines` stub with a per-symbol `Vec<Candle>` served from a `Mutex<HashMap<String, Vec<Candle>>>`, plus:

```rust
    /// Seed `n` H1 candles whose closes follow `f`, ending at `last_open_ms`.
    /// The spread the bot sees is entirely determined by these two series, so
    /// a test can put the z-score exactly where it wants it.
    pub fn candles(&self, symbol: &str, last_open_ms: i64, n: usize, f: impl Fn(usize) -> Decimal) -> &Self
```

- [ ] **Step 2: Write the failing loop tests**

`bot/tests/pairs_loop_behaviour.rs` — model these on the existing `bot/tests/engine_loop_behaviour.rs`, which drives the other bot's loop the same way. Each test builds a `PairContext` against a `FaultExchange` and a `Journal::open_local` on a `tempfile::tempdir`, then calls `evaluate_bar` once rather than spinning `run_pair`:

```
- a_z_beyond_the_entry_band_opens_a_position_and_journals_it
- a_z_inside_the_band_opens_nothing_and_still_writes_a_heartbeat
- a_repeated_bar_is_skipped_without_re_evaluating_the_signal
- a_target_reversion_closes_the_position_and_clears_the_journal_row
- a_max_hold_breach_closes_on_wall_clock_bars_not_evaluated_bars
- a_guard_deferral_journals_entry_skipped_and_places_no_order
- an_unwind_exhaustion_halts_this_pair_and_leaves_the_others_running
- shadow_mode_journals_the_entry_it_would_have_made_and_places_nothing
- too_few_overlapping_candles_skips_the_bar_rather_than_signalling_on_a_short_window
- a_reconciliation_halt_prevents_any_order_being_placed
```

Write each with a real body. For `a_max_hold_breach_closes_on_wall_clock_bars_not_evaluated_bars`, seed a candle series with a gap so the two age definitions differ, and assert on the wall-clock one — this is the single intentional strategy-affecting change in the port, and Task 11 measures its size.

- [ ] **Step 3: Write `evaluate_bar` and `run_pair`**

Structure `evaluate_bar` so every branch is reachable without a timer: it takes the already-fetched candle series and quotes and returns a `BarOutcome` describing what it did. `run_pair` is then a thin loop — fetch, call `evaluate_bar`, journal, sleep — and holds all the I/O.

The decision order, matching the Python's and with the two additions marked:

```
1. Fetch klines for both legs (window + kline_margin_bars).
2. Intersect open_time_ms; require >= window + 1, else skip the bar.       [+ margin]
3. Feed log spreads through RollingZ; take the last Stats.
4. Write the heartbeat and publish a BotSnapshot to the guard.
5. If latest_bar_ms == last_bar_ms, return Unchanged.
6. Reconcile journal vs exchange. On Halt, journal Halted and return Halted. [+ mirror case]
7. If flat and a signal exists:
     a. guard.defer_reason -> Some: journal EntrySkipped, return Deferred.
     b. balance -> per_leg_notional -> journal the Sizing including capped_by.
     c. shadow ? journal a would-be Entry : open_pair.
     d. On Some(pair): upsert_pair_position, journal Entry.
8. If holding:
     a. Arm breakeven if should_arm_breakeven.
     b. age_bars = (latest_ms - opened_at_ms) / timeframe.duration_ms().
     c. exit_reason(...); on Some: close_pair, clear_pair_position, journal Exit.
9. Return the outcome.
```

`run_pair` maps `Err(ExecutionError::UnwindExhausted { legs })` to: journal `UnwindFailed` with the stranded legs in `detail`, `journal.set_halt(...)`, `error!`, and `return`. Every other `Err` is logged at `warn` and retried on the next tick — those are transient exchange errors that `with_retry` has already exhausted its short budget on, and the next bar is 60 seconds away.

- [ ] **Step 4: Write the binary**

`bot/src/bin/pairs.rs`, modelled on `bot/src/main.rs`:

```rust
#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    // rustls 0.23 refuses to pick a provider when more than one is compiled
    // in, and panics deep inside a handshake rather than at startup. Same
    // reasoning as bot/src/main.rs — choose before anything connects.
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("no rustls crypto provider may be installed before this point");

    let profile = Profile::from_name(
        &std::env::args().nth(1).unwrap_or_else(|| "testnet".into()),
    )?;
    let shadow = std::env::args().any(|a| a == "--shadow");
    let cfg = load_pairs_config(profile)?;
    // ... build BybitRest, open the Journal, spawn one run_pair task per bot,
    // and join them. A task that returns (halted) must not take the others
    // down: log it and keep the rest running.
}
```

Register it in `bot/Cargo.toml`:

```toml
[[bin]]
name = "crypto-pairs"
path = "src/bin/pairs.rs"
```

- [ ] **Step 5: Run everything, lint, commit**

```bash
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
git add crates/pairs bot
git commit -m "feat(pairs): single-process supervisor loop and the crypto-pairs binary"
```

---
### Task 11: Backtest parity, and measuring the two known divergences

**Files:**
- Create: `crates/pairs/src/backtest.rs`
- Create: `crates/pairs/src/bin/pairs-backtest.rs`
- Create: `crates/pairs/tests/backtest_parity.rs`
- Modify: `crates/pairs/Cargo.toml` (add `history`, `backtest`, `serde_json`)

**Interfaces:**
- Consumes: `pairs::{RollingZ, SignalEngine, per_leg_notional}` (Tasks 1–3), `history::HistoryDb`.
- Produces:
  - `pairs::backtest::{AgeBasis, BacktestParams, BacktestResult, Trade, run_backtest}`
    - `run_backtest(series: &PairSeries, params: &PairParams, basis: AgeBasis, window: Option<(i64, i64)>) -> BacktestResult`
    - `AgeBasis::{EvaluatedBars, WallClockBars}`
    - `BacktestResult { trades: usize, wins: usize, losses: usize, win_rate: f64, profit_factor: f64, net: f64, avg_trade: f64, max_drawdown_pct: f64, capped_entries: usize, detail: Vec<Trade> }`
  - `pairs::backtest::load_pair_series(db: &HistoryDb, params: &PairParams) -> Result<PairSeries, HistoryError>`

**Context the implementer needs:** ports `backtest` from [`scripts/pairs_bot.py:345-465`](../../../scripts/pairs_bot.py) and reads the same `candles` table `crates/history` already owns (`symbol, timeframe, open_time_ms, close` — closes are `TEXT`, so parse rather than casting).

This task has three jobs and they are separable in review:

**1. Prove the port is faithful.** These are the Python's actual numbers on the current `data/history.db`, measured on 2026-09-05:

| Pair | Common bars | Trades | Net (× initial equity) |
|---|---|---|---|
| AAVE/ETH | 16,864 | 134 | 82.4914 |
| ENA/XRP | 20,591 | 43 | 0.6210 |
| BNB/XAUT | 11,804 | 41 | 0.8109 |

With `AgeBasis::EvaluatedBars` — the Python's definition — the Rust must reproduce these. Trades and bar counts must match exactly; net to 1e-6 relative.

**2. Measure the one intentional strategy change.** The Python's backtest counts *evaluated bars* since entry (`position["age"] += 1` per iterated bar) while its live loop uses wall-clock bars (`(latest_ms - opened_at_ms) // TF_MS`). Those disagree wherever candles are missing, which means **the time-stop being backtested is not the time-stop being run.** The port unifies both on wall-clock. Run each pair under both bases and report the delta; that number is the real, previously-unmeasured cost of the bug.

**3. Explain the 8,249%.** `capped_entries` counts entries where `Sizing::capped_by` was `Some`. The AAVE/ETH figure comes from compounding at a per-leg notional pinned to 100% of equity — 2× gross, no margin model, no funding, no slippage. Reporting the count next to the return is what stops that headline being read as an edge.

- [ ] **Step 1: Write the failing parity test**

`crates/pairs/tests/backtest_parity.rs`:

```rust
//! Parity against the Python this replaces, on the real history database.
//!
//! Ignored by default: it needs `data/history.db` (620 MB, not in git). Run it
//! deliberately with `cargo test -p pairs --test backtest_parity -- --ignored`.

use pairs::backtest::{AgeBasis, load_pair_series, run_backtest};

const DB: &str = "../../data/history.db";

#[tokio::test]
#[ignore = "requires data/history.db"]
async fn aave_eth_reproduces_the_python_numbers_exactly() {
    let db = history::HistoryDb::open_local(DB).await.expect("history db opens");
    let params = aave_eth_params();
    let series = load_pair_series(&db, &params).await.unwrap();
    assert_eq!(series.len(), 16_864, "common bar count");

    let r = run_backtest(&series, &params, AgeBasis::EvaluatedBars, None);
    assert_eq!(r.trades, 134, "trade count");
    assert!(
        (r.net - 82.4914).abs() / 82.4914 < 1e-6,
        "net was {}, Python measured 82.4914",
        r.net
    );
}

#[tokio::test]
#[ignore = "requires data/history.db"]
async fn ena_xrp_and_bnb_xaut_reproduce_the_python_numbers_exactly() {
    let db = history::HistoryDb::open_local(DB).await.unwrap();
    for (params, bars, trades, net) in [
        (ena_xrp_params(), 20_591, 43, 0.6210),
        (bnb_xaut_params(), 11_804, 41, 0.8109),
    ] {
        let series = load_pair_series(&db, &params).await.unwrap();
        assert_eq!(series.len(), bars, "{} bar count", params.display_pair());
        let r = run_backtest(&series, &params, AgeBasis::EvaluatedBars, None);
        assert_eq!(r.trades, trades, "{} trades", params.display_pair());
        assert!(
            (r.net - net).abs() / net < 1e-4,
            "{} net was {}, Python measured {net}",
            params.display_pair(),
            r.net
        );
    }
}

#[tokio::test]
#[ignore = "requires data/history.db"]
async fn the_sizing_cap_binds_on_most_entries_which_is_what_inflates_the_headline() {
    // Not a threshold to tune — a fact to keep visible. If this ever drops to
    // zero, the risk formula stopped asking for more than the account has and
    // the returns above mean something different.
    let db = history::HistoryDb::open_local(DB).await.unwrap();
    let params = aave_eth_params();
    let series = load_pair_series(&db, &params).await.unwrap();
    let r = run_backtest(&series, &params, AgeBasis::EvaluatedBars, None);
    assert!(
        r.capped_entries * 2 > r.trades,
        "cap bound on {} of {} entries",
        r.capped_entries,
        r.trades
    );
}

#[tokio::test]
#[ignore = "requires data/history.db"]
async fn the_whole_dashboard_workload_completes_in_well_under_a_second() {
    // The Python takes 12.5 s for exactly this: three pairs, three slices each.
    // The gain is the O(1) rolling accumulator far more than the language, and
    // this is the assertion that keeps it.
    let db = history::HistoryDb::open_local(DB).await.unwrap();
    let started = std::time::Instant::now();
    for params in [aave_eth_params(), ena_xrp_params(), bnb_xaut_params()] {
        let series = load_pair_series(&db, &params).await.unwrap();
        let split = series.times()[series.len() * 70 / 100];
        run_backtest(&series, &params, AgeBasis::WallClockBars, None);
        run_backtest(&series, &params, AgeBasis::WallClockBars, Some((i64::MIN, split)));
        run_backtest(&series, &params, AgeBasis::WallClockBars, Some((split, i64::MAX)));
    }
    let elapsed = started.elapsed();
    assert!(elapsed.as_millis() < 500, "took {elapsed:?}");
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p pairs --test backtest_parity -- --ignored`
Expected: FAIL — `unresolved import pairs::backtest`.

- [ ] **Step 3: Write the backtester**

`crates/pairs/src/backtest.rs`. Follow the Python's loop structure exactly so parity is achievable, with `AgeBasis` as the only branch:

```rust
/// How the age of an open position is counted.
///
/// The Python's backtest counted evaluated bars while its live loop counted
/// wall-clock bars, so the time-stop it measured was not the time-stop it ran.
/// Both are implemented here so the size of that discrepancy can be measured
/// once rather than argued about; `WallClockBars` is what the live bot uses and
/// what everything after this task should be measured on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgeBasis {
    EvaluatedBars,
    WallClockBars,
}
```

The rest mirrors `scripts/pairs_bot.py:345-465` one-for-one: warm `RollingZ` over the log-spread series, and on each bar with a score either look for an entry (sizing via `per_leg_notional` with `total_equity == available_equity == equity`, or the fixed notional when `risk_pct_of_equity` is zero) or evaluate an exit via `SignalEngine::exit_reason` with `unrealized_pnl_fraction`. Keep the Python's quirks that affect results — the entry bar `continue`s without an exit check, equity compounds by `equity += pnl`, drawdown is computed on the trade-by-trade curve — and record `capped_by` per entry.

Reuse `backtest::metrics` for profit factor and max drawdown if its input shape fits a pair trade; if it does not, write them locally rather than contorting the existing type, and say so in a comment.

- [ ] **Step 4: Write the CLI**

`crates/pairs/src/bin/pairs-backtest.rs`: takes `--db`, `--profile`, `--split-pct` (default 0.70) and `--age-basis` (default `wall-clock`), and prints the train/holdout/full JSON summary the Python's `backtest` subcommand printed, plus `capped_entries` and the `age_basis` used. Same shape, so the two can be diffed directly during cutover.

- [ ] **Step 5: Run parity and record the divergence**

```bash
cargo test -p pairs --test backtest_parity -- --ignored --nocapture
cargo run -p pairs --bin pairs-backtest --release -- --age-basis evaluated-bars > /tmp/rust-evaluated.json
cargo run -p pairs --bin pairs-backtest --release -- --age-basis wall-clock     > /tmp/rust-wallclock.json
python scripts/pairs_bot.py backtest --leg-a AAVEUSDT --leg-b ETHUSDT \
  --rolling-window 180 --entry-z 3.0 --stop-z 4.0 --target-z 0.0 \
  --max-hold-bars 48 --risk-pct-of-equity 0.03 --enable-breakeven false > /tmp/py-aave.json
```

Write the three-way comparison into `docs/superpowers/specs/2026-09-05-pairs-runtime-rust-design.md` under a new "Measured divergences" heading: per pair, trades and net under each age basis, and the percentage change. **If the wall-clock delta is large, stop and report it before Task 13** — it means the live bot has been running a materially different time-stop than the one that was backtested, which is a finding about the strategy, not about the port.

- [ ] **Step 6: Lint and commit**

```bash
cargo clippy --workspace --all-targets -- -D warnings
git add crates/pairs docs
git commit -m "feat(pairs): backtester at Python parity, with the age-basis divergence measured"
```

---

### Task 12: Point the Python reporting at the journal

**Files:**
- Modify: `scripts/render_pairs_dashboard.py:19-28,68-90,470-508`
- Modify: `scripts/portfolio_status.py:16,95-140`
- Create: `scripts/pairs_journal.py`
- Modify: `python_tests/test_portfolio_status.py`
- Create: `python_tests/test_pairs_journal.py`

**Interfaces:**
- Consumes: the `pair_positions` and `pair_heartbeats` tables from Task 8.
- Produces: `scripts.pairs_journal.{read_pair_position, read_pair_heartbeat, read_pair_events}` — the same dict shapes `RuntimeState` exposed today, so the dashboard's template needs no change.

**Context the implementer needs:** the dashboard stays in Python deliberately. It is a read-only reporting surface refreshed every five minutes; porting a 26 KB HTML template to Rust buys nothing and risks the one artefact that is currently working well. What must change is where it reads state from: the JSON files stop being written once Task 13 cuts over.

Keep the read strictly read-only — `mode=ro` — so a dashboard refresh can never take a write lock on the live bot's journal.

- [ ] **Step 1: Write the failing tests**

`python_tests/test_pairs_journal.py`:

```python
import sqlite3
import unittest
from tempfile import TemporaryDirectory
from pathlib import Path

from scripts.pairs_journal import read_pair_heartbeat, read_pair_position

SCHEMA = """
CREATE TABLE pair_positions (
    bot_id TEXT PRIMARY KEY, side TEXT NOT NULL, opened_at_ms INTEGER NOT NULL,
    entry_z TEXT NOT NULL, a_symbol TEXT NOT NULL, a_qty TEXT NOT NULL,
    a_entry TEXT NOT NULL, a_order_id TEXT NOT NULL, b_symbol TEXT NOT NULL,
    b_qty TEXT NOT NULL, b_entry TEXT NOT NULL, b_order_id TEXT NOT NULL,
    breakeven_armed INTEGER NOT NULL, per_leg_notional TEXT NOT NULL, capped_by TEXT);
CREATE TABLE pair_heartbeats (
    bot_id TEXT PRIMARY KEY, last_bar_ms INTEGER, last_loop_ms INTEGER,
    last_z TEXT, last_signal TEXT, last_guard_reason TEXT);
"""


class PairJournalTests(unittest.TestCase):
    def setUp(self):
        self._dir = TemporaryDirectory()
        self.db = str(Path(self._dir.name) / 'bot.db')
        conn = sqlite3.connect(self.db)
        conn.executescript(SCHEMA)
        conn.commit()
        conn.close()

    def tearDown(self):
        self._dir.cleanup()

    def test_a_missing_position_reads_as_none_not_an_error(self):
        # A flat bot is the common case; it must not raise.
        self.assertIsNone(read_pair_position(self.db, 'aave_eth'))

    def test_a_position_reads_back_in_the_shape_the_dashboard_expects(self):
        conn = sqlite3.connect(self.db)
        conn.execute(
            "INSERT INTO pair_positions VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
            ('aave_eth', 'long_spread', 1700000000000, '-3.21', 'AAVEUSDT', '1.5',
             '300.25', 'oid-a', 'ETHUSDT', '0.4', '3000.5', 'oid-b', 0, '450',
             'available_equity'),
        )
        conn.commit()
        conn.close()
        got = read_pair_position(self.db, 'aave_eth')
        self.assertEqual(got['side'], 'long_spread')
        self.assertEqual(got['opened_at_ms'], 1700000000000)
        self.assertEqual(got['per_leg_notional_usdt'], '450')
        self.assertEqual(got['capped_by'], 'available_equity')
        self.assertFalse(got['breakeven_armed'])

    def test_a_heartbeat_reads_back_with_its_guard_reason(self):
        conn = sqlite3.connect(self.db)
        conn.execute(
            "INSERT INTO pair_heartbeats VALUES (?,?,?,?,?,?)",
            ('aave_eth', 1700000000000, 1700000060000, '-1.25', None,
             'deferred to higher-priority ENA/XRP position on shared symbol'),
        )
        conn.commit()
        conn.close()
        got = read_pair_heartbeat(self.db, 'aave_eth')
        self.assertEqual(got['last_bar_ms'], 1700000000000)
        self.assertEqual(got['last_seen_z'], -1.25)
        self.assertIsNone(got['last_seen_signal'])
        self.assertIn('deferred', got['last_guard_reason'])

    def test_a_missing_journal_file_reads_as_none_rather_than_crashing_the_dashboard(self):
        # The dashboard must render before the Rust bot has ever run.
        self.assertIsNone(read_pair_position('/nonexistent/bot.db', 'aave_eth'))
```

- [ ] **Step 2: Run to verify it fails**

Run: `python -m unittest python_tests.test_pairs_journal`
Expected: FAIL — `ModuleNotFoundError: scripts.pairs_journal`.

- [ ] **Step 3: Write `scripts/pairs_journal.py`**

Open with `sqlite3.connect(f"file:{db_path}?mode=ro", uri=True)` — read-only so a dashboard refresh can never take a write lock on the live bot's journal — return `None` on `sqlite3.OperationalError` (the file may not exist yet, and the dashboard must still render), and map rows into exactly the dicts `RuntimeState` produced today: `side`, `opened_at_ms`, `entry_z`, `per_leg_notional_usdt`, `capped_by`, `breakeven_armed` for a position; `last_bar_ms`, `last_loop_wall_time`, `last_seen_z`, `last_seen_signal`, `last_guard_reason` for a heartbeat. Matching the old shapes is what keeps the HTML template unchanged.

- [ ] **Step 4: Rewire both consumers**

In `scripts/render_pairs_dashboard.py`, replace `RuntimeState.from_file(item['state_path'], params)` with the two journal reads, and drop `RuntimeState` from the `scripts.pairs_bot` import. Same in `scripts/portfolio_status.py`. Take the chance to delete the two unused imports in the dashboard (`html`, `defaultdict`) and to replace the duplicated entry-signal logic in `latest_signal_from_cache` with a call to `PairSignalEngine(params).entry_signal(...)` — a second copy of the entry rule in the reporting layer is exactly how a dashboard starts lying.

Add `bot_id` to whatever profile list the two scripts iterate, so they can key the journal reads.

- [ ] **Step 5: Verify**

```bash
python -m unittest python_tests.test_pairs_journal python_tests.test_pairs_bot python_tests.test_portfolio_status
python -m py_compile scripts/pairs_bot.py scripts/render_pairs_dashboard.py scripts/portfolio_status.py scripts/pairs_journal.py
python scripts/render_pairs_dashboard.py
```

Expected: all tests pass and the dashboard regenerates. Open `dashboard/pairs-dashboard.html` and confirm the per-bot state panels still populate.

- [ ] **Step 6: Commit**

```bash
git add scripts python_tests
git commit -m "feat(dashboard): read pair state from the Rust journal"
```

---

### Task 13: Shadow run, comparison, and cutover

**Files:**
- Create: `scripts/compare_shadow.py`
- Create: `deploy/crypto-pairs.service`
- Modify: `README.md`
- Delete (at the end, not before): `scripts/pairs_bot.py` live path, `deploy/crypto-bot-{aave-eth,ena-xrp,bnb-xaut}.service`

**Context the implementer needs:** the Python bots keep running and keep trading throughout Tasks 1–12. This task is the only one that changes what touches the account, and it is deliberately slow. Nothing here is reversible by `git revert` alone — a wrong cutover leaves real positions in an ambiguous state — so the gate is evidence, not confidence.

- [ ] **Step 1: Write the service unit**

`deploy/crypto-pairs.service` — one unit replacing three. Model it on `deploy/crypto-bot-aave-eth.service`, but the `ExecStart` is now short because the configuration lives in TOML:

```ini
# All three statistical-arbitrage pairs, one process.
[Unit]
Description=Crypto pairs bot (Bybit, all configured pairs)
After=network-online.target
Wants=network-online.target
StartLimitIntervalSec=0

[Service]
Type=simple
WorkingDirectory=/var/home/acekavi/Projects/Crypto
EnvironmentFile=/var/home/acekavi/Projects/Crypto/.env
ExecStart=/var/home/acekavi/Projects/Crypto/target/release/crypto-pairs testnet
Restart=always
RestartSec=10s
NoNewPrivileges=true
StandardOutput=journal
StandardError=journal
SyslogIdentifier=crypto-pairs

[Install]
WantedBy=default.target
```

- [ ] **Step 2: Start the shadow run**

```bash
cargo build --release -p bot --bin crypto-pairs
install -Dm644 deploy/crypto-pairs.service ~/.config/systemd/user/crypto-pairs-shadow.service
sed -i 's/crypto-pairs testnet/crypto-pairs testnet --shadow/' ~/.config/systemd/user/crypto-pairs-shadow.service
sed -i 's/SyslogIdentifier=crypto-pairs/SyslogIdentifier=crypto-pairs-shadow/' ~/.config/systemd/user/crypto-pairs-shadow.service
systemctl --user daemon-reload
systemctl --user enable --now crypto-pairs-shadow.service
systemctl --user status crypto-pairs-shadow.service --no-pager
```

Point the shadow at a **separate journal file** (`data/pairs-shadow.db` via `runtime.journal_path`) so it cannot contend with anything, and leave all three Python services running and trading untouched.

- [ ] **Step 3: Write the comparison script**

`scripts/compare_shadow.py` reads the shadow journal's `pair_heartbeats` and `pair_events` alongside the Python bots' JSON state files and log lines, and for every closed bar where both recorded a view, reports:

- `last_bar_ms` agreement (a mismatch means the two disagree about which candle is closed — the most likely source of a real difference);
- `last_seen_z` agreement to 1e-6;
- `last_seen_signal` agreement — **this is the gate**;
- every bar where the shadow would have entered or exited and the Python did not, or vice versa.

Exit non-zero on any signal disagreement so it can be run from cron.

- [ ] **Step 4: Run for seven days and gate on the result**

```bash
python scripts/compare_shadow.py --since-days 7
```

**The gate: zero signal disagreements across all three pairs over seven days.** Any disagreement is investigated and explained before proceeding — an unexplained one means the port is not faithful and Task 11's parity tests missed something. A disagreement traced to the `kline_margin_bars` change (the shadow signalling on a bar where the Python had too few candles) is expected and is a fix, not a defect; record it as such.

While waiting, confirm from the shadow journal that: every bot writes a heartbeat every loop; reconciliation reports `Flat` or `Holding` and never `Halt`; and no `UnwindFailed` event has been recorded.

- [ ] **Step 5: Cut over**

Only after Step 4's gate passes, and only when **every Python bot is flat** — a cutover with an open position hands the Rust bot a pair it did not open, which reconciliation will correctly refuse to trade:

```bash
python scripts/portfolio_status.py      # confirm: no local positions, no account positions
systemctl --user stop crypto-bot-aave-eth crypto-bot-ena-xrp crypto-bot-bnb-xaut
systemctl --user disable crypto-bot-aave-eth crypto-bot-ena-xrp crypto-bot-bnb-xaut
systemctl --user stop crypto-pairs-shadow.service
systemctl --user disable crypto-pairs-shadow.service

install -Dm644 deploy/crypto-pairs.service ~/.config/systemd/user/crypto-pairs.service
systemctl --user daemon-reload
systemctl --user enable --now crypto-pairs.service
systemctl --user status crypto-pairs.service --no-pager
journalctl --user -u crypto-pairs -n 50 --no-pager
```

Watch the first three closed bars before walking away, and confirm the dashboard populates from the live journal.

- [ ] **Step 6: Update the README and retire the old path**

Rewrite `README.md`'s "Active services", "Important paths", "Status / inspection", "Restart / rollout" and "Test commands" sections for the single binary and the TOML config. Keep the "Reality check" paragraph verbatim — it is still true, and more so now that Task 11 has quantified how much of the backtest headline is a sizing artefact.

Then remove the superseded live path in one commit, so `git revert` restores a working Python bot in a single step:

```bash
git rm deploy/crypto-bot-aave-eth.service deploy/crypto-bot-ena-xrp.service deploy/crypto-bot-bnb-xaut.service
# Keep scripts/pairs_bot.py's pure functions: the dashboard still imports
# backtest, load_pair_series_from_db, PairSignalEngine and json_ready, and
# they remain the reference the Rust is pinned against. Delete only the live
# path: BybitClient's order methods, place_pair_entry, close_pair_position,
# wait_for_terminal_state, run_live, and the `live` subparser.
git commit -m "chore: retire the Python live trading path in favour of crypto-pairs"
```

- [ ] **Step 7: Record what actually happened**

Append a "Cutover record" section to the spec: the date, the seven-day disagreement count, the age-basis divergence from Task 11, and anything found during the shadow run. This is the document the next person reads when the Rust bot does something surprising.

---

## Self-Review

Checked against `docs/superpowers/specs/2026-09-05-pairs-runtime-rust-design.md`:

| Spec requirement | Task |
|---|---|
| Leg orphaning fixed | 5 (`settle`), 6 (`open_pair` + unwind) |
| Non-converging close fixed | 7 (`close_pair`) |
| Priority guard made real | 9 (`PortfolioGuard`), 10 (published from the loop) |
| Sizing cap made visible | 3 (`Sizing::capped_by`), 11 (`capped_entries`) |
| Atomic durable state | 8 (journal tables + reconciliation) |
| Retry / backoff / error classification | Inherited from `crates/exchange`; used from Task 4 on |
| Limit-only on every path including unwind | Global constraint; asserted in Task 6 Step 3 |
| GTC, not PostOnly, not IOC | 4 (wire assertion in `rest_legs.rs`) |
| Exact arithmetic parity | 1, 2, 3 (golden vectors), 11 (whole-backtest parity) |
| O(1) rolling statistics | 1; timing asserted in 11 |
| Age-basis divergence measured | 11 |
| One process, N pairs | 9, 10 |
| Config out of argv, mainnet gated | 9 |
| Dashboard stays Python, reads the journal | 12 |
| Shadow run before cutover | 13 |
| Thin kline margin fixed | 9 (`kline_margin_bars`), 10 |
| Duplicated entry rule in the dashboard removed | 12 Step 4 |

**Known gaps, stated rather than hidden:**

- **Task 10's test bodies are named but not written out.** Ten scenarios are specified by name with the decision order they exercise; the implementer writes them against the existing `bot/tests/engine_loop_behaviour.rs` as the model. This is the one place the plan leans on a pattern already in the repo instead of quoting code, and it is deliberate — that file is 765 lines and the loop's shape must match it.
- **`crates/backtest`'s `gate` may not accept a pair trade's shape.** Task 11 reuses `metrics` where it fits and says to write local equivalents rather than contort the existing types. Whether the walk-forward gate can run on pairs is an open question that task will answer.
- **Fee and funding modelling is unchanged.** The backtest still charges `4 × fee_per_leg` and ignores funding entirely, exactly as the Python does, because parity comes first. Once Task 11's parity is green, routing pair trades through `backtest::costs` is a worthwhile follow-up and is *not* in this plan.
- **`OrderStatus` duplicates most of `OpenOrder`.** Justified in Task 4 by `avg_price`, but if a later task needs a third shape, collapsing the two is the right move.

**Corrected during self-review**, recorded because each was a real defect in an earlier draft of this plan:

- `exchange::bybit::wire::Ticker` carries **no bid or ask** — it was built for universe ranking. Task 6 Step 1 now adds them and names the ten literals that will stop compiling. A draft that assumed a top of book would have failed at the first `cargo check` of the biggest task.
- `unwind_legs` was defined in Task 6 with one signature and silently given another in Task 7. It now takes `quotes` and `tag` from the start, and the dead `tick_size_for` helper — which called the 800-symbol `instruments()` on the unwind path — is gone.
- `HistoryDb::open`, used in Task 11, does not exist. The constructor is `open_local`.
