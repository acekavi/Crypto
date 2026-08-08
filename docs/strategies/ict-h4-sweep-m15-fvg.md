# ICT H4 Sweep → M15 FVG

**Status:** research candidate. Measured on the research window only; **never validated on held-out
data**. Not approved for testnet or live trading.

**Implementation:** `crates/strategy/src/ict.rs` (`IctStrategy`, `IctParams`)

---

## 1. What it trades

A three-part structural sequence:

1. Price sweeps a prior 4-hour swing level — takes out the liquidity resting beyond it — and
   **closes back through** the level, showing rejection rather than a break.
2. On the 15-minute chart, the move away from that sweep leaves a **fair value gap**: a
   three-candle imbalance price never traded through.
3. A limit order waits inside that gap. If price retraces into it, the trade is on.

The bet is that a swept level marks where the move exhausted, and that price returning to the
imbalance offers entry before continuation.

It is deliberately **not** a trend-continuation setup (the first strategy this project rejected) nor
a displacement-reversion setup (the second). Both of those were measured and failed; this one is a
structural sequence and fails differently, which is the only reason it is worth testing.

---

## 2. Timeframe roles

| Timeframe | Role |
|---|---|
| **D1** | Directional bias — close above/below the 50-period EMA |
| **H4** | Bias confirmation **and** where sweeps are detected |
| **M15** | Execution — fair value gap detection and entry placement |

D1 and H4 must **agree**. When they disagree the symbol is not traded; there is no partial-alignment
tier, because that would be another tunable knob for no mechanistic reason.

H4 serves two roles at once. `IctStrategy::timeframes()` therefore subscribes to `[M15, H4, D1]` —
H4 appears once, not twice. There is a regression test pinning this, because an earlier version
returned a hardcoded list and a variant configured for 5-minute execution **never received a single
5-minute candle**. The funnel reported zero and it looked like "the setup is rare" rather than "the
wiring is broken."

---

## 3. Exact rules

Every definition below is written out because ICT practitioners formalise these differently. A
result validates **these** definitions, not "ICT" as a body of ideas.

### 3.1 Swing point

A candle is a **swing high** if its high strictly exceeds the highs of the `swing_lookback` candles
on *both* sides. A **swing low** is the mirror.

Requiring both sides means the most recent `swing_lookback` candles can never yet be swing points.
That is the cost of a *confirmed* structure rather than a guessed one. Equal highs do not qualify —
a double top is not a single swing point.

### 3.2 Liquidity sweep

On the 4-hour chart:

```
bullish sweep:  candle.low  <  prior swing low   AND  candle.close >  prior swing low
bearish sweep:  candle.high >  prior swing high  AND  candle.close <  prior swing high
```

**The close is the whole rule.** A candle that closes *past* the level has broken it, which is the
opposite event and must not arm a trade in the opposing direction. Merely touching the level is not
a sweep either — the low must go strictly beyond.

Naming follows the **direction traded**: a bullish sweep takes out lows and then buys.

### 3.3 Market structure shift — optional, and off here

Originally the sweep had to be confirmed by a close through the opposing swing within
`mss_window` candles. Measured across 769 days and 8 symbols, **only 3.2% of sweeps ever produced
one**, which starved the strategy of trades (15 signals in 769 days).

`require_mss: false` in this configuration. The sweep alone arms the setup.

This is the single change that unlocked measurable frequency, and it is a real weakening of the
setup: a sweep without structural confirmation is a weaker signal. That trade-off is the reason this
configuration is a research candidate rather than the pre-registered study.

### 3.4 Fair value gap

On the 15-minute chart, across three consecutive candles:

```
bullish FVG:  candle1.high  <  candle3.low     gap = (candle1.high, candle3.low)
bearish FVG:  candle1.low   >  candle3.high    gap = (candle3.high, candle1.low)
```

Only gaps facing the setup's direction count. A bearish setup enters a gap *above* price; accepting
a bullish-shaped gap would mean trading into the move being faded.

