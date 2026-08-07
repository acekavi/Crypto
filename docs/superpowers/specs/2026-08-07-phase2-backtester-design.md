# Phase 2: Backtester — Design

**Status:** awaiting review
**Depends on:** Phase 1 (complete — `botcore`, `indicators`, `exchange`, `persistence`, `strategy`, `risk`, `engine`, `bot`)

## The question this answers

Phase 1 built a bot that trades a baseline strategy. Nobody knows whether that
strategy makes money. Phase 2 exists to answer one question honestly:

> Does this strategy have an edge that survives fees, funding, and realistic
> fills — or does it only look good because the exit rules flatter noise?

The design is shaped throughout by the fact that a backtester's failure mode is
not "wrong number" but "convincing wrong number". Every choice below picks the
pessimistic option where reality is ambiguous.

## Architecture

The backtester replays **the identical live pipeline**. `EngineLoop` already
takes `Arc<dyn ExchangeClient>`, so Phase 2 supplies a `SimulatedExchange`
implementing that same trait instead of the Bybit REST client.

```
historical klines ─┐
funding rates    ─┼─► SimulatedExchange ─► EngineLoop ─► same strategy/risk/executor
recorded turnover─┘         (ExchangeClient)              as live trading
```

This is the central decision and everything else follows from it. Its
consequence: the owner's hard rules — limit orders only, 1:2 R:R, 1% risk, max 4
concurrent, max 5 entries/day, the drawdown halts — are **inherited, not
reimplemented**. A backtest cannot accidentally test a more permissive bot than
the one that trades. A second implementation of the sizing or gating logic would
be free to disagree with the live one, and the disagreement would be invisible.

**Cost accepted:** the live pipeline is async and event-driven, so replay is
slower than a purpose-built vectorised loop. For 1h/4h candles over a few years
across ~20 symbols this is seconds-to-minutes, not hours. Worth it.

### New crates

| Crate | Responsibility |
|---|---|
| `crates/history` | Download and store historical klines and funding rates; reconstruct the historical universe |
| `crates/backtest` | `SimulatedExchange`, the replay driver, walk-forward harness, metrics |
| `bin/` additions | `download-history`, `backtest` binaries |

`crates/history` is separate from `crates/backtest` because downloading is slow,
network-bound, and done rarely, while backtesting is fast, local, and done
constantly. Different failure modes, different test strategies.

## Data layer

### Storage

Historical OHLCV goes in **a separate SQLite database** (`data/history.db`), not
the trading journal. Bulk market data and the trade audit trail have different
sizes, retention needs, and backup requirements; mixing them would make the
journal — the record of what the bot actually did with real money — large and
awkward to inspect.

Schema, decimals as TEXT exactly as the journal does:

```sql
CREATE TABLE candles (
  symbol TEXT NOT NULL,
  timeframe TEXT NOT NULL,
  open_time_ms INTEGER NOT NULL,
  open TEXT NOT NULL, high TEXT NOT NULL, low TEXT NOT NULL, close TEXT NOT NULL,
  volume TEXT NOT NULL, turnover TEXT NOT NULL,
  PRIMARY KEY (symbol, timeframe, open_time_ms)
);

CREATE TABLE funding_rates (
  symbol TEXT NOT NULL,
  funding_time_ms INTEGER NOT NULL,
  rate TEXT NOT NULL,
  PRIMARY KEY (symbol, funding_time_ms)
);
```

The primary keys make re-downloading idempotent: an interrupted download resumes
without duplicating or corrupting rows.

### Downloader

Bybit's kline endpoint is paginated and rate-limited. The downloader reuses the
existing `RateLimiter` and retry/backoff from `crates/exchange` rather than
introducing a second HTTP path. It records, per symbol and timeframe, the range
already fetched, so a resumed run starts where it stopped.

**Gaps are recorded, never silently interpolated.** A missing candle range is
stored as a gap and reported; backtests refuse to run across a gap rather than
pretending price moved smoothly through it. This mirrors the live engine's
existing refusal to advance `last_open_ms` past an unfilled hole.

### Universe reconstruction — improving on the captured decision

The originally captured decision was a **fixed top-20 snapshot**, with a
mitigation owed for survivorship bias. This design does better.

Bybit klines carry per-candle `turnover`, so rolling 24h turnover is computable
historically for every symbol. The backtester therefore **reconstructs the
universe as it actually was at each point in time**, applying the same
`min_turnover_24h` floor and top-N ranking the live bot applies daily.

