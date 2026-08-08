# ICT Liquidity Sweep v2 — the strategy the bot trades

**Status:** live on testnet. **Not validated out of sample.** Read the provenance section before
reading the numbers.

## The rules

Sweep liquidity on H4, enter the fair value gap on M15, target 5R, pull the stop to entry at 2R.

| | |
|---|---|
| Structure timeframe | H4 |
| Execution timeframe | M15 |
| Bias | D1 and H4 must agree, 50-period EMA |
| Liquidity | fractal swings + previous day's high/low |
| Entry | 50% into the fair value gap, or an order block within 5 candles if no gap formed |
| Stop | the sweep candle's own extreme, no ATR buffer |
| Target | **5 × risk** |
| Breakeven | stop moves to entry once the trade reaches **2 × risk** in favour |
| Entry expiry | 12 M15 candles (three hours) |
| Market structure shift | not required |
| Session filter | none — trades around the clock |

Frozen in `IctParams::liquidity_sweep_v2()` and pinned by two tripwire tests. `config/testnet.toml`
and `config/mainnet.toml` are pinned to the same constructor by
`bot/tests/config_matches_frozen_strategy.rs`, so config and code cannot drift — they already did
once, with the config files driving `PullbackStrategy` at 1:2 while the measured strategy was ICT.

## Risk envelope

| | | why |
|---|---|---|
| Risk per trade | 1% | at 2% the strategy fails profit factor and drawdown, and the halt fires |
| Max concurrent | 8 | one per symbol; measured to saturate at 6 |
| Max daily entries | 5 | owner's 3–5/day rule. Measured **non-binding** — identical results at 5, 8 and 100 |
| Daily halt | −5% | |
| Total halt | −20% | raised from −15%: measured drawdown is 17.1%, so −15% would halt on a normal path |
| Universe | 8 pinned symbols | not screened — see below |

The universe is **pinned, not screened**. ADAUSDT, BNBUSDT, BTCUSDT, DOGEUSDT, ETHUSDT, LINKUSDT,
SOLUSDT, XRPUSDT. These are the symbols the strategy was measured on. Screening to 20 by turnover
would mean 8 concurrent positions drawn from an unmeasured universe, and 20 correlated positions at
1% each is 20% at risk simultaneously — the drawdown halt on its own.

## Research-window result

8 symbols, maker fee 0.0002, starting equity 10000, `data/history.db`.

```
n=298   win 24.2%   PF 1.457   maxDD 17.1%   net 18008
```

Against the previous configuration (1:3, no breakeven): PF 1.291 → 1.457, net 11454 → 18008.

Breakeven win rate at 1:5 is 16.7%, so 24.2% carries real margin — roughly 19% at the lower bound of
the confidence interval on 298 trades.

### Why it beats 1:3

Two independent effects, both supported by the MFE diagnostic:

- **The 5R target collects moves previously capped at 3R.** Winners were running past 3R and being
  cut off.
- **Breakeven at 2R converts reached-2R-then-reversed from −1R to 0R.** 16.3% of losing short trades
  reached +2R before turning; those were paying a full stop for a trade that had been well in profit.

### Robustness

| | 1:3 no breakeven | **1:5 BE@2R** |
|---|---|---|
| Profitable symbols | 8/8 (BTC +27 and DOGE +109 are noise) | 7/8 |
| **Profitable quarters** | **7/10** | **9/10** |
| Best quarter's share of net | 45% | **35%** |
| Net excluding the best quarter | 6326 | **11180** |

The gain is broad rather than concentrated: it nearly doubles net with the best quarter deleted, and
depends on that quarter *less* than the old config did. Two of three losing quarters flip positive
(2026Q1 −1216 → +369, 2025Q3 +126 → +1502). Improving consistency rather than just the total is the
signature of a structural fix rather than a curve fit — which is the main reason to believe it.

## Provenance — read this before trusting the numbers

**This configuration was selected by searching a 12-cell grid on the research window.** Reward
multiples 3/4/5/6 against breakeven thresholds none/1R/2R.

**The 330-day holdout is spent.** It was consumed by `liquidity_sweep_v1`, which failed it at
PF 1.054 after passing a five-criterion research gate. Pre-registration rules out re-running it with
changed parameters. **There is no clean validation sample left in this repository.**

So the numbers above are evidence about the research window and nothing else. The same process
produced a false positive once already.

**Cell noise is roughly ±20%.** The 1:4-no-breakeven cell scores *below* 1:3-no-breakeven, which
cannot be a real effect — it calibrates how much these cells wobble. The 1:3 → 1:5 gain of +59%
expectancy sits above that floor; the 1:6 row does **not** meaningfully beat 1:5.

**Testnet forward trading is the only uncontaminated evidence available**, and it takes months, not
a backtest run.

## What would falsify it

Stated in advance so it cannot be reinterpreted later:

- Win rate materially below 24% over 100+ testnet trades
- Drawdown past 20%
- Profit factor below 1.2 over a full quarter of forward trading

## Known live/backtest divergences

1. **Breakeven trigger sampling.** The backtest reads `candle.high`/`candle.low`; the live engine
   samples the ticker last price on its timer. A spike that crosses 2R and fully retraces between two
   polls moves the stop in the backtest but not live. The divergence runs toward trading *less*
   favourably than measured.
2. **Restart loses stop protections.** `EngineLoop::protections` is in-memory and populated only on
   candle close; nothing rebuilds it from `reconcile`. After a restart, an open position's breakeven
   and escalation records are gone. Pre-existing — it disables the escalation ladder across restarts
   too — and closing it needs journal persistence that has not been built.

## History

- `liquidity_sweep_v1` — 1:3, no breakeven. Research PASS, **holdout FAIL** (111 trades, PF 1.054,
  below the random-entry p95). Documented in `ict-h4-sweep-m15-fvg.md`.
- `liquidity_sweep_v2` — this document. Same setup detection; only the reward multiple and the
  breakeven stop differ, pinned by `v2_differs_from_v1_only_in_reward_and_breakeven`.
