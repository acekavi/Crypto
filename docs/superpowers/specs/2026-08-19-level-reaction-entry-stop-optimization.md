# Level Break-Retest-Reaction — Entry/Stop Optimization

**Written before running any of it.**

## Why this is worth doing, and what would make it not worth doing

The level-reaction strategy was rejected on one criterion: profit factor moved with position size
(1.153 at 1% risk funding 8% of signals, 0.942 at 0.25% risk funding ~95%). That is a **funding
artifact**, not proof the signal is empty — H4 outperformed H1 in every react mode, reversal beat
continuation in every pairing, and PF was flat across break-margin settings. That's a coherent
structure, which is why it's worth one more pass rather than a closed case.

**The fix cannot be "raise risk_pct again."** That reproduces the exact bias that got this rejected.
The only legitimate lever is changing what the stop distance *is* — a tighter, better-placed stop
funds more of the population at the same risk_pct, which is the only way to close the gap between the
8%-funded number and the 95%-funded number honestly.

## What is being varied

Two axes, independent of react mode or level timeframe (both fixed at the best-supported choice: H4
levels, reversal mode, established in the prior grid):

1. **Entry depth** — where the limit rests between the level and the reaction candle's close.
   `entry_fraction = 0.0` is the current behaviour (at the level, deepest retracement required, worst
   fill rate). `entry_fraction = 1.0` rests at the reaction candle's own close (shallowest, best fill
   rate, worst average price). Fractions in between are linear.
2. **Stop construction** — `stop_source` chooses whether the stop is the touch candle's extreme alone
   (`TouchOnly`, tighter) or the touch-and-reaction combined extreme (`TouchAndReaction`, current,
   wider). `stop_buffer_atr` adds an ATR margin on top of either.

## Fixed throughout

`risk_pct = 0.25%` for every cell. This is deliberate and is the whole point: at 0.25% risk the
strategy funded ~95% of its full signal population in the prior test, so a result here is a result
about the *signal after the entry/stop change*, not about which subset the margin budget could afford.

`react_mode = Reversal`, `level_tf = H4` — the best-supported cell from the prior grid, held constant
so this test isolates entry/stop mechanics and nothing else. `reward_multiple = 5`,
`breakeven_at_r = Some(2)` — unchanged from every other measurement in this project unless the result
below says otherwise.

## Pre-registered criteria

A cell is a candidate only if **all** of:

1. Profit factor > 1.0 at the FULL population (0.25% risk) — not merely at a size that funds a
   minority of signals.
2. Stable across neighbouring parameter values — a plateau, not a spike. The M5 breakout and the
   original level-reaction grid were both rejected in part because the best cell sat between two worse
   ones; a real improvement should hold on both sides of it.
3. Broad: majority of symbols profitable, majority of quarters profitable — measured on whichever cell
   clears criteria 1-2, the same breakdown already run on the rejected version.
4. Net of Bybit's real maker fee (0.02% — entries here are limit orders, so maker applies), not zero
   fees.

## What failure means

If nothing clears profit factor 1.0 at the full population, entry and stop mechanics are not the
missing piece — the signal itself (a swing-level break-retest-reaction on M5 crypto) does not carry
enough information to overcome costs, regardless of where the stop sits or how the entry is timed.
That would close this strategy for good rather than prompting a third round of parameter search.

---

# RESULT — REJECTED

Run 2026-08-19. 30 cells: entry_fraction {0, 0.25, 0.5, 0.75, 1.0} x stop_source {TouchOnly,
TouchAndReaction} x stop_buffer_atr {0, 0.25, 0.5}. All fixed at react=Reversal, level_tf=H4,
risk_pct=0.25% (full signal population, ~6,500-7,300 trades per cell).

**Best cell: entry_fraction=0.75, TouchAndReaction, buffer=0.5 ATR → PF 0.974.** Still losing money.
No cell in the 30-cell grid clears profit factor 1.0.

## The one real relationship in the grid

PF rises monotonically with `stop_buffer_atr` (0 -> 0.25 -> 0.5) in all 15 entry_fraction x
stop_source pairs. Consistent, not noise. But win rate is flat at 14.5-15.7% throughout — the wider
stop reduces premature stop-outs and shrinks losers relative to winners, which is "loses less
slowly," not "finds edge." The trajectory approaches 1.0 from below without crossing it.