### 3.5 Entry

A limit order at `fvg_entry_fraction` depth into the gap, measured from **the edge price reaches
first** as it retraces:

```
bullish:  entry = fvg.high - (fvg.high - fvg.low) * fraction
bearish:  entry = fvg.low  + (fvg.high - fvg.low) * fraction
```

At 0.5 that is the gap midpoint.

The order expires after `entry_expiry_candles` (3) **execution candles** — 45 minutes on M15. This
matters more than it looks: on 5-minute execution the same setting is only 15 minutes, and most
orders expire unfilled. It is a major reason signal counts and trade counts diverge so sharply.

### 3.6 Stop

At the **sweep candle's own extreme**, offset by `stop_buffer_atr × ATR(14)` on the execution
timeframe:

```
bullish:  stop = sweep_candle.low  - buffer
bearish:  stop = sweep_candle.high + buffer
```

With `stop_buffer_atr: 0` the stop sits exactly on the level price rejected — the structural
invalidation point. Price returning there means the sweep was not a sweep.

**Consequence worth understanding:** risk per trade is set by a *4-hour* candle's range while the
entry is placed with *15-minute* precision. Stops are therefore wide, position sizes correspondingly
small, and a 3R target is a large move to ask for.

### 3.7 Target

`reward_multiple × R`, where R is measured **entry to stop-limit**, not entry to stop trigger.

That distinction is not cosmetic. A stop fills at the stop-limit, which sits beyond the trigger by
the ATR offset. Sizing on the trigger distance under-states the loss: a nominal 1R realised as 1.2R,
turning a nominal 1:2 into a realised 1.60 and silently moving the breakeven win rate from 33.3% to
38.4%. Fixed in `2d19d2d`; see `crates/risk/src/manager.rs`.

---

## 4. Parameters — best known configuration

| Parameter | Value | Meaning |
|---|---|---|
| `structure_tf` | `H4` | Where sweeps are detected |
| `execution_tf` | `M15` | Where entries are placed |
| `bias_ema` | 50 | D1 and H4 bias EMA period |
| `swing_lookback` | 5 | Candles either side of a swing point |
| `require_mss` | `false` | Structure shift not required |
| **`use_pdh_pdl`** | **`true`** | **Also sweep previous-day extremes** |
| `use_session_levels` | `false` | Session extremes rejected — see §5.2 |
| **`use_order_block`** | **`true`** | **Fall back to an order block when no FVG** |
| **`ob_lookback`** | **5** | Execution candles searched for an order block |
| `fvg_entry_fraction` | 0.50 | Depth into the zone |
| `stop_buffer_atr` | 0.00 | Stop exactly at the sweep extreme |
| `atr_period` | 14 | ATR period on the execution timeframe |
| `reward_multiple` | 3 | Target as a multiple of R |
| `session_filter` | `false` | No session restriction |
| `entry_expiry_candles` | **12** | Backtest config, not a strategy param |

Constructed as:

```rust
IctParams {
    execution_tf: Timeframe::M15,
    use_pdh_pdl: true,
    use_order_block: true,
    ob_lookback: 5,
    ..IctParams::h4_sweep_m5_entry()
}
// with BacktestConfig { entry_expiry_candles: 12, .. }
```

### Inherited risk rules — not configurable here

These live in `RiskManager` and are applied identically to every strategy, live and backtested:

- **Limit orders only.** No market orders exist anywhere in the workspace; a source-level test fails
  the build if the literal appears.
- 1% of equity risked per trade
- Maximum 4 concurrent positions, 1 per symbol
- Maximum 5 filled entries per UTC day
- Halt at −5% daily / −15% total drawdown, persisted across restarts

---

## 5. Measured results — research window only

769 days, 8 symbols (ADA, BNB, BTC, DOGE, ETH, LINK, SOL, XRP), Bybit USDT perpetuals, mainnet
history. Maker fee 0.02%, funding charged from real history. The final 330 days are **excluded and
have never been queried**.

