# ICT Study — Pre-Registered Design

**Status:** pre-registration. Written BEFORE any run of this strategy on any data.
**Holdout:** the final 330 days of `data/history.db` remain sealed. Two studies have now been run
without opening them.

## Standing on thinner ice than the last study

The research window has now been examined three times: the pullback post-mortem, the reversion
study, and this. Each look makes any pattern found on it less trustworthy, because the chance of
noticing something that is merely noise accumulates whether or not I intend it.

That is the reason this design is deliberately narrower than what was asked for. Dropped, and
recorded here so the omission is visible rather than quiet:

- **Economic-calendar conditioning.** `tedata` was evaluated at the owner's suggestion. It scrapes
  historical indicator *time series* ("CPI was 3.2% in March"), not a *release calendar* with
  intraday timestamps and forecast-versus-actual. Event-reactive rules need the latter. It is also
  Selenium-driven, capped at 10 years, and documented as flaky. **Unresolved, not solved.**
- **Political-statement sentiment.** No reliable timestamped archive, and the period contains on the
  order of dozens of major events — an effective sample far too small to condition on without
  inventing an edge.

Both remain legitimate ideas. Neither can be tested honestly with the data available here.

## The hypothesis

> Price sweeps a prior liquidity level, then shifts market structure against the sweep. Entering the
> resulting fair-value gap, in the direction of the higher-timeframe bias, during the New York
> session, produces a win rate above breakeven.

Four clauses, each doing work:

1. **Liquidity sweep** — price takes out a prior swing high/low and closes back through it. The
   sweep is the event; a level merely being touched is not.
2. **Market structure shift (MSS)** — after the sweep, price breaks the most recent opposing swing,
   evidencing that the move has actually turned rather than paused.
3. **Fair value gap (FVG)** — a three-candle imbalance left behind by the MSS leg. The entry is a
   limit order inside it, which suits the limit-only rule: price must return to fill.
4. **Higher-timeframe bias and session** — only trade with the 1D/4h/1h stack aligned, and only
   during New York hours.

## Why this is a different bet from the two failures

- **PullbackStrategy** bet on trend *continuation* after a pullback ended. It lost, and the
  post-mortem showed its trend filter's chosen direction underperformed its own mirror.
- **ReversionStrategy** bet on *contraction* of displacement in flat regimes. It lost, and its best
  variant was destroyed by fee drag at high turnover.
- **This** bets on a *specific structural sequence* — sweep, then structural break, then a return to
  an imbalance. It is far more selective than either, which matters: the reversion study's clearest
  finding was that fees consume a thin edge at high trade frequency. Fewer, better-defined setups is
  the direct response to that evidence.

## Strategy design

New module `crates/strategy/src/ict.rs` implementing the existing `Strategy` trait, so it replays
through the identical live pipeline. **All owner rules inherited unchanged**: limit orders only, 1%
risk, max 4 concurrent, max 5 entries/day, 1:2 R:R, the drawdown halts.

### Timeframes

| | Role |
|---|---|
| **D1** | Directional bias: close above/below the `bias_ema` |
| **H4** | Bias confirmation: must agree with D1 |
| **H1** | Where sweeps and market structure are tracked |
| **M15** | Execution: FVG entry |

An entry requires D1 and H4 to agree. When they disagree the symbol is not traded — no "partial
alignment" tier, which would be another knob.

### Definitions, fixed here so they cannot drift

**Swing point** — a candle whose high is the highest of the `swing_lookback` candles either side (a
swing high), or whose low is the lowest (a swing low). Standard fractal definition.

**Liquidity sweep (H1)** — a candle whose *high* exceeds a prior swing high but whose *close* falls
back below it (bearish sweep), or whose *low* undercuts a prior swing low but whose *close* recovers
above it (bullish sweep). The close is what distinguishes a sweep from a genuine break.

**Market structure shift (H1)** — after a bullish sweep, a subsequent H1 close *above* the most
recent swing high. After a bearish sweep, a close *below* the most recent swing low. Must occur
within `mss_window` candles of the sweep, or the setup expires.

**Fair value gap (M15)** — three consecutive candles where candle 1's high is below candle 3's low
(bullish FVG), or candle 1's low is above candle 3's high (bearish FVG). The gap is the untraded
range between them. Only FVGs formed *after* the MSS count.

**Entry** — a limit at the FVG's `fvg_entry_fraction` depth, measured from the edge price first
reaches. 0.5 is the midpoint. Expires after the standard 3 candles.

