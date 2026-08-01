# Bybit Trading Bot — Phase 1 Design

**Date:** 2026-08-01
**Status:** Approved
**Scope:** Phase 1 only — trading engine running unattended on Bybit testnet.

---

## 1. Goal and non-goals

Build a Rust trading engine that runs 24/7 against Bybit testnet, evaluates a
pluggable strategy on 1h/4h candle closes across a dynamically ranked universe of
USDT perpetuals, and executes trades under hard risk limits it cannot override.

Phase 1 succeeds when the bot survives a two-week unattended testnet soak without
losing track of a position, opening a duplicate order, or holding an unprotected
position — **not** when it is profitable. Profitability is Phase 2's question.

**Non-goals for Phase 1:** backtester (Phase 2), web dashboard (Phase 3), mainnet
trading (Phase 4), spot, options, multi-account, machine learning.

### Project phases

| Phase | Delivers | Success condition |
|---|---|---|
| 1 | Engine + testnet live loop | Two-week unattended soak, no state divergence |
| 2 | Historical downloader, replay backtester, fee/funding/slippage model, metrics | A strategy with a measured edge and calibrated risk numbers |
| 3 | Axum web dashboard: equity curve, positions, trade history, kill switch | Observable and controllable from a browser |
| 4 | Mainnet hardening: canary sizing, live-vs-backtest drift monitoring, alerting | Real capital deployed deliberately |

Phase 1 is deliberately built before Phase 2 because unattended bots fail at
plumbing — dropped sockets, restarts that orphan positions, stops that were never
placed, duplicate orders on retry. Testnet is free, so exercising the engine there
with an unvalidated strategy is the cheapest way to surface those failures.

---

## 2. Decisions taken

| Decision | Choice | Rationale |
|---|---|---|
| Language / runtime | Rust, tokio | User requirement; gives low resource footprint and reliable long-running processes |
| Cadence | Decisions on 1h/4h candle close | Swing horizon; latency is not a competitive factor |
| Market | Bybit USDT perpetuals (`category: "linear"`), long and short | Shorts roughly double setup frequency |
| Leverage | Configurable, above 3x permitted | User decision, made with the liquidation risk stated |
| Position sizing | Risk-based: `(1% × equity) ÷ stop-distance` | Makes leverage a margin-efficiency setting rather than a risk multiplier |
| Risk envelope | 1% per trade, max 4 concurrent, halt at −5% daily or −15% total drawdown | Standard systematic swing defaults |
| Universe | Daily re-rank of perps by 24h turnover, with floors | User choice; floors mitigate drift into illiquid pairs |
| Architecture | Event-driven tokio pipeline, Cargo workspace | Phase 2 backtester reuses the identical pipeline via trait swaps |
| Exchange client | Thin in-house Bybit V5 client | ~11 endpoints; needs exact control of idempotent retries, rate limiting, and `Decimal` handling. The official `bybit-rust-api` SDK has negligible adoption (~1 star), which is a poor basis for leveraged order placement |
| Baseline strategy | Trend-filtered pullback | Unambiguous to code and cheap to falsify in Phase 2 |
| Observability | Structured logs + SQLite journal in Phase 1; web dashboard in Phase 3 | Dashboard is meaningful scope, deferred so the core lands first |

---

## 3. Architecture

### 3.1 Workspace layout

```
crypto-bot/
├── Cargo.toml                 # workspace manifest
├── crates/
│   ├── core/                  # domain types, no I/O, no dependencies on other crates
│   ├── exchange/              # ExchangeClient + MarketFeed traits; Bybit V5 implementation
│   ├── indicators/            # EMA, RSI, ATR — incremental, O(1) per update
│   ├── strategy/              # Strategy trait + baseline implementation
│   ├── risk/                  # position sizing, hard limits, kill switches
│   ├── engine/                # event loop, CandleStore, Executor, Reconciler
│   └── persistence/           # SQLite journal via sqlx
└── bot/                       # binary: config loading, wiring, graceful shutdown
```

Dependency direction is strictly downward: `core` depends on nothing internal;
`indicators`, `exchange`, `persistence` depend only on `core`; `strategy` depends on
`core` + `indicators`; `risk` on `core`; `engine` on all of them; `bot` wires them.
No cycles.

### 3.2 Data flow

