# FOMC News-Reaction Strategy — Pre-Registration

**Written before any measurement.** A genuinely different mechanism from everything tested so far:
event-time-gated, not a continuous technical pattern.

## Scope, and why it's narrower than originally asked

Requested: FOMC + CPI + NFP. Delivered: **FOMC only.** `bls.gov` blocks this environment's fetch tool
outright (403 on every page tried, including the schedule page itself), and the one third-party
source that responded returned an incomplete, reordered table that also surfaced a real irregularity
(October 2025 CPI delayed by the government shutdown) — exactly the kind of thing a date-generation
heuristic would silently get wrong. Compiling ~70 CPI/NFP dates from unverifiable fragments would bet
the whole backtest on transcription accuracy with no way to check it. Owner's explicit call, given
that finding: FOMC only for now.

FOMC dates came from `federalreserve.gov/monetarypolicy/fomccalendars.htm` directly — the primary
source, fetched cleanly. Sanity-checked: 27 of 28 fall on a Wednesday (the one exception, 2024-11-07,
is a real, known shift — that meeting moved to Thursday because Tuesday was Election Day), and gaps
between meetings run 41-56 days, consistent with the FOMC's own ~6-8-week cadence. Announcement time
2:00pm ET, converted to UTC via `zoneinfo` (`America/New_York`), correctly split across EST/EDT.
**24 of the 28 fetched dates fall inside the price-data window** (2023-08-04 .. 2026-08-08); the
other four predate it.

## Mechanism

Not a straddle (see the entry/stop optimization doc's "momentum-confirmation entry" section for why
a real conditional order can't be honestly modelled by this engine). Instead:

1. **News window**: `window_candles` execution candles starting at the announcement timestamp. No
   entries taken; only the window's high and low are recorded. This is deliberately naive volatility
   exposure, not alpha-seeking — the point is to let the initial spike happen and define a range from
   it.
2. **Break**: within `active_window_candles` candles after the news window closes, the range must be
   broken by a close beyond it by `min_break_atr` ATRs — identical construction to `level_reaction`'s
   break rule.
3. **Retest, then reaction**: same state machine as `level_reaction` — price returns to the broken
   boundary, the NEXT candle's close decides whether the level held or failed, and that decision
   (parameterised the same way, `ReactMode`) becomes the trade.
4. **Entry/stop/target**: identical construction to `level_reaction`'s `build_signal` — entry between
   the level and the reaction candle's close, stop at the structural extreme plus a buffer, target at
   `reward_multiple × R`.

## The honest statistical-power problem, stated now

**24 events × 8 symbols is a maximum of 192 possible setups**, and only a fraction of those will
actually break, retest, and produce a valid reaction — realistically well under 100 trades total. That
is an order of magnitude smaller than every other sample this project has tested. This is stated
before running anything so a null result is not later reinterpreted as evidence of nothing, and a
positive result is held to a higher bar than usual rather than a lower one — with a sample this small,
noise alone can easily produce an apparently strong profit factor.

## Criteria

Given the sample-size problem, the bar is deliberately conservative:

1. Profit factor > 1.0, but **treated as suggestive only below ~150 trades** — not a pass/fail line at
   this sample size the way it has been for every prior test.
2. No single event or single symbol supplying the majority of net PnL — concentration is fatal at this
   sample size specifically, because one lucky trade dominates a small sample far more than a large
   one.
3. Reported honestly if the trade count is too small to say anything at all, which is the single most
   likely outcome given 24 events.

## What failure (including "too few trades") means

A trade count near zero, or a result dominated by one or two events, is not a strategy failure to
investigate further — it is the correct, expected consequence of a 24-event sample, and ends this line
without a second pass. Extending to CPI/NFP would be the only legitimate way to get a larger sample,
and that is blocked on finding a trustworthy calendar source, not on strategy design.

---

# RESULT — REJECTED

Run 2026-08-19. `ReactMode::Both`, 109 trades across 24 events x 8 symbols, fixed 0.25% risk.

## The headline number is misleading — and the pre-registration said to check for exactly this

```
n=109   win 20.2%   PF 1.447   maxDD 5.1%   net +855
```

Looks like the best result of the whole session. It is not, because **109 trades is not 109
independent trials.** Crypto symbols move together, and within almost every FOMC event most symbols
resolved the same way — four winning together at near-identical PnL, four losing together, within
minutes of each other. Regrouping by event (the real, independent unit here) rather than by
individual trade:

```
10 of 24 EVENTS net positive — worse than a coin flip on breadth.
```

## Concentration is total, not partial

Four events — 2024-07-31, 2025-05-07, 2025-12-10, 2026-01-28 — supply +1,584.6 between them.
Total net across all 24 events was +855. **The other 20 events, combined, are net NEGATIVE
(-729.6).** Four events out of twenty-four account for more than the entire result.

This is exactly the failure mode the pre-registration named in advance: *"no single event or single
symbol supplying the majority of net PnL — concentration is fatal at this sample size."* Four events
supplying more than 100% of net is a more extreme version of the same concentration that rejected the
reaction-strength and D1-bias confluences on `level_reaction` — just starker, because the sample here
is an order of magnitude smaller.

## Conclusion

**Rejected.** Not "inconclusive due to small sample" — the small-sample framing was already priced
into the pre-registration, and the result it produced is unambiguous: fewer than half the events were
profitable, and the aggregate positive number is an artifact of a handful of outsized events, not a
persistent reaction pattern. Extending to CPI/NFP would triple the event count and could genuinely
change this, but that is blocked on finding a trustworthy calendar source (see the scope note above),
not on the strategy design.