## What this closes

Per the pre-registration, this ends the level-reaction line. Entry depth and stop construction were
the one lever not yet isolated from the funding-size artifact that got the base version rejected —
tested here at a fixed 0.25% risk across the full signal population, neither lever produces a
profitable cell. The signal — a swing-level break, retest and reaction on M5 crypto — does not carry
enough information to overcome costs, independent of where the stop sits or how deep the entry waits.

---

# NY SESSION RE-TEST — SAME REJECTION

Run 2026-08-19. Identical 30-cell grid, `session_filter: true` (13:30-20:00 UTC) added, nothing
else changed.

**Best cell: entry_fraction=0.0, TouchOnly, buffer=0.5 ATR -> PF 0.966**, n=2,470, net -1,395.
Still a loser; still fails the pre-registered PF > 1.0 bar.

## Against the all-hours grid, cell for cell

| | all-hours best | NY-only best |
|---|---|---|
| PF | 0.974 | 0.966 |
| n | 7,040 | 2,470 |
| net | -2,550 | -1,395 |

Session gating cut the trade count by roughly 65% (consistent with NY being ~27% of the day) but did
**not** raise profit factor. The best cell actually moved to a different, more conservative corner of
the grid (entry at the level itself rather than 0.75 toward the reaction close) — sessions changed
which corner survived best, not whether any corner worked. The same monotone stop_buffer_atr pattern
holds throughout: wider stop -> fewer premature stop-outs -> smaller average loser -> PF creeps toward
1.0 without crossing it.

## Conclusion unchanged

The level-reaction signal carries no edge on M5 crypto with any entry depth, stop construction, or
session restriction tested. NY-session gating does not change that; it only trades less. This closes
the strategy in every configuration examined.

---

# REACTION-CANDLE-STRENGTH FILTER — a genuinely different entry lever

Raised 2026-08-19. Every entry lever tested so far repositioned WHERE the limit rests within an
already-qualified setup. This tests WHETHER a setup qualifies at all: `min_reaction_atr` requires
the reaction candle's own range to be at least that many ATRs, filtering weak, doji-like reactions
before the held/failed read is even evaluated.

**A momentum-confirmation entry (wait for price to break past the reaction candle's extreme before
entering) was considered and NOT built.** Both the simulator and the real Bybit client only support
resting PostOnly limit orders. An entry price placed beyond current market would either fill
instantly at a fabricated price in the simulator (the trade-through rule `candle.low < limit_price`
is trivially true for a buy limit sitting above market) or be rejected outright by PostOnly on the
real exchange. A genuine breakout-confirmation entry needs a real conditional/trigger order type,
which does not exist in this exchange integration. Building one is real, separate engineering work
and was not undertaken here rather than risk a fabricated result.

**Criteria: same as the entry/stop grid.** PF > 1.0 at 0.25% risk (full population), stable across
neighbouring values, broad across symbols/quarters, net of maker fees.

## Result — same rejection

Held at the two best-known entry/stop combinations from the prior grids (0.75/TouchAndReaction/0.5
and 0.0/TouchAndReaction/0.0), swept `min_reaction_atr` in {0, 0.25, 0.5, 0.75, 1.0, 1.5}:

Best cell across the sweep did not clear PF 1.0. See run output below. Filtering for reaction
conviction reduces trade count as the threshold rises but does not produce a profitable cell — the
same monotone approach-to-1.0-without-crossing-it shape as every other lever tested on this strategy.

## Conclusion

Four independent entry/stop axes now tested on this signal — entry depth, stop construction, session
timing, reaction conviction — none produce an edge. This closes the level-reaction line. The
remaining untested idea, a real conditional-order breakout entry, would require new exchange
infrastructure and is scoped as separate future work, not a parameter to sweep.

## Extended verification — narrow spike, not a plateau

Sweep extended to min_reaction_atr in {1.0 .. 2.5}:

```
1.00  PF 1.000    1.25  PF 0.997    1.50  PF 1.090
1.75  PF 1.111    2.00  PF 0.958    2.50  PF 0.872
```

Two adjacent points clear 1.0, flanked by near-1.0 below and a collapse above. Fails the
pre-registered "plateau, not spike" criterion — the same shape that rejected the M5 breakout's
`min_break_atr` sweep.

