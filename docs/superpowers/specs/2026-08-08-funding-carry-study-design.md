# Funding Carry Study — Pre-Registration

**Status:** pre-registration. Written before any measurement of this signal.

## The rule conflict, up front

A carry strategy does not fit the owner's stated hard rules, and pretending otherwise would waste
the work. Carry is: rank every symbol by funding, hold the receivers, short the payers, rebalance
each funding period. That implies:

| Owner's rule | Carry needs |
|---|---|
| Max 5 entries per day | Up to 40 rebalances per period, three periods a day |
| Fixed risk-to-reward (1:3, revised from 1:2 during the ICT work) | No price target at all — the return is the funding collected |
| Stop loss on every trade | Positions held to the next rebalance, not stopped out |
| Max 4 concurrent positions | Breadth is the entire mechanism; 4 defeats it |

**This study therefore does NOT propose a tradeable strategy yet.** It tests whether the signal
carries information. If it does not, nothing further is needed and no rules had to be renegotiated.
If it does, the rules become a real decision — yours, made explicitly — rather than something
quietly bent to accommodate a result.

## Stage 0 — the signal test

The only question at this stage:

> Does the funding rate at time *T* predict cross-sectional returns from *T* to the next funding
> timestamp?

This is a **statistical study, not a backtest.** No orders, no fills, no fees, no engine. Testing
whether a signal exists before building machinery around it is the cheaper order of operations, and
it keeps the question falsifiable: a signal that only appears once wrapped in entry rules, stops and
position sizing is not a signal.

### Method

At every funding timestamp, across all symbols with data:

1. Rank symbols by funding rate.
2. Form quintiles. Q1 is the most negative (shorts pay longs), Q5 the most positive (longs pay
   shorts).
3. Measure each symbol's **price return** from that timestamp to the next funding timestamp.
4. Record the spread: mean return of Q1 minus mean return of Q5.

The carry hypothesis predicts a **positive spread** — the crowded side underperforms.

Price return, deliberately excluding the funding payment itself. Collecting funding is arithmetic
and guaranteed; the open question is whether the price move eats it. Including it would let the
mechanical part of carry mask a price drift going the other way.

### Pre-registered thresholds

Fixed now:

| Criterion | Threshold |
|---|---|
| Mean Q1−Q5 spread | > 0 |
| t-statistic on the spread | **> 3.0** |
| Positive in each yearly sub-period | at least 2 of 3 |
| Spread exceeds round-trip cost | > 0.04% (2 × 0.02% maker) |

**t > 3.0, not the conventional 1.96.** Three strategies have already been rejected here, the
universe is one venue and one asset class, and published factors routinely fail to replicate at
t ≈ 2. A higher bar is the appropriate response to a search that has already produced one false
positive on this data.

### Data

The 40-symbol universe at a 20M turnover floor, ~1,100 days. **The original 8 symbols are excluded
from the primary test** — they have been examined repeatedly across three studies and the ICT
holdout was spent on them. The 32 previously untouched symbols are the clean sample.

The 8 original symbols will be reported separately as a secondary check, clearly labelled as
contaminated.

### Known biases, stated before the result

1. **Survivorship.** The universe is symbols liquid enough *today*; those that died are absent.
   Likely to flatter any long-side result.
2. **Non-crypto instruments.** The list includes tokenised equities and metals (XAU, XAG, SOXL,
   SKHYNIX). Different dynamics, probably short history. Reported separately.
3. **Short history.** Some symbols will have far less than 1,100 days. Any symbol with fewer than
   200 funding observations is excluded, declared now rather than chosen after seeing which helps.
4. **Cross-sectional dependence.** Crypto symbols move together, so observations are not
   independent and the naive t-statistic is optimistic. This is the main reason for the t > 3 bar.

## Stage 1 — only if Stage 0 passes

If the signal survives, the next step is a strategy design, and it starts with a decision only the
owner can make: **which rules change, and to what.** Options would include holding carry positions
without price targets, raising the concurrent-position limit to get breadth, or accepting that carry
cannot be traded inside the current envelope.

That decision is not pre-made here. Stage 1 does not begin until Stage 0 produces a result.

## What failure means

**A failed Stage 0 ends this line of work.** No re-ranking by a different funding window, no
switching to a longer holding period to find a horizon that works, no dropping symbols until the
t-statistic clears. Any of those is a new study needing data this repository does not have.

The honest prior: funding carry in crypto is well known and heavily traded. If a simple version
still worked at t > 3 on a liquid 40-symbol universe, it would be surprising.
