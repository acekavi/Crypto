# Pairs Portfolio — `max_hold_bars` Tuning Note

**Research finding only. Not applied to `config/pairs-testnet.toml` or the running
`crypto-pairs.service`.** Nothing here changes live behavior until someone deliberately
edits the config and redeploys.

## How this came up

Investigating why the live testnet portfolio had taken zero real trades (`orders`/`fills`
empty) despite real signals firing led to: the two-leg entries were partially filling on
both legs, then the naked-leg guard was correctly unwinding them (`settle.rs`'s
`Settlement::Unwind`). A follow-up simulation — let the unfilled remainder rest until it
naturally traded through, then run the resulting whole position to its normal exit — found
those unwound entries would mostly have been profitable, and that the currently configured
time stops were doing real, necessary work in some cases and needlessly cutting off winners
in others. That led to sweeping `max_hold_bars` against the full historical dataset
(`data/history.db`) with a proper train/holdout split, which is what this note records.

## Method

For each pair, `run_backtest_with_experiment` (unmodified, from `bot::pairs_backtest`) was
run directly — via a throwaway binary, never a change to `crates/pairs` or
`bot/src/pairs_backtest.rs` — with only `max_hold_bars` overridden on a local clone of the
pair's `PairParams`. Everything else (`entry_z`, `stop_z`, `target_z`, `rolling_window`,
`fee_per_leg`, sizing) stayed exactly as configured.

- **AAVE/ETH**: fixed at 72 bars (up from the configured 48), per instruction.
- **ENA/XRP**: fixed at unlimited (time stop disabled), per instruction.
- **BNB/XAUT**: swept 18 candidates from 24 to 720 bars, **selected on the train segment
  only** (fixed-notional, requiring ≥10 train trades to qualify — thin samples excluded),
  then evaluated fresh on holdout. Holdout was never used to pick the candidate.

Split is the tool's existing default: 70% train / 30% holdout, one static split, not a
walk-forward. That matters for how much to trust this — see Limitations.

### bnb_xaut sweep (train segment, fixed-notional)

| max_hold_bars | train pf | trades | max_dd |
|---:|---:|---:|---:|
| 24 | 1.21 | 35 | 85.6% |
| 36 | 1.22 | 32 | 69.7% |
| **48** | **1.95** | **31** | **35.7%** ← selected |
| 60 | 1.53 | 30 | 55.6% |
| 72 (current default) | 1.65 | 29 | 43.7% |
| 96–120 | 1.4–1.6 | 26–28 | 63–92% |
| 144–192 | <1.0 (net negative) | 20–25 | 111–135% |
| 216–720 | mostly <1.0, one candidate hit 808% drawdown | 9–20 | — |

Quality degrades past ~72 bars and turns actively bad beyond 144 — a longer hold is not a
free upgrade for this pair, shorter is.

## Result: full-history train/holdout, tuned config

| pair | setting | segment | risk-sized (compounding) | fixed $25/leg |
|---|---|---|---|---|
| AAVE/ETH | 72 bars (was 48) | train | 74 trades, 74.3% win, pf 2.40, net +2688% | pf 7.86, net +$206 |
| | | **holdout** | 28 trades, 78.6% win, **pf 3.66**, net +209% | pf 3.75, net +$31 |
| | | full | 102 trades, 75.5% win, pf 3.07, net +8519% | pf 6.73, net +$238 |
| ENA/XRP | unlimited (was 120) | train | 28 trades, 39.3% win, pf 1.09, net +8% | pf 1.28, net +$8.04 |
| | | **holdout** | 12 trades, 83.3% win, **pf 11.06**, net +115% | pf 12.56, net +$27.58 |
| | | full | 40 trades, 52.5% win, pf 2.31, net +133% | pf 2.15, net +$35.62 |
| BNB/XAUT | 48 bars (was 72, sweep-selected) | train | 31 trades, 64.5% win, pf 1.75, net +32% | pf 1.95, net +$8.33 |
| | | **holdout** | 15 trades, 80.0% win, **pf 11.09**, net +31% | pf 10.95, net +$7.05 |
| | | full | 46 trades, 69.6% win, pf 2.58, net +73% | pf 2.62, net +$15.38 |

**No blow-ups on any pair, any segment** — the first tuning combination this session where
that's true. Under the current defaults, and under a uniform "no time stop"/"7-day" cap
tried earlier, AAVE/ETH's risk-sized backtest hard-failed (equity compounded to ≤0) on the
train and full segments, and BNB/XAUT's holdout carried a 438% single-trade drawdown. Both
are gone at this combination. All three holdout profit factors are strong and mutually
consistent (3.66 / 11.06 / 11.09), which hadn't happened together before in any combination
tried.

**AAVE/ETH sensitivity worth flagging:** at 168 bars (7 days) this same pair blew up the
same way it did at "unlimited." At 72 bars it's completely stable. Somewhere between 72 and
168 bars is a cliff edge for this pair specifically — 72 sits safely on the near side of it,
but the edge itself hasn't been located.

## Rerun against the bot's actual live window

Same tuned settings, rerun against the isolated `data/pairs-live-window-backtest.db`
(Bybit testnet, same data source the live bot trades against) over the bot's real run
window, 2026-09-05 15:00 UTC → present:

| pair | old config result | tuned config result |
|---|---|---|
| AAVE/ETH (72 bars) | 1 trade, target hit, net +0.1188 | **unchanged** — target hit well inside both 48 and 72 bars |
| ENA/XRP (unlimited) | 1 trade, target hit, net +0.1007 | **unchanged** — target hit at 58 bars, inside both 120 and unlimited |
| BNB/XAUT (48 bars) | 1 trade, **time stop @ 73 bars, net ‑0.0092** (small loss) | 1 trade, **time stop @ 48 bars (2026-09-07 16:00 UTC), net +0.0358** (win) |

Only `bnb_xaut` changes in this specific window, and favorably: the shorter cap forces the
exit at a point the spread happened to be more favorable than where the old 72-bar cap cut
it off (z=0.948 at 48 bars vs. having drifted back out by bar 73). `aave_eth`/`ena_xrp` are
unaffected here because both hit `target_z` well inside every hold-length tried, in this or
any earlier run this session — the time stop was never their binding constraint.

## Limitations — read before acting on this

- **One static 70/30 split, not a walk-forward.** The project's own `pairs_backtest
  --audit-walk-forward` exists specifically to stress a choice like `bnb_xaut`'s 48 bars
  across multiple rolling in-sample/out-of-sample folds instead of trusting one split. This
  hasn't been run yet for this combination.
- **Coarse grid.** The bnb_xaut sweep tested 18 discrete values, not a continuous search —
  48 is the best of those 18, not necessarily a true optimum.
- **`ena_xrp`'s "unlimited" and `aave_eth`'s "72" were given, not searched.** Only
  `bnb_xaut` went through selection; the other two settings were requested directly and
  evaluated, not chosen by any criterion.
- **Small holdout samples** (12–28 trades per pair) — enough to see the shape, not enough to
  treat any single profit factor as precise.
- **The live run-window check is n=3 trades total**, one per pair, and only one of the three
  (`bnb_xaut`) was even affected by the parameter change in that specific window.

## Status

Documented only. Applying this would mean editing `config/pairs-testnet.toml`
(`max_hold_bars` for all three bots) and redeploying `crypto-pairs.service` — a live-service
change, not made here.