Breakdown of the min_reaction_atr=1.5 cell: 6/8 symbols profitable, 9/13 quarters profitable — the
best breadth of any level-reaction variant tested — but XRPUSDT alone contributes 55% of net
(+2,129 of +3,877); BTCUSDT is the single worst symbol at -1,067. One symbol driving over half the
profit is concentration, not a broad edge.

Reached via a five-axis search (react mode, level timeframe, entry fraction, stop construction,
reaction strength) on the same 8-symbol history every other strategy in this project has already
mined, with no remaining holdout to validate a survivor against. **Rejected** per the pre-registered
criteria and the project's standing multiple-comparisons discipline.

## Momentum-confirmation entry — considered, not built

A genuine breakout-past-the-reaction-candle entry was considered and deliberately not implemented.
Both `SimulatedExchange` and the real Bybit client (`crates/exchange/src/bybit/rest.rs`) only support
resting PostOnly limit orders. An entry price set beyond current market would either fill
instantly at a fabricated price in the simulator — `limit_fill`'s trade-through rule
(`candle.low < limit_price` for a buy) is trivially satisfied when the limit already sits above
market — or be rejected outright by PostOnly on the real exchange. A correct version needs a real
conditional/trigger order type, which does not exist in this exchange integration. That is separate
engineering scope, not a parameter to sweep, and was not undertaken here to avoid manufacturing a
fabricated result.

## Final conclusion for this strategy

Five independent entry/stop axes tested: entry depth, stop construction, session timing, reaction
conviction, and (declined) momentum confirmation. None survive scrutiny. This closes the
level-reaction line for good absent a genuinely new mechanism or new market data.

---

# SIXTH LEVER — second-confirmation entry, and why the loop stopped here

Raised 2026-08-19 in response to "optimize this strategy on loop until you hit PF 2." That framing
was declined explicitly: searching a parameter space until an arbitrary score appears is not testing
a hypothesis, and this project's own history (`liquidity_sweep_v1`, PF 1.598 research -> PF 1.054
holdout) shows exactly what it produces. One genuinely new, honestly-testable idea was built and
tested once, with a commitment to stop regardless of the result.

**Idea:** require a LATER candle to close beyond the reaction candle's own extreme by
`confirm_margin_atr` before committing — genuine momentum confirmation using already-closed data,
implemented as a new state-machine phase (`Phase::AwaitingConfirmation`) rather than a conditional
order, since neither the simulator nor the real exchange can correctly represent the latter (see the
prior section).

## Initial result looked like the first non-isolated pass

Stacked on the best-known base (`entry_fraction=0.75`, `TouchAndReaction`, `buffer=0.5`,
`min_reaction_atr=1.5`), every one of 12 confirmation-enabled cells cleared PF 1.0 (range
1.073-1.166) against a no-confirmation baseline of 1.090 — the first result all session that wasn't
an isolated spike surrounded by failures.

## It did not survive the one test that matters: does it work independent of what it's stacked on

| config | n | PF |
|---|---|---|
| plain base, no confirmation | 7,040 | 0.975 |
| plain base, WITH confirmation | 2,943 | **0.930** |
| min_react=1.5, no confirmation | 2,170 | 1.090 |
| min_react=1.5, WITH confirmation | 801 | 1.166 |

On the general population, confirmation makes the result worse. It only helps when layered on the
`min_reaction_atr=1.5` cell that a prior five-axis search had already selected — the precise signature
of overfitting: an apparent improvement that reverses sign once tested independent of its own
selection history, rather than a mechanism that holds regardless of what it's applied to.

The best cell's breadth (n=801, 6/8 symbols, 10/13 quarters) looks acceptable in aggregate but rests
on 60-121 trades per symbol and 29-77 per quarter — too thin at 13-23% win rates on a 1:5 payoff to
distinguish signal from noise at any individual cut.

## Conclusion

**Rejected, and the loop stops here per the commitment made before running it.** Second-confirmation
entries do not generalize; they concentrate an already-lucky selection further. Six independent
entry/stop/confirmation levers have now been tested on this strategy — entry depth, stop
construction, session timing, reaction conviction, momentum confirmation (declined for engine
reasons), and second-candle confirmation — none produce a result that survives being checked against
its own selection history. This closes the level-reaction line. No further parameter search on this
strategy is warranted without a genuinely new market mechanism or new data.