```
WS kline ──▶ CandleStore ──▶ StrategyEngine ──▶ Signal ──▶ RiskManager
                  ▲                                            │
             REST gap-fill                                OrderIntent
                                                               ▼
WS private ─▶ PositionTracker ◀── Reconciler          Executor ──▶ Bybit
                  │                                        │
                  └────────────▶ SQLite journal ◀──────────┘
```

Each box is an independent tokio task. Market data fans out over
`tokio::sync::broadcast`; commands flow over bounded `tokio::sync::mpsc`. Channels
are bounded deliberately: if a consumer falls behind, the engine logs the lag and
blocks new entries rather than silently dropping events.

### 3.3 Core traits

```rust
#[async_trait]
pub trait ExchangeClient: Send + Sync {
    async fn instruments(&self) -> Result<Vec<Instrument>>;
    async fn tickers(&self) -> Result<Vec<Ticker>>;
    async fn klines(&self, s: &Symbol, tf: Timeframe, n: u16) -> Result<Vec<Candle>>;
    async fn place_entry(&self, req: EntryOrder) -> Result<OrderAck>;
    async fn amend_stop(&self, s: &Symbol, px: Decimal) -> Result<()>;
    async fn close_position(&self, s: &Symbol) -> Result<()>;
    async fn positions(&self) -> Result<Vec<Position>>;
    async fn open_orders(&self) -> Result<Vec<OpenOrder>>;
    async fn set_leverage(&self, s: &Symbol, lev: Decimal) -> Result<()>;
    async fn balance(&self) -> Result<Balance>;
}

#[async_trait]
pub trait MarketFeed: Send + Sync {
    async fn subscribe(&self, subs: &[Subscription]) -> Result<Receiver<MarketEvent>>;
}

pub trait Strategy: Send {
    fn timeframes(&self) -> &[Timeframe];
    fn warmup_candles(&self) -> usize;
    fn on_candle_close(&mut self, ctx: &MarketContext) -> Option<Signal>;
}
```

`Signal` carries direction, stop price and target price — **never a quantity**. The
strategy has no knowledge of account equity. `RiskManager` alone converts
`Signal → OrderIntent` by attaching size. This keeps sizing bugs out of strategy
code and lets Phase 2 replay any strategy against any equity curve.

In Phase 2, `HistoricalFeed` and `SimulatedExchange` implement these same two
traits, so the backtester drives the identical pipeline. This is the only structure
under which backtest results say anything about live behaviour.

---

## 4. Bybit V5 integration

### 4.1 Endpoints used

| Purpose | Endpoint |
|---|---|
| Instrument metadata (tick size, qty step, min notional) | `GET /v5/market/instruments-info` |
| Universe ranking (24h turnover) | `GET /v5/market/tickers` |
| Historical candles / gap fill | `GET /v5/market/kline` |
| Place entry with attached stop | `POST /v5/order/create` |
| Cancel order | `POST /v5/order/cancel` |
| Open orders (reconciliation) | `GET /v5/order/realtime` |
| Positions (reconciliation) | `GET /v5/position/list` |
| Adjust stop on an open position | `POST /v5/position/trading-stop` |
| Set leverage | `POST /v5/position/set-leverage` |
| Equity | `GET /v5/account/wallet-balance` |

Base URLs: testnet `https://api-testnet.bybit.com`, mainnet `https://api.bybit.com`.

WebSocket: public `wss://stream-testnet.bybit.com/v5/public/linear` (topic
`kline.{interval}.{symbol}`), private `wss://stream-testnet.bybit.com/v5/private`
(topics `order`, `position`, `execution`, `wallet`). Mainnet hosts are
`stream.bybit.com`. A `{"op":"ping"}` heartbeat is sent every 20 seconds.

### 4.2 Authentication

REST requests carry `X-BAPI-API-KEY`, `X-BAPI-TIMESTAMP` (ms), `X-BAPI-RECV-WINDOW`
and `X-BAPI-SIGN`. The signature is `HMAC_SHA256(timestamp + api_key + recv_window +
(query_string | json_body))`, lowercase hex. `recv_window` defaults to 5000 ms.
Bybit requires `server_time − recv_window ≤ timestamp < server_time + 1000`, so the
client tracks clock offset against the server's response `time` field and corrects
for drift rather than trusting local time.

The private WebSocket authenticates with `{"op":"auth", "args":[api_key, expires,
HMAC_SHA256("GET/realtime" + expires)]}`.