This eliminates two of the three biases:
- **Look-ahead universe bias** (trading symbols that were not yet liquid) — gone.
- **Thin-liquidity flattery** (filling against volume that did not exist) — gone;
  such periods now fall below the live floor and are never selected.

**Residual bias, stated honestly:** Bybit only serves klines for symbols that
exist *today*. A symbol that was in the top 20 and was later delisted cannot be
downloaded, so it is absent from the reconstructed universe. Results are
therefore biased toward symbols that survived. The magnitude cannot be measured
from Bybit data alone. This is disclosed in every report rather than buried —
it is the one bias this design cannot remove.

## SimulatedExchange and the fill model

### Trade-through required

A resting limit order fills **only if price traded strictly past it**, never on
touch:

- Buy limit at P fills only if `candle.low < P`
- Sell limit at P fills only if `candle.high > P`

Rationale: the live bot posts limit orders and sits behind the queue at its own
price level. When price merely touches a level, the orders ahead absorb the
volume and a latecomer does not fill. Touch-fill is the single most common way
backtests manufacture edge that does not exist.

Fill price is the **limit price**, never better. Price trading through does not
mean the order filled at the extreme.

### Intra-candle ambiguity — the pessimistic rule

OHLC gives no intra-candle path. When both the stop and the target are reachable
within the same candle, the true order is unknowable.

**Rule: assume the stop filled first.** Always. This is deliberately the worst
case. Any other choice (target-first, or proportional) systematically inflates
results, and the inflation is largest exactly on the volatile candles that
dominate returns.

The count of candles where this rule was invoked is reported. If it is a large
fraction of trades, the result depends heavily on an assumption rather than on
the data, and the report says so.

### Stop escalation in replay

A triggered stop places a limit order, so **the same trade-through rule applies
to it**: the stop-limit fills only if price traded strictly past it. Applying
one consistent fill rule everywhere is what keeps the model honest — a special
case for stops would be exactly where a hidden optimism could live.

This makes replay match live behaviour more closely than expected:

- Price gaps well through the stop → the stop-limit is traded through → fills.
  The gap loss is realised in full, which is the risk the limit-only rule
  deliberately accepts.
- Price triggers the stop but reverses without trading past the stop-limit →
  no fill → the ladder widens a rung and the next candle is re-evaluated.
- All rungs exhausted without a fill → **the position stays open**, exactly as
  live. It is not force-closed to tidy up the simulation.

**Known fidelity gap:** live rungs time out in ~10s, but replay can only widen
at candle boundaries, so a 1h candle gives each rung far longer than it gets in
reality. Replay therefore models the ladder as *more patient* than the live bot.
The number of trades whose exit depended on a widened rung is reported; if it is
material, escalation behaviour is being inferred rather than measured, and only
the testnet soak can settle it.

### Fees and funding

- **Maker fee** on every fill. The rate lives in the backtest config file, not
  in code, and is **sourced from Bybit's current USDT-perp fee schedule at
  implementation time and recorded there with the date it was checked**. No
  default is written into this spec deliberately: a number invented here would
  be indistinguishable from a verified one six months from now, and a stale fee
  assumption silently scales every result. Taker fees never apply — the bot
  places no market orders.
- **Funding** charged per 8h period a position is held, from **real downloaded
  funding-rate history**, signed by side (a long pays a positive rate, receives
  a negative one). Funding is a genuine cost of holding swing positions for days
  and omitting it would flatter every long in a bull market.
- **Slippage** is zero by construction: a limit order fills at its limit price
  or not at all. The real cost of limit-only execution is **non-fill**, which
  the trade-through rule already models. Stating this explicitly because a
  "0% slippage" line in a report otherwise looks like an oversight.

## Walk-forward harness

**6 months in-sample, 2 months out-of-sample, rolling.**

Parameters are tuned on each in-sample window; the following out-of-sample
window is measured with those parameters frozen. The window then rolls forward.
Roughly three years of history yields ~14 out-of-sample folds.

**Only concatenated out-of-sample results count as evidence.** In-sample numbers
are reported for diagnosis but are never the basis of a pass/fail decision, and
the report labels them as such. A single in-sample-only number is how a curve-fit
strategy gets deployed.

Fold boundaries are recorded so a suspicious fold can be inspected individually —
an edge that comes entirely from one lucky fold is not an edge.

## Random-entry benchmark

The strategy is compared against random entries using **identical** sizing,
exits, universe, position caps, daily caps, fees, and funding — the only
difference being when entries fire.

This separates two very different claims:
- "my entry signal predicts price" (real edge), and
- "1:2 R:R with a 1% risk cap makes money on any entry" (risk management
  flattering noise).

