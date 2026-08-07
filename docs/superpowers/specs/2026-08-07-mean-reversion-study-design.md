# Mean-Reversion Study — Pre-Registered Design

**Status:** pre-registration. Written BEFORE any run of this strategy on any data.
**Holdout:** the final 330 days of `data/history.db` are sealed and have never been queried.

## Why this document exists before any code

The previous strategy failed, and the post-mortem produced an attractive observation: taking the
same trades in the opposite direction won 39.83% versus 29.79%. That observation came from the data
it was measured on, and acting on it directly would be the exact failure this project has spent two
phases building defences against.

So this is a **new study with a new hypothesis**, written down first. Everything that could later
be adjusted to flatter a result — entry rule, variants, thresholds, what counts as failure — is
fixed here, in a reviewed file, before the first run.

## The hypothesis

> When price becomes sharply stretched from its short-term mean **during a low-trend regime**, it
> reverts far enough to pay 2R before it reaches the mean.

Two clauses matter and both are testable:

1. **Stretch predicts reversion.** Not "price is falling so it will bounce" — specifically that a
   large, ATR-normalised displacement from a 20-period mean is followed by displacement shrinking.
2. **Only in a range.** In a strong trend, a large displacement is the trend working, and fading it
   is the classic falling-knife loss. The regime gate is what separates this from that.

**This is a genuinely different bet from a sign-flipped trend-follower.** The old strategy entered
when a pullback *ended* (RSI crossing back) and bet on continuation. This enters when displacement
is *extreme* and bets on contraction. Different trigger, different stop geometry, different reason.
Inverting the old strategy would have kept its entry timing — the thing the research identified as
defective.

## Honest declaration: what is in-sample derived

The regime gate threshold comes from the failed strategy's post-mortem (Q7: the weakest-trend
quartile, trend strength `< 0.0208`, had the best profit factor). **It is in-sample knowledge.**

It is included because there is a mechanistic reason to expect it, independent of the data — mean
reversion works in ranges and fails in trends, which is standard and was not discovered here. But a
holdout pass must be read as *"survived a filter chosen with hindsight"*, not *"discovered a filter
that works"*. That is weaker evidence, and this paragraph exists so that nobody, including me,
forgets it later.

## Strategy design

New crate module: `crates/strategy/src/reversion.rs`, implementing the existing `Strategy` trait so
it replays through the identical live pipeline. **All owner rules are inherited unchanged**: limit
orders only, 1% risk, max 4 concurrent, max 5 entries/day, 1:2 R:R, the drawdown halts.

### Timeframes

- **H4** — regime gate only. No entries.
- **H1** — stretch measurement and entries.

### Regime gate (H4)

```
trend_strength = |EMA50 - EMA200| / EMA200
gate: trend_strength < range_max        (default 0.02)
```

Refuse every entry when the gate fails. A symbol in a strong trend is simply not traded.

### Stretch trigger (H1)

```
stretch = (close - EMA20) / ATR14
long  when stretch <= -stretch_atr
short when stretch >= +stretch_atr
```

ATR-normalised so one threshold works across BTC at $70,000 and DOGE at $0.16 — a percentage
threshold would not.

### Entry, stop, target

- **Entry**: limit at `close ∓ entry_offset_atr × ATR` (default 0.25), placed *deeper* into the
  stretch. Requires price to extend slightly further before filling, which improves the entry and
  is consistent with limit-only execution. Expires after 3 candles like every other entry.
- **Stop**: `entry ∓ stop_atr × ATR`, beyond the extreme.
- **Target**: mechanical 2R, computed by `RiskManager` from the entry-to-stop-limit distance — the
  corrected definition from `2d19d2d`.

### The geometric constraint that makes this coherent

A reversion target must land **before** the mean, or the trade needs price to overshoot the very
level it is reverting to. With the live 0.3 ATR stop-limit offset:

```
risk_in_atr   = stop_atr + 0.3
target_in_atr = 2 × risk_in_atr
REQUIRED:  target_in_atr  ≤  stretch_atr
```

Every variant below satisfies this. Any future variant that does not is invalid by construction and
must not be run.

## The six pre-declared variants

Fixed now. No variant may be added, removed or altered after the first run.