### 4.3 Order placement

Entries are `orderType: "Market"`, `category: "linear"`, `positionIdx: 0` (one-way
mode), with `stopLoss`, `takeProfit`, and `slTriggerBy: "MarkPrice"` set in the same
request. Attaching the stop to the entry request — rather than as a follow-up call —
is what guarantees no window exists in which a position is open without protection.

`slTriggerBy` uses mark price, not last price, because Bybit liquidates on mark
price and last-price wicks would otherwise trigger stops that liquidation logic
ignores.

---

## 5. Safety invariants

Enforced in `risk` and `engine`. Strategy code cannot override any of them.

| # | Invariant | Failure it prevents |
|---|---|---|
| 1 | Every entry order carries an exchange-native stop in the same request | Crashed or disconnected bot leaves an unprotected leveraged position |
| 2 | Reject any order whose liquidation price is nearer than its stop | High leverage silently converting a normal drawdown into liquidation |
| 3 | `size = (risk_pct × equity) ÷ stop_distance`, rounded **down** to `qtyStep` | Rounding that increases risk; oversized positions |
| 4 | Reject if resulting notional < instrument minimum or > available margin | Silent exchange rejects mid-strategy |
| 5 | `orderLinkId` = deterministic hash of (symbol, signal candle timestamp, direction), ≤36 chars | A retry after a network timeout opening a second position |
| 6 | Max 4 concurrent positions, max 1 position per symbol | Correlated over-exposure |
| 7 | Halt on −5% daily or −15% total drawdown; halt state persisted, cleared only by a human | A losing streak compounding; a restart un-halting itself |
| 8 | Block new entries when no candle has arrived for a subscribed symbol within 2× its timeframe | Trading on stale candles after a silent socket death |
| 9 | All monetary and quantity values are `rust_decimal::Decimal` | `f64` drift producing wrong sizes and rejected orders |
| 10 | Mainnet requires an explicit profile flag and confirmation env var | Accidentally trading real money |

**Halt definitions.** *Daily drawdown* is measured against account equity at the most
recent 00:00 UTC boundary. *Total drawdown* is measured against the all-time
high-water mark of equity, persisted across restarts. Reaching either threshold sets
the persisted halt flag: open positions keep their exchange-side stops and targets
and are left to resolve, but no new entries are placed until a human clears the flag.

**On leverage:** invariants 2 and 3 together mean leverage changes only how much
margin a position locks, not how much is risked. Risk per trade stays 1% of equity
regardless of the leverage setting. Leverage becomes dangerous only when sizing is
derived from available margin instead of stop distance, which this design never does.

---

## 6. Baseline strategy

**Trend-filtered pullback.** This is a starting point for exercising the engine, not
a validated edge. No claim is made about its profitability; Phase 2 determines
whether it or anything else reaches mainnet.

| Element | Rule |
|---|---|
| Bias filter (4h) | Long bias if EMA50 > EMA200; short bias if EMA50 < EMA200; otherwise no trade |
| Pullback (1h) | Within the last 5 closed candles, the low (long) or high (short) came within 0.5 × ATR(14) of EMA20 |
| Trigger (1h) | On the closing candle, RSI(14) crosses from below 40 to at or above 40 (long), or from above 60 to at or below 60 (short) |
| Volatility gate | ATR(14) ÷ close must fall in [0.3%, 5.0%] |
| Entry | Market order on 1h candle close, in the 4h bias direction only |
| Stop | The further from entry of: the lowest low (long) or highest high (short) of the last 10 closed candles, or 1.5 × ATR(14) |
| Target | 2R, attached as `takeProfit`; stop moved to breakeven once price reaches 1R |
| Exit | Exchange-side stop or target only — no discretionary exit logic in Phase 1 |

"R" is the distance between entry price and stop price. All thresholds (EMA periods,
RSI levels, ATR multiple and band, lookback lengths, R multiple) are config values,
not constants, so Phase 2 can sweep them without code changes. The values above are
the defaults, chosen as conventional starting points — they are not tuned, and
tuning them is Phase 2 work.

### Universe selection

Re-ranked daily at 00:00 UTC by 24h turnover from `GET /v5/market/tickers`, taking
the **top 20** symbols that pass all filters: 24h turnover ≥ 50,000,000 USDT, listed
at least 30 days, and `status == "Trading"`. Symbols with an open position are never
dropped from the universe mid-trade regardless of ranking changes. Universe size and
filter thresholds are config values.