**Stop** — beyond the sweep extreme by `stop_buffer_atr` × ATR(14) on M15. The sweep extreme is the
structural invalidation point: price returning there means the sweep was not a sweep.

**Target** — mechanical 2R from the entry-to-stop-limit distance, as corrected in `2d19d2d`.

### New York session

Entries only when the M15 candle's open falls within `[ny_open_utc, ny_close_utc)`, default
13:30–20:00 UTC (09:30–16:00 ET). Held positions are **not** force-closed at session end — that
would be a second, unrelated exit rule and another knob.

**Stated plainly:** ET shifts against UTC with US daylight saving. A fixed UTC window is therefore
one hour off for part of each year. Correcting it requires a timezone database and introduces its
own edge cases; the fixed window is declared as an approximation rather than silently treated as
exact.

## The six pre-declared variants

Fixed now. None may be added, removed or altered after the first run.

| | swing_lookback | mss_window | fvg_entry_fraction | stop_buffer_atr |
|---|---|---|---|---|
| **A (PRIMARY)** | 5 | 12 | 0.50 | 0.25 |
| B | 5 | 12 | 0.75 | 0.25 |
| C | 3 | 12 | 0.50 | 0.25 |
| D | 8 | 12 | 0.50 | 0.25 |
| E | 5 | 24 | 0.50 | 0.25 |
| F | 5 | 12 | 0.50 | 0.50 |

Held constant: `bias_ema` 50 on D1 and H4, ATR period 14, the session window, and every owner rule.
Each variant changes **one** thing from A, so a difference is attributable.

**A is primary**, named now, before any result.

## Protocol

### Stage 1 — research (769-day window, `--holdout-days 330`)

Run all six. Record every result including failures. **A Stage 1 pass is not evidence**; its only
job is to select which single variant proceeds.

**Selection rule, fixed now:** highest out-of-sample expectancy among variants producing ≥200
research trades. If none reaches 200, the study fails at Stage 1 for insufficient setup frequency
and the holdout is not opened. Given how selective this setup is, that is a live possibility and is
an acceptable outcome, not a reason to loosen a definition.

### Stage 2 — validation (330-day holdout, `--holdout-only`)

**One variant. One run. Once.** Whatever it reports is the result.

## Pass/fail

Four criteria unchanged from the approved Phase 2 spec: OOS expectancy > 0 net of costs; ≥200 OOS
trades; max drawdown ≤ 15% (worst fold, per `0a9cb7a`); profit factor ≥ 1.3.

The benchmark criterion is tightened identically to the reversion study — six variants means six
chances to find noise:

| | Phase 2 | This study |
|---|---|---|
| Random-entry benchmark | beat p95 | **beat p99.17** (Bonferroni across six) |
| Benchmark seeds | 100 | **500** |

`GateThresholds::pre_registered` is untouched and keeps its tripwire test.

## What failure means, committed to in advance

**A third failure is a real possibility and would be informative.** Three independent strategy
families — trend continuation, mean reversion, and structural — all failing on the same universe and
period would say something about the universe and period, not just about the strategies.

Ruled out in advance as responses to a failure: adjusting any definition or threshold in this
document and re-running; running a second variant against the holdout; adding a seventh variant;
extending the universe or date range to find a friendlier sample; or dropping to 5m execution to
manufacture more trades.

## Limitations that survive a pass

1. **The research window has been examined three times.** This is the weakest joint, and it cannot
   be repaired by anything inside this study.
2. **ICT definitions vary between practitioners.** The ones above are one reasonable formalisation,
   fixed here. A pass validates *these* definitions, not "ICT" as a body of ideas.
3. **The NY session window is a fixed UTC approximation** of a daylight-saving-shifting local time.
4. **No macro conditioning**, which is how much of ICT is actually traded.
5. 330 days is one validation window; a pass is evidence, not proof.
6. Delisted symbols absent; eight symbols, one venue, not a full market cycle.
7. **Start small on real money regardless.**

## Implementation notes

- `crates/strategy/src/ict.rs`, reusing `indicators::{Ema, Atr}`. No new indicator maths.
- `IctParams` mirroring the other params structs so the walk-forward harness needs no change.
- Swing/sweep/MSS/FVG detection extracted as **pure functions** so each can be tested on hand-built
  candle sequences — the fill-model lesson from `46499fb`, where a rule tested only through the
  engine was a coin flip.
- Existing guards apply: `Decimal` only, no `f64`, no wall-clock, no market orders, `botcore` never
  `core`.