| | stretch_atr | stop_atr | risk (ATR) | target (ATR) | mean (ATR) | valid |
|---|---|---|---|---|---|---|
| **A (PRIMARY)** | 3.0 | 1.0 | 1.30 | 2.60 | 3.00 | ✅ |
| B | 2.5 | 0.8 | 1.10 | 2.20 | 2.50 | ✅ |
| C | 3.5 | 1.2 | 1.50 | 3.00 | 3.50 | ✅ |
| D | 3.0 | 0.8 | 1.10 | 2.20 | 3.00 | ✅ |
| E | 4.0 | 1.2 | 1.50 | 3.00 | 4.00 | ✅ |
| F | 2.5 | 0.6 | 0.90 | 1.80 | 2.50 | ✅ |

Held constant across all six: `range_max = 0.02`, `entry_offset_atr = 0.25`, EMA/ATR periods
(20/14 on H1, 50/200 on H4), and every owner rule.

**A is the primary** because its 3.0 ATR stretch is a genuinely unusual displacement while leaving
comfortable room between target (2.60) and mean (3.00). It is named as primary *now*, before any
result, so that "the primary passed" cannot later be redefined.

## Protocol

### Stage 1 — research (769-day window, `--holdout-days 330`)

Run all six variants. Record every result, including the failures. **A research-window pass means
nothing on its own** and is not reportable as evidence — its only job is to select which single
variant proceeds.

**Selection rule, fixed now:** the variant with the highest out-of-sample expectancy *in the
research walk-forward*, provided it also produced ≥200 trades there. If none clears 200 trades, the
study fails at Stage 1 for insufficient signal frequency and the holdout is not opened.

### Stage 2 — validation (330-day holdout, `--holdout-only`)

**Exactly one variant. Exactly one run. Once.**

Whatever that run reports is the result. If it fails, the study fails. There is no second
configuration, no "let's also try B", no widening the window. Re-opening the holdout after seeing
it would destroy the only clean data remaining and there is no way to undo it.

## Pass/fail — the gate, with a correction for six variants

Four criteria are unchanged from the approved Phase 2 spec:

| Criterion | Threshold |
|---|---|
| OOS expectancy | > 0, net of fees and funding |
| OOS trade count | ≥ 200 |
| Max drawdown | ≤ 15% |
| Profit factor | ≥ 1.3 |

The fifth is **tightened**, because testing six variants and reporting the best is six chances to
find noise:

| Criterion | Phase 2 | This study |
|---|---|---|
| Random-entry benchmark | beat p95 | **beat p(100 − 5/6) = p99.17** |
| Benchmark seeds | 100 | **500** |

The percentile follows the standard Bonferroni logic — spend the 5% across six looks — and the seed
count rises to 500 so that a 99.17th percentile is estimated from ~496 runs rather than
extrapolated off the end of 100.

The holdout run is a **single** variant, so it is not itself a multiple comparison. The correction
guards the selection that happened upstream in research.

**Thresholds do not move after this document is committed.** `GateThresholds::pre_registered` keeps
its own tripwire test; this study's stricter percentile is a separate, explicitly named constant.

## What failure means, committed to in advance

If Stage 2 fails, the conclusion is: **mean reversion on this design, universe and period does not
have a demonstrable edge.** Not "needs more tuning."

Specifically ruled out in advance as responses to a failure:
- adjusting any threshold in this document and re-running
- running a second variant against the holdout
- extending the universe or the date range to find a friendlier sample
- switching timeframes and calling it the same study

Any of those is a new study, needing new data this repository does not have. The honest base rate is
that most strategy ideas fail this process, and a study that cannot fail is not a study.

## Limitations that survive a pass

1. **The regime gate is in-sample derived** (see above). This is the weakest joint in the design.
2. **330 days is one validation window** — roughly 5 walk-forward folds. A pass is evidence, not
   proof.
3. **Delisted symbols are absent**, so results skew toward survivors by an unmeasurable amount.
4. **Eight symbols, one venue, one asset class**, over a period that is not a full market cycle.
5. **The maker fee is corroborated, not primary-sourced** — confirm against Bybit's own schedule
   before any real money follows a pass.
6. Even a full pass means *start small*. The gate is a filter against self-deception, not a
   guarantee.

## Implementation notes

- `crates/strategy/src/reversion.rs`, reusing `indicators::{Ema, Atr}`. No new indicator maths.
- `ReversionParams` mirroring `StrategyParams`'s shape so the walk-forward harness needs no change.
- Existing guards apply: `Decimal` only, no `f64`, no wall-clock, no market orders, `botcore` never
  `core`.
- The six variants become the walk-forward grid. Note the harness tunes *within* a fold on
  in-sample expectancy — that is fold-level tuning and is separate from the Stage 1 selection
  across variants, which happens on concatenated research OOS results.