Phase 2 backtests pin a fixed universe snapshot instead of re-ranking, because
dynamic ranking over historical data embeds survivorship bias.

---

## 7. State, recovery and error handling

### 7.1 Persistence

SQLite via `sqlx`, with these tables: `orders` (intent through terminal state),
`fills`, `positions` (bot's view), `equity_snapshots` (for drawdown tracking),
`halt_state`, and `candles` (rolling window for warmup after restart).

### 7.2 Startup reconciliation

The exchange is the source of truth in every disagreement. On every startup, before
any strategy evaluation:

1. Fetch live positions and open orders.
2. Diff against the journal.
3. Adopt positions the journal doesn't know about; mark journal positions the
   exchange doesn't have as closed, backfilling their outcome from execution history.
4. Verify every open position has a stop attached. If one does not, place it
   immediately; if that fails, flatten the position.
5. Recompute drawdown state and re-apply any persisted halt.
6. Warm up indicators from stored plus refetched candles before accepting signals.

### 7.3 Error classification

| Class | Examples | Handling |
|---|---|---|
| `Retryable` | Network error, 5xx, rate limit (`retCode` 10006/10018) | Exponential backoff with jitter, capped attempts; idempotent via `orderLinkId` |
| `Rejected` | Invalid params, insufficient margin, min-qty violation | Log with full context, skip the signal, continue |
| `Fatal` | Auth failure, invalid API key, account restricted | Halt trading, alert, require human intervention |

WebSocket disconnects trigger reconnect with backoff, re-subscription, and a REST
refetch of recent klines to fill any gap in the candle series. Rate limiting is
enforced client-side by a token-bucket limiter per endpoint group, sized below
Bybit's published limits, so retries never cascade into a ban.

---

## 8. Testing strategy

| Layer | Approach |
|---|---|
| Indicators | Fixture tests against known-good EMA/RSI/ATR values; incremental output must equal batch-computed output |
| Sizing | `proptest` over random equity, price, stop distance, tick size and qty step, asserting realized risk never exceeds the budget and quantities always satisfy exchange constraints |
| Strategy | Golden-file tests: a fixed candle series produces an exact expected signal sequence |
| Engine | `MockExchange` implementing both traits, driving the full pipeline through order rejects, WebSocket disconnects, duplicate acks, and restarts while a position is open |
| Reconciliation | Explicit tests for each divergence case: orphan position, phantom position, missing stop, stale halt |
| End-to-end | Testnet smoke run, then a two-week unattended soak with restart injection |

Tests assert observable behaviour — orders placed, state transitions, invariants
upheld — not internal call sequences.

---

## 9. Configuration and secrets

Configuration is TOML with per-profile files (`config/testnet.toml`,
`config/mainnet.toml`) covering symbol filters, strategy parameters, risk limits and
leverage. **API credentials come from environment variables only** and are never
written to a config file or committed. `.gitignore` excludes `.env` and the SQLite
database.

The testnet API key must be created with trade permission and **withdrawal disabled**.
Selecting the mainnet profile requires both an explicit `--profile mainnet` argument
and a confirmation environment variable, so no single mistake can route orders to
real money.

Deployment target is a systemd service with automatic restart; the reconciliation
step in §7.2 is what makes restarts safe.

---

## 10. Risks and open questions

- **No demonstrated edge.** The baseline strategy is unvalidated. Phase 1 proves the
  machinery, nothing more. Deciding whether any strategy is worth real capital is
  Phase 2's job, and the honest answer may be "none of these."
- **Testnet fidelity is poor.** Bybit testnet has thin liquidity and unrealistic
  fills. It validates correctness of the plumbing, not execution quality or slippage.
- **Funding costs are not modelled in Phase 1.** Multi-day perpetual holds pay
  funding; Phase 2's backtester must include it or results will be optimistic.
- **Dynamic universe complicates attribution.** A changing symbol set makes live
  results harder to compare against a fixed-universe backtest. Phase 4's drift
  monitoring needs to account for this.
- **Leverage above 3x remains the largest single risk factor**, mitigated but not
  eliminated by invariants 2 and 3. A gap through the stop — exchange outage,
  extreme news event — can still exceed the intended 1% loss.
