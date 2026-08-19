# Time-Series Momentum — Pre-Registration

**Written before any measurement of this signal.** Method, horizons and thresholds fixed here.

## Why this signal, and why it is not the one already rejected

The 8-signal screen measured **cross-sectional** momentum: rank symbols by trailing return, form
quintiles, measure the spread. It scored t = 2.32 on the 406-symbol universe and was rejected against
a 3.5 bar.

That result **replicates the literature rather than contradicting it.** Han, Kang & Ryu — whose study
is explicitly built around realistic assumptions — conclude that evidence of *cross-sectional*
momentum in crypto is weak while evidence of *time-series* momentum is strong. Liu, Tsyvinski & Wu
(Journal of Finance, 2022) find momentum among the three factors pricing the crypto cross-section.

**Time-series momentum is a different construction and has never been tested here.** No ranking, no
quintiles: each symbol's own trailing return is tested against its own forward return.

## Stage 0 — the signal test

> Does a symbol's own trailing return over lookback *L* predict the sign of its own forward return
> over horizon *H*?

**A statistical study, not a backtest.** No orders, no fills, no fees, no engine. The carry study
proved this order of operations: establish that a signal predicts before building anything on it.

### Method

For every symbol and every daily timestamp *t*:

1. `signal = sign(return over the trailing L days)`
2. `payoff = signal × (forward return over the next H days)`
3. Pool payoffs across all symbols and dates; report mean, t-statistic, and per-year stability.

This is the Moskowitz–Ooi–Pedersen construction. A positive mean payoff means the trailing sign
carries information about the forward move.

### The bull-market trap, and how it is controlled

The sample covers a period in which crypto rose substantially. **A long-biased rule wins trivially in
a rising market**, and a naive TSMOM payoff would mostly measure passive exposure rather than timing.
Three controls, fixed now:

1. **Buy-and-hold benchmark** reported alongside every cell.
2. **Long and short legs reported separately.** A signal that only works long is beta, not timing.
3. **The primary statistic is the payoff net of passive exposure** — TSMOM minus the mean forward
   return over the same observations. A signal that cannot beat always-being-long has added nothing.

### Parameters — fixed now, none added later

| | |
|---|---|
| Lookbacks *L* | 7, 30, 90 days |
| Horizons *H* | 1, 7 days |
| Tests | 6 |

### Thresholds

**6 tests.** Family-wise 5% gives roughly |t| > 2.64. **The bar is t > 3.0**, deliberately above it:
crypto symbols move together so the naive t-statistic is optimistic, and this project has already
produced one false positive that passed a five-criterion gate before failing its holdout.

A cell must also:

- have a **positive mean payoff net of passive exposure**
- exceed **0.04%** per rebalance (round-trip maker cost)
- be **directionally consistent across years** (at least 2 of 3)
- not be **driven by a single symbol** — reported per-symbol

## Data

`data/wide.db`: 406 symbols, daily, ~1,100 days. The widest and least survivorship-affected universe
available here.

**This is a screening sample, not a validation sample.** It was already used for the
cross-sectional momentum re-test, so a survivor has been selected on data this project has seen. The
ICT holdout is spent and no clean sample exists. That constraint is stated now so a survivor is not
mistaken for a finding — it would be a *candidate*, needing a different venue, period, or forward
paper trading before it meant anything.

## What the outcomes mean

**Nothing clears t > 3.0 net of passive exposure** — the likeliest result, and it would close the
momentum line entirely: both constructions tested, both rejected, on top of four rejected strategies.
That is a real answer and should end the search rather than widen it.

**Something clears** — a candidate, not a strategy. It would then face the rule conflicts carry hit:
TSMOM holds until the signal flips, with no fixed target and no stop, which contradicts the standing
1:5 R:R and stop-loss rules. That decision is the owner's, made explicitly.

## Ruled out in advance

- Adding lookbacks or horizons after seeing results
- Dropping symbols or years until a t-statistic clears
- Reporting the best cell without reporting all 6
- Treating a gross-of-cost or gross-of-beta number as the headline

---

# RESULT — REJECTED

Run 2026-08-19 on `data/wide.db`: 292 symbols with ≥ 200 daily closes.

## All six cells fail

Payoffs are net of passive exposure, sampled on **non-overlapping** windows.

| L | H | n | net % | t | verdict |
|---|---|---|---|---|---|
| 7 | 1 | 205,967 | +0.009 | 0.39 | FAIL |
| 7 | 7 | 29,351 | −0.325 | −1.12 | FAIL |
| 30 | 1 | 199,384 | +0.004 | 0.16 | FAIL |
| 30 | 7 | 28,346 | +0.187 | 0.96 | FAIL |
| 90 | 1 | 181,931 | +0.049 | 2.12 | FAIL |
| 90 | 7 | 25,875 | +0.232 | 0.92 | FAIL |

Nothing approaches the pre-registered t > 3.0.

## The apparent winner was an overlap artifact

Sampled daily, L=30/H=7 scored **t = 4.80** and looked like a clear pass. But a 7-day forward return
sampled every day reuses six of its seven days in the next observation, so those observations are not
independent and the t-statistic is inflated by roughly √H.

| sampling | n | net % | t |
|---|---|---|---|
| overlapping (daily) | 197,633 | +0.376 | **4.80** |
| non-overlapping (every 7 days) | 28,346 | +0.187 | **0.96** |

Same data, same rule. The entire result was a counting artifact. This is the single most common way a
momentum study manufactures significance, and it is worth recording as the reason this project now
samples non-overlapping windows by default.

## The sign flips by year

L=30/H=7, non-overlapping:

| year | n | net % | t |
|---|---|---|---|
| 2023 | 1,735 | **−1.986** | −6.94 |
| 2024 | 6,734 | **−1.745** | −5.37 |
| 2025 | 11,049 | **+1.621** | +4.63 |
| 2026 | 8,828 | +0.294 | +0.79 |

Two years strongly negative, one strongly positive. The pooled positive number comes almost entirely
from 2025, which also carries the most observations. That is regime dependence, not a signal — and it
fails the pre-registered consistency criterion outright.

## Not an outlier problem either

171 of 292 symbols positive (59%). Trimming the top and bottom five leaves +0.155% against +0.187%
pooled, so the result is neither rescued nor caused by a handful of extreme symbols. It is simply
absent.

## What this closes

Both momentum constructions have now been tested on this data and both are rejected: cross-sectional
at t = 2.32 in the 8-signal screen, time-series here with nothing above t = 2.12.

Combined with four rejected strategies (trend continuation, mean reversion, ICT, funding carry), that
is substantive evidence that simple systematic edges are not available to this project on liquid
crypto perpetuals with the data it has. Per the pre-registration, this ends the momentum line rather
than prompting a wider search.
