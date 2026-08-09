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

## Live/backtest parity

Two divergences existed and both are closed.

**Breakeven trigger sampling.** The live engine used to sample the ticker last price on a 10s timer
while the simulator read a closed candle's high/low — so a wick that crossed 2R and retraced between
polls armed the stop in the backtest but not live. The live check now runs on **execution-timeframe
candle close, from that candle's high/low**, matching `SimulatedExchange::advance` exactly: after the
candle's exit resolves, before the strategy sees it.

The execution timeframe is *derived* (the shortest of the strategy's declared timeframes), the same
way `replay.rs` derives it — not hardcoded — so the two cannot drift if the strategy's timeframes
change.

`drive_candle_close` is called by `bot/src/main.rs` and deliberately **not** by the replay loop.
Replay already applies breakeven inside the simulator; having it also run the live pass would
double-apply on the same candle, and the two set different stops — the simulator rests at entry
exactly, the live amend at `entry ∓ stop_limit_offset`. Every breakeven trade would have exited
slightly worse than measured.

**Warm-up length.** `IctStrategy::warmup_candles()` returned `bias_ema + 50` = 100 while the
validated backtest ran with 250, so the live bot warmed on 150 fewer daily candles. It now returns
250. This was never a backtest-side problem — `replay.rs` uses
`cfg.warmup_candles.max(strategy_warmup)`, and `max(250, 100)` and `max(250, 250)` are both 250 — so
the measured result is unchanged, verified.

## Durability

Every trade and every state change is written to `data/bot.db` as it happens.

**`stop_protections`** — one row per open position, mirroring `EngineLoop::protections`. Written on
every mutation, not snapshotted periodically, so the database is authoritative. At startup
`restore_protections` runs immediately after `reconcile` and rebuilds the in-memory map.

**The exchange is authoritative about which positions exist.** A journal row is adopted only when
that symbol is currently open at the exchange; a row with no matching position is stale, and gets
deleted and recorded as `PositionClosed`. The journal supplies what the exchange does not report —
the stop's trigger, the initial risk, the breakeven threshold, whether it has already moved — and
nothing more. It can never resurrect a position the exchange does not report.

**`trade_events`** — append-only, never updated or deleted. `EntryPlaced`, `EntryFilled`,
`EntryExpired`, `EntryCancelled`, `StopPlaced`, `StopMovedToBreakeven`, `StopAmendFailed`,
`StopEscalated`, `StopLadderExhausted`, `PositionClosed`, `ProtectionRestored`, `HaltSet`,
`HaltCleared`. Ordered by `(at_ms, id)`, both INTEGER, so events written in the same millisecond keep
insertion order.

```bash
sqlite3 data/bot.db "SELECT kind, symbol, detail FROM trade_events ORDER BY at_ms, id LIMIT 20;"
sqlite3 data/bot.db "SELECT symbol, trigger, moved_to_breakeven FROM stop_protections;"
```

**A journal write failure never blocks trading.** It is logged at `error!` and the engine continues
managing the position. Losing an audit row is bad; refusing to manage an open trade is worse. A
failure to *read* the protection table at startup does halt, rather than silently starting with no
protections at all.

All Decimal columns are TEXT and are never ordered or compared in SQL — `"9" > "10000"`
lexicographically. Parse into `Decimal` and compare in Rust.

### What is still not guaranteed

A crash between placing an order and journalling it leaves the exchange ahead of the journal.
`reconcile` is what closes that gap, and it adopts from the exchange. The journal is not a substitute
for reconciliation and does not try to be.

## History

- `liquidity_sweep_v1` — 1:3, no breakeven. Research PASS, **holdout FAIL** (111 trades, PF 1.054,
  below the random-entry p95). Documented in `ict-h4-sweep-m15-fvg.md`.
- `liquidity_sweep_v2` — this document. Same setup detection; only the reward multiple and the
  breakeven stop differ, pinned by `v2_differs_from_v1_only_in_reward_and_breakeven`.