The benchmark runs over many seeds (default 100) to produce a distribution
rather than a single comparison. The strategy must beat the **95th percentile**
of that distribution. Beating the median proves almost nothing — with enough
random runs, some do well by luck.

## Metrics

Per fold and concatenated across out-of-sample folds: expectancy (in R and
USDT), profit factor, maximum drawdown, Sharpe, win rate, average win/loss,
trade count, time in market, total fees paid, total funding paid, and the count
of trades that hit the intra-candle ambiguity rule.

Fees and funding are reported as separate line items, not netted silently, so it
is visible when a nominally profitable strategy is actually paying its edge away
in costs.

## The pre-registered decision rule

Fixed now, before any result is seen. **All five must hold** on concatenated
out-of-sample results:

| Criterion | Threshold |
|---|---|
| OOS expectancy | > 0, net of fees and funding |
| OOS trade count | ≥ 200 |
| Max drawdown | ≤ 15% (matches the live halt) |
| Profit factor | ≥ 1.3 |
| Random-entry benchmark | strategy beats the 95th percentile |

Anything short of all five is a **FAIL**. No partial credit, no post-hoc
threshold adjustment. These numbers are committed to this document so that
changing them later is a visible edit to a reviewed file rather than a quiet
decision made while staring at a disappointing result.

A FAIL is a successful outcome for this phase: it means the tooling stopped a
losing strategy from reaching real money.

## Determinism

Identical inputs must produce byte-identical results, or no comparison between
runs means anything.

- No wall-clock time anywhere in replay; simulated time only.
- Random-entry seeds are explicit and recorded in the output.
- No iteration over `HashMap` where order can affect results.
- Results carry the config hash and the history-database range they were
  produced from.

A test asserts that the same backtest run twice produces identical output.

## Testing

Beyond unit coverage, the fill model and cost model carry oracle tests: a
hand-computed scenario (known candles, known limit prices, known funding rates)
with the expected fills, fees, funding, and final equity worked out by hand and
asserted exactly. These are the tests that catch a plausible-but-wrong
backtester, which is the whole risk of this phase.

Explicit negative controls:
- A strategy that never signals produces zero trades and zero costs.
- A limit order that is only *touched* does not fill.
- A position held across a funding timestamp is charged exactly once.

## Implementation sequencing

Too large for one plan. Three, each producing something independently useful and
testable — mirroring how Phase 1 was decomposed:

**Plan 2a — history layer (`crates/history`)**
Downloader, `history.db` schema, resumable paging, gap detection, funding-rate
download, and historical universe reconstruction from recorded turnover.
Deliverable: a populated database and a `download-history` binary. Verifiable on
its own — you can inspect what was downloaded before any backtest exists.

**Plan 2b — SimulatedExchange and replay (`crates/backtest`)**
`ExchangeClient` implementation over historical candles, the trade-through fill
model, the pessimistic intra-candle rule, fee and funding accounting, and the
replay driver wiring it to the existing `EngineLoop`. Deliverable: a single
backtest over one date range producing a trade list. This is where the oracle
tests live and is the highest-risk plan.

**Plan 2c — walk-forward, benchmark, and the decision rule**
Rolling window harness, the random-entry benchmark across seeds, the metrics
suite, and the pre-registered pass/fail gate. Deliverable: a `backtest` binary
that answers PASS or FAIL for a strategy.

Plan 2b is the one to be most careful with: a subtly wrong fill model produces
confident, plausible, wrong numbers, and every later result inherits the error.

## Out of scope

- Parameter optimisation beyond the walk-forward tuning loop (no genetic search
  or large grid sweeps — that is how overfitting gets industrialised).
- Multi-strategy portfolio allocation.
- Order-book / L2 simulation. Candle data cannot support it honestly.
- Live paper trading — that is the testnet soak, not the backtester.

## Honest limitations, collected

1. **Delisted symbols are absent** — results are biased toward survivors, by an
   amount that cannot be measured from Bybit data.
2. **Intra-candle path is unknown** — mitigated by always assuming the worst
   ordering, and by reporting how often it mattered.
3. **Escalation fidelity** — the ladder cannot be validated at candle
   resolution; the testnet soak validates it instead.
4. **Funding history depth** may be shorter than kline history for some symbols;
   affected ranges are excluded rather than assumed zero.
5. **A passing backtest is evidence, not proof.** It says the strategy did not
   fail these specific tests on this specific history. Position sizing on real
   money should still start small.