### 5.1 How the configuration was reached — each change measured alone

Starting point was 28 trades. Every change below was applied and measured
individually, so the marginal contribution of each is visible rather than only
the combined result. **Two of the four proposed changes were rejected.**

| Change | n | win% | PF | maxDD | net | Verdict |
|---|---|---|---|---|---|---|
| Baseline (expiry 3) | 28 | 35.7 | 1.556 | 6.9% | +1,114 | — |
| Entry expiry → 12 | 41 | 36.6 | 1.573 | 12.6% | +1,807 | **adopt** |
| + PDH/PDL | 119 | 38.7 | 1.670 | 16.0% | +7,765 | **adopt** |
| + order block (5) | **200** | 35.5 | **1.470** | 16.3% | **+10,546** | **adopt** |
| + session levels | 633 | 29.2 | 1.162 | 27.2% | +9,869 | **reject** |
| Sweep on H1 not H4 | 66 | 21.2 | 0.762 | 16.6% | −1,227 | **reject** |

**28 → 200 trades, profit factor holding at 1.47.**

### 5.2 Why the two rejections matter

**Session levels** produce the most trades (633) and the highest raw profit,
and are still the wrong choice: profit factor collapses to 1.162 — below the
1.3 gate — and drawdown reaches 27%, well past the 15% limit. More trades, much
worse trades.

**Sweeping on H1** was expected to be a cheap win and is not. It gives more
trades and *destroys* the edge (PF 0.762, net −1,227). The 4-hour sweep level is
load-bearing: a 4h swing is a level participants watch, an hourly one is noise.

Worth recording that this was predicted wrong. H1 was ranked "low risk, already
measured" — and it *had* been measured, at net −958 in an earlier table. The
data was available and the recommendation was made anyway. Measuring one change
at a time is what caught it.

### 5.3 The best configuration

```
trades          200
win rate        35.5%
profit factor   1.470
expectancy      +52.73 per trade
net             +10,546 on 10,000 starting equity
max drawdown    16.3%
```

Breakeven at a true 1:3 is 25%.

There is a second candidate worth keeping: **without** the order block fallback
the same config gives 119 trades at PF **1.670** and drawdown 16.0%. Order
blocks buy 81 trades for 0.2 of profit factor. Neither dominates — it is sample
size against quality, and both clear the 1.3 bar.

### Robustness — the reason this is a candidate at all

**Parameter neighbourhood.** A genuine edge is a plateau; an overfit is a spike.

| Config | n | net | PF |
|---|---|---|---|
| `swing_lookback` 3 | 43 | +4,682 | 2.748 |
| `swing_lookback` 5 *(base)* | 28 | +1,114 | 1.556 |
| `swing_lookback` 8 | 13 | +2,012 | 4.601 |
| `fvg_entry_fraction` 0.25 | 34 | +438 | 1.165 |
| `fvg_entry_fraction` 0.75 | 24 | +296 | 1.161 |
| `stop_buffer_atr` 0.25 | 31 | +768 | 1.331 |
| `reward_multiple` 2.5 | 28 | +962 | 1.516 |
| `reward_multiple` 3.5 | 28 | +1,144 | 1.538 |

**8 of 8 neighbours profitable.** This is the property that distinguishes it from every other
configuration tested in this project, where nudging one setting routinely flipped the sign.

**Across time.** Q1 0 trades (warm-up), Q2 +849, Q3 +551, Q4 +245 — every active quarter positive.

**Across symbols.** 5 of 8 profitable (ADA, BNB, DOGE, LINK, SOL); BTC, ETH and XRP negative. Not
carried by a single instrument.

---

## 6. Limitations — read before believing any of the above

1. **Sample size.** 13–43 trades depending on configuration. Resolving a 10-point edge at 95%
   confidence needs roughly **180**. Every confidence interval here is ~30 points wide and contains
   both "excellent" and "loses money."

