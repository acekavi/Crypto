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
