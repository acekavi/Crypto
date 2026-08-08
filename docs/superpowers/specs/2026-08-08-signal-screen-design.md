# Signal Screen — Pre-Registration

**Written before running any of it.** Signal list, method, horizons and threshold fixed here.

## Why a screen rather than another strategy

Four strategies have been built and rejected one at a time: trend continuation, mean reversion, ICT
structure, funding carry. Each cost days of implementation before producing a verdict that a
statistical test could have delivered in minutes.

The carry study proved the cheaper order of operations: **test whether a signal predicts before
building anything on it.** This applies that to a batch.

## Method — identical for every signal

At each 8-hour timestamp, across all symbols with sufficient history:

1. Rank symbols by the signal value.
2. Form quintiles.
3. Measure each symbol's forward **price return** over the horizon.
4. Record the spread: mean return of the top quintile minus the bottom.
5. Report mean spread, t-statistic, and per-year stability.

No orders, no fills, no fees, no engine. A signal that only appears once wrapped in entry rules,
stops and sizing is not a signal.

## The signals — fixed now, none added later

| # | Signal | Mechanism |
|---|---|---|
| 1 | Momentum, trailing 7d return | Trend persistence |
| 2 | Momentum, trailing 30d return | Slower trend persistence |
| 3 | Reversal, trailing 8h return | Short-horizon overreaction |
| 4 | Reversal, trailing 24h return | Daily overreaction |
| 5 | Realised volatility, 7d | Low-volatility anomaly |
| 6 | Turnover vs its own 30d mean | Attention / participation |
| 7 | Position in trailing 30d range | Where price sits in its range |
| 8 | Funding rate | **Control** — already rejected; must reproduce |

Signal 8 is a control. It has already been measured at t = −1.01. If the screen shows anything
materially different for it, the screen itself is wrong and no other result from it can be trusted.

## Horizons

Two: **8 hours** and **24 hours**. Two only, because each additional horizon multiplies the number
of tests and therefore the bar.

## The threshold, with its correction

**8 signals × 2 horizons = 16 tests.**

Testing 16 things and reporting the best is 16 chances to find noise. Family-wise 5% across 16 tests
gives a per-test level of 0.3%, i.e. |t| > 2.94.

**The bar is |t| > 3.5**, deliberately above that. This project has already produced one false
positive that passed a five-criterion gate before failing its holdout, crypto symbols move together
so the naive t-statistic is optimistic, and a screen is exactly the setting where a marginal result
is most likely to be luck.

A signal must also clear **0.04% per rebalance** (round-trip maker cost) to be worth anything, and
be **directionally consistent across years**.

## Data

The 41-symbol universe, ~1,100 days, minimum 200 observations per symbol. The same data the carry
study used.

**This is a screening sample, not a validation sample.** Anything surviving the screen has been
selected on this data and must be validated on something else before it means anything. What that
something else is becomes a real problem — the ICT holdout is spent and this screen consumes the
40-symbol set. Fresh validation would need a different venue, a different period, or forward paper
trading. That constraint is stated now, so a survivor is not mistaken for a finding.

## What the outcomes mean

**Nothing clears |t| > 3.5** — the likeliest result. Combined with four rejected strategies, that is
substantive evidence that simple systematic edges are not available on liquid Bybit perpetuals with
the data available here. That is a real answer and it should end the search rather than prompt a
wider one.

**Something clears** — it becomes a candidate, not a strategy. Next step would be a pre-registered
study with a validation sample that does not yet exist.

## Ruled out in advance

- Adding signals after seeing results
- Adding horizons to rescue a marginal signal
- Lowering the threshold
- Reporting the best signal without reporting all 16 tests