2. **The neighbours are not independent.** Nudging a parameter re-uses mostly the same market
   events, so "8 of 8 profitable" evidences *stability*, not eight confirmations. It is much weaker
   than the count suggests.

3. **This configuration was selected by search.** The research window has been examined
   repeatedly — the pullback post-mortem, the reversion study, the ICT study, then this exploration.
   Every additional look raises the chance of finding something that is only noise.

4. **Not the pre-registered study.** `docs/superpowers/specs/2026-08-08-ict-study-design.md` fixed
   H1 sweeps, M15 execution, required MSS, NY session, 1:2. This config changes four of those. The
   pre-registration's protections do not extend to it.

5. **Delisted symbols are absent** from Bybit's kline history, so results skew toward survivors by
   an amount this data cannot measure.

6. **Intra-candle path is unknown.** Where a candle could hit both stop and target, the stop is
   assumed first. Pessimistic, but still an assumption.

7. **The maker fee is corroborated, not primary-sourced** — see `config/backtest.toml`.

8. **Not live-validated.** The stop-escalation ladder and restart behaviour are only exercised by a
   testnet soak, which has not run.

---

## 7. What would make this trustworthy

**One holdout run.** 330 days, never queried, one shot. That is the only clean test available.

If run, the recommended configuration is **`swing_lookback: 3`** — chosen for the **largest sample**
(43 trades), not the highest return. Selecting on sample size rather than peak performance is the
criterion least likely to be picking noise, and the entire neighbourhood is profitable so any choice
within it is defensible.

The pass bar would be the pre-registered gate with the study's tightened benchmark:
expectancy > 0, ≥200 trades, max drawdown ≤ 15%, profit factor ≥ 1.3, and beating the 99.17th
percentile of 500 seeded random-entry runs.

**Two criteria are at risk before the run even starts, and both should be understood now:**

**Drawdown.** The best configuration measures 16.3% on a single full-window pass,
above the 15% limit. That is not the same measure the gate uses — the gate takes
the worst *fold* of a walk-forward, where each fold restarts at its own equity —
but it is close enough to the line to fail on this criterion alone.

**Trade count.** 200 trades came from 769 days. The holdout is 330 days, roughly
43% of that, so it should produce **around 85–110 trades** — well short of the
200 the gate requires. The criterion cannot be met on a window that short at this
frequency.

That is a structural problem with testing this strategy against this gate, not a
technicality to waive. The options are to accept that the holdout cannot satisfy
a 200-trade bar and say so explicitly, or to design a different validation. What
must **not** happen is quietly lowering the threshold after seeing the result.

---

## 8. Reproducing these numbers

```bash
# History (public endpoints; no credentials, read-only)
BYBIT_ALLOW_MAINNET=1 cargo run --release --bin download-history -- \
  --profile mainnet --days 1100 \
  --symbol BTCUSDT --symbol ETHUSDT --symbol SOLUSDT --symbol XRPUSDT \
  --symbol BNBUSDT --symbol DOGEUSDT --symbol ADAUSDT --symbol LINKUSDT

# Baseline and robustness
cargo test -p backtest --release --test diag_h4m15_robust -- --ignored --nocapture

# Frequency funnel across configurations
cargo test -p strategy --release --test diag_h4_sweep -- --ignored --nocapture
```

Backtests are deterministic: identical inputs produce byte-identical output. That was not always
true — resting-order fills once depended on `HashMap` iteration order and the same command produced
255, 256 and 257 trades on successive runs. Fixed in `46499fb` by price priority with a
`order_link_id` tiebreak.

---

## 9. Related documents

- `docs/superpowers/specs/2026-08-08-ict-study-design.md` — the pre-registered study
- `docs/superpowers/specs/2026-08-07-phase2-backtester-design.md` — backtester design and gate
- `.superpowers/sdd/2026-08-07-phase2c-walk-forward-and-gate/entry-research-report.md` — why the
  trend strategy failed
