# Holdout Validation — Pre-Registration

**Written before the run. Nothing below may change after seeing the result.**

The 330-day holdout block has never been queried by anything in this project. Three studies were
run without opening it. This spends it.

## What is being tested

`IctParams::liquidity_sweep_v1()` — frozen, committed in `cc5c35b`, pinned by a tripwire test.

```
structure_tf        H4          execution_tf      M15
require_mss         false       use_pdh_pdl       true
use_session_levels  false       use_order_block   true
ob_lookback         5           fvg_entry_fraction 0.50
stop_buffer_atr     0.00        reward_multiple   3
session_filter      false       entry_expiry      12 execution candles
risk_pct            1%          max_concurrent    4
max_daily_entries   5           halts             -5% daily / -15% total
```

**Risk is 1%, not 2%.** Measured: at 2% the strategy fails on profit factor (1.247 vs 1.3) and
drawdown (23.78% vs 15%), and the trade count halves because the -15% halt fires and stops trading.
1% is not a conservative default here; it is the setting at which the halt and the position size are
compatible.

## Method

**Single pass over the holdout, not a walk-forward.**

The walk-forward exists to stop parameters being tuned on the data they are scored against. Nothing
is being tuned — the configuration is frozen and committed. Running a walk-forward on 330 days would
fit exactly one fold (180 in-sample + 60 out-of-sample) and yield roughly **24 trades**, which is
uselessly small. A single pass with a frozen config is both the honest test and the informative one.

## Expected sample, declared in advance

The research walk-forward produced 216 trades over 540 out-of-sample days: **0.40 trades/day**.
At that rate 330 days gives approximately **132 trades**.

## Criteria

| Criterion | Threshold | Applies |
|---|---|---|
| Expectancy | > 0, net of fees and funding | **Yes** |
| Profit factor | ≥ 1.3 | **Yes** |
| Max drawdown | ≤ 15% | **Yes** |
| Beats random-entry benchmark | > p99.17 of 500 seeded runs | **Yes** |
| Trade count | ≥ 200 | **CANNOT BE MET — see below** |

**The trade-count criterion cannot be satisfied on a 330-day window at 0.40 trades/day.** This is
stated now, before the result, so it cannot be reinterpreted afterwards. It is a limitation of the
validation window, not evidence about the strategy, and it is not being waived — the other four
criteria carry the verdict, and a pass on four of five is explicitly **weaker evidence** than a pass
on five of five would have been.

## What the outcomes mean

**All four applicable criteria pass** — the strategy survived data that never influenced any
decision. That is the strongest evidence this project can produce. It still is not proof: 132 trades
gives a win-rate confidence interval roughly ±8 points, the universe is 8 symbols on one venue, and
delisted symbols are absent. The next step would be a testnet soak, not real money.

**Any applicable criterion fails** — the strategy is rejected. The research-window PASS was then
in-sample luck, which is exactly what a holdout exists to detect.

## Ruled out in advance as responses to a failure

- Re-running with any parameter changed
- Running a second configuration against the holdout
- Extending the universe or date range to find a friendlier sample
- Relaxing any of the four applicable thresholds
- Re-reading the holdout for any reason

The holdout is one-shot. After this run it is spent, and no further result from it means anything.

## The honest prior

This configuration was reached by searching the research window repeatedly — entry expiry, liquidity
pools, order blocks, sweep timeframe, execution timeframe. Heavy search raises the chance that the
research PASS was noise. A failure here would be unsurprising and would not indicate anything was
done wrong; it is the process working.
