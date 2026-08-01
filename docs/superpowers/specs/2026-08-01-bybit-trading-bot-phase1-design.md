# Bybit Trading Bot — Phase 1 Design

**Date:** 2026-08-01
**Status:** Approved (revised 2026-08-01: limit-only orders, Turso journal, daily trade cap, graphify knowledge base)
**Scope:** Phase 1 only — trading engine running unattended on Bybit testnet.

---

## 1. Goal and non-goals

Build a Rust trading engine that runs 24/7 against Bybit testnet, evaluates a
pluggable strategy on 1h/4h candle closes across a dynamically ranked universe of
USDT perpetuals, and executes trades — **using limit orders exclusively** — under
hard risk limits it cannot override.

Phase 1 succeeds when the bot survives a two-week unattended testnet soak without
losing track of a position, opening a duplicate order, placing a single market
order, or exceeding its daily trade cap — **not** when it is profitable.
Profitability is Phase 2's question.

**Non-goals for Phase 1:** backtester (Phase 2), web dashboard (Phase 3), mainnet
trading (Phase 4), spot, options, multi-account, machine learning.

### Project phases

| Phase | Delivers | Success condition |
|---|---|---|
| 1 | Engine + testnet live loop | Two-week unattended soak, no state divergence |
| 2 | Historical downloader, replay backtester, limit-fill model, fee/funding/slippage model, metrics | A strategy with a measured edge and calibrated risk numbers |
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
| Language / runtime | Rust, tokio | User requirement; low resource footprint, reliable long-running processes |
| Cadence | Decisions on 1h/4h candle close | Swing horizon; latency is not a competitive factor |
| Market | Bybit USDT perpetuals (`category: "linear"`), long and short | Shorts roughly double setup frequency |
| Leverage | Configurable, above 3x permitted, capped per-setup by the liquidation-buffer rule | User decision, made with liquidation risk stated |
| **Order types** | **Limit orders only — no market orders anywhere, including stops** | User rule. Guarantees maker fees and no entry slippage; residual gap risk documented in §11 |
| Position sizing | Risk-based: `(1% × equity) ÷ stop-distance` | Makes leverage a margin-efficiency setting rather than a risk multiplier |
| Risk : reward | 1 : 2 (target at 2R) | User rule; already the design's default |
| Risk envelope | 1% per trade, max 4 concurrent, halt at −5% daily or −15% total drawdown | Standard systematic swing defaults |
| **Daily trade cap** | **Max 5 filled entries per UTC day** | User rule ("3–5 per day"). See §5.1 on why the lower bound cannot be enforced |
| Universe | Daily re-rank of perps by 24h turnover, with floors | User choice; floors mitigate drift into illiquid pairs |
| Architecture | Event-driven tokio pipeline, Cargo workspace | Phase 2 backtester reuses the identical pipeline via trait swaps |
| Exchange client | Thin in-house Bybit V5 client | ~12 endpoints; needs exact control of idempotent retries, rate limiting, and `Decimal` handling. The official `bybit-rust-api` SDK has negligible adoption (~1 star), a poor basis for leveraged order placement |
| **Trade journal** | **Turso (`turso` crate, `sync` feature)** | User rule. Local-first: all reads/writes hit a local file, background `push()` to Turso cloud for durability and remote inspection |
| Baseline strategy | Trend-filtered pullback, limit entry at EMA20 | Unambiguous to code and cheap to falsify in Phase 2 |
| Rule discipline | Config loaded once at startup, hashed, recorded per trade; no runtime override path | User rule ("always stick to the rules") made auditable — see §5.2 |
| Knowledge base | graphify graph at `graphify-out/`, bootstrapped now, `--update` per chunk | User rule; dev-side navigation aid, not part of bot runtime |
| Observability | Structured logs + Turso journal in Phase 1; web dashboard in Phase 3 | Dashboard is meaningful scope, deferred so the core lands first |

---

## 3. Architecture

### 3.1 Workspace layout

```
crypto-bot/
├── Cargo.toml                 # workspace manifest
├── crates/
│   ├── botcore/               # domain types, no I/O, no dependencies on other crates
│   ├── exchange/              # ExchangeClient + MarketFeed traits; Bybit V5 implementation
│   ├── indicators/            # EMA, RSI, ATR — incremental, O(1) per update
│   ├── strategy/              # Strategy trait + baseline implementation
│   ├── risk/                  # position sizing, hard limits, kill switches
│   ├── engine/                # event loop, CandleStore, Executor, OrderTracker, Reconciler
│   └── persistence/           # Turso journal + sync task
└── bot/                       # binary: config, wiring, graceful shutdown
```

Dependency direction is strictly downward: `botcore` depends on nothing internal;
`indicators`, `exchange`, `persistence` depend only on `botcore`; `strategy` depends on
`botcore` + `indicators`; `risk` on `botcore`; `engine` on all of them; `bot` wires them.
No cycles.

### 3.2 Data flow

```
WS kline ──▶ CandleStore ──▶ StrategyEngine ──▶ Signal ──▶ RiskManager
                  ▲                                            │
             REST gap-fill                                OrderIntent
                                                               ▼
WS private ─▶ PositionTracker ◀── Reconciler          Executor ──▶ Bybit
                  │                    ▲                   │
                  │              OrderTracker ◀────────────┘
                  │              (expiry, partial fills,
                  │               stop escalation)
                  └────────────▶ Turso journal (local) ──▶ push() ──▶ Turso cloud
```

Each box is an independent tokio task. Market data fans out over
`tokio::sync::broadcast`; commands flow over bounded `tokio::sync::mpsc`. Channels
are bounded deliberately: if a consumer falls behind, the engine logs the lag and
blocks new entries rather than silently dropping events.

`OrderTracker` is new in this revision and exists solely because limit orders do not
fill immediately. It owns the lifecycle of every resting order: expiry, partial
fills, and stop-limit escalation.

### 3.3 Core traits

```rust
#[async_trait]
pub trait ExchangeClient: Send + Sync {
    async fn instruments(&self) -> Result<Vec<Instrument>>;
    async fn tickers(&self) -> Result<Vec<Ticker>>;
    async fn klines(&self, s: &Symbol, tf: Timeframe, n: u16) -> Result<Vec<Candle>>;
    async fn place_limit_entry(&self, req: LimitEntry) -> Result<OrderAck>;
    async fn amend_stop(&self, s: &Symbol, trigger: Decimal, limit: Decimal) -> Result<()>;
    async fn cancel_order(&self, s: &Symbol, link_id: &str) -> Result<()>;
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

There is deliberately **no `place_market_order` method on the trait**. The
market-order path does not exist in the type system, so it cannot be reached by
mistake — this is how the limit-only rule is enforced structurally rather than by
convention.

`Signal` carries direction, entry limit price, stop price and target price —
**never a quantity**. The strategy has no knowledge of account equity.
`RiskManager` alone converts `Signal → OrderIntent` by attaching size. This keeps
sizing bugs out of strategy code and lets Phase 2 replay any strategy against any
equity curve.

In Phase 2, `HistoricalFeed` and `SimulatedExchange` implement these same two
traits, so the backtester drives the identical pipeline. `SimulatedExchange` must
model limit fills honestly (§11).

---

## 4. Bybit V5 integration

### 4.1 Endpoints used

| Purpose | Endpoint |
|---|---|
| Instrument metadata (tick size, qty step, min notional) | `GET /v5/market/instruments-info` |
| Universe ranking (24h turnover) | `GET /v5/market/tickers` |
| Historical candles / gap fill | `GET /v5/market/kline` |
| Place limit entry with attached stop and target | `POST /v5/order/create` |
| Cancel resting order (expiry, escalation) | `POST /v5/order/cancel` |
| Amend resting order price (stop escalation) | `POST /v5/order/amend` |
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

### 4.3 Order policy — limit only

Every order the system places is a limit order. No code path constructs
`orderType: "Market"`, and the exchange client asserts this on every request before
signing.

**Entry.** `orderType: "Limit"`, `category: "linear"`, `positionIdx: 0` (one-way
mode), `timeInForce: "PostOnly"`, price = the EMA20 value at the signal candle's
close, rounded to `tickSize` away from the market (never into it). `stopLoss`,
`takeProfit`, `slOrderType: "Limit"`, `tpOrderType: "Limit"` and
`slTriggerBy: "MarkPrice"` are attached in the same request, so protection exists
from the instant the entry fills.

`slTriggerBy` uses mark price, not last price, because Bybit liquidates on mark
price; a last-price wick would otherwise trigger stops that liquidation logic
ignores.

`PostOnly` guarantees maker fees (~0.02% versus ~0.055% taker) and means the order
is rejected rather than filled if it would cross the book. Because a long entry sits
at EMA20 *below* market and a short entry at EMA20 *above* market, the order is
naturally maker; a `PostOnly` rejection means price has already run through the
entry level, in which case **the signal is skipped and logged**, never re-priced
into a taker fill.

**Entry expiry.** A resting entry is cancelled if unfilled after 3 closed 1h
candles. If it filled partially, the remainder is cancelled and the filled portion
is kept as a normal position — its stop and target are position-level, so they
already cover the actual filled size, and realized risk is below budget rather than
above it.

**Stop.** A stop-limit: trigger at the stop price (mark price), with `slLimitPrice`
set *beyond* the trigger by `stop_limit_offset` (default 0.3 × ATR(14)) so it fills
into the move rather than at its edge.

**Stop escalation ladder.** If the stop has triggered but remains unfilled after
`stop_fill_timeout` (default 30s), `OrderTracker` amends the resting limit to a
wider offset — 0.6 × ATR, then 1.2 × ATR — up to `max_stop_escalations` (default 3),
alerting on each step. Every rung is still a limit order. If the ladder is exhausted
with the position open, the bot alerts loudly, halts all new entries, and leaves a
resting limit at the widest offset. It will not place a market order to escape.
This is the accepted consequence of the limit-only rule; see §11.

**Take-profit.** Limit order at 2R (1:2 risk:reward), maker.

---

## 5. Safety invariants

Enforced in `risk` and `engine`. Strategy code cannot override any of them.

| # | Invariant | Failure it prevents |
|---|---|---|
| 1 | Every entry order carries an exchange-native stop and target in the same request | Crashed or disconnected bot leaving an unprotected leveraged position |
| 2 | No order may be constructed with `orderType: "Market"`; asserted at the client boundary | Silent violation of the limit-only rule |
| 3 | Reject any trade whose liquidation price is nearer than `liq_buffer_multiple` (default 3.0) × stop distance | High leverage plus a limit-only stop turning a gap into liquidation |
| 4 | `size = (risk_pct × equity) ÷ stop_distance`, rounded **down** to `qtyStep` | Rounding that increases risk; oversized positions |
| 5 | Reject if resulting notional < instrument minimum or > available margin | Silent exchange rejects mid-strategy |
| 6 | `orderLinkId` = deterministic hash of (symbol, signal candle timestamp, direction), ≤36 chars | A retry after a network timeout opening a second position |
| 7 | Max 5 filled entries per UTC day | Overtrading beyond the stated 3–5/day rule |
| 8 | Max 4 concurrent positions, max 1 position per symbol | Correlated over-exposure |
| 9 | Halt on −5% daily or −15% total drawdown; halt state persisted, cleared only by a human | A losing streak compounding; a restart un-halting itself |
| 10 | Block new entries when no candle has arrived for a subscribed symbol within 2× its timeframe | Trading on stale candles after a silent socket death |
| 11 | All monetary and quantity values are `rust_decimal::Decimal` | `f64` drift producing wrong sizes and rejected orders |
| 12 | Journal write failures never block or fail an order | A database outage stopping trading or losing a stop |
| 13 | Mainnet requires an explicit profile flag and confirmation env var | Accidentally trading real money |

**Halt definitions.** *Daily drawdown* is measured against account equity at the most
recent 00:00 UTC boundary. *Total drawdown* is measured against the all-time
high-water mark of equity, persisted across restarts. Reaching either threshold sets
the persisted halt flag: open positions keep their exchange-side stops and targets
and are left to resolve, but no new entries are placed until a human clears the flag.

**On leverage.** Invariants 3 and 4 together mean leverage changes only how much
margin a position locks, not how much is risked. Invariant 3 is what makes the
limit-only stop survivable: by requiring liquidation to sit at least 3× the stop
distance away, a gap through the stop has room to be caught by the escalation ladder
before the exchange force-closes the position. In practice this caps the effective
leverage any individual setup can use, and setups requiring more are rejected rather
than resized.

### 5.1 On the 3–5 trades per day rule

The **upper** bound is a hard invariant: at most 5 entries fill per UTC day, counted
at fill rather than at placement, so cancelled and expired limit orders do not
consume the budget. When the cap is reached, further signals are logged and skipped.

The **lower** bound cannot be enforced, and deliberately is not. Forcing a third
trade on a day that produced two valid setups would require relaxing the entry
criteria, which directly contradicts "always stick to the rules." If the bot
persistently produces fewer than three trades a day, that is a measurement to carry
into Phase 2 — widen the universe, revisit the filters with backtest evidence — not
a reason for the live engine to loosen its own rules.

Note the interaction with invariant 8: with at most 4 concurrent positions and swing
holds measured in hours to days, position turnover is what actually limits entry
frequency. If average hold time exceeds roughly a day, 3–5 entries daily is
arithmetically unreachable regardless of signal count. Phase 2 measures the real
hold-time distribution and determines whether the concurrent-position limit, not the
daily cap, is the binding constraint.

### 5.2 Rule discipline

"Always stick to the rules" is made structural rather than aspirational:

- Config is loaded once at startup and is immutable for the process lifetime.
- A SHA-256 hash of the effective config is written to every trade row, so each
  trade is attributable to an exact ruleset.
- There is no runtime parameter-mutation API, no manual trade-injection path, and no
  discretionary override in Phase 1.
- Changing any parameter requires editing config and restarting, which produces a
  new config hash and a visible discontinuity in the journal.

---

## 6. Baseline strategy

**Trend-filtered pullback with a limit entry at EMA20.** This is a starting point for
exercising the engine, not a validated edge. No claim is made about its
profitability; Phase 2 determines whether it or anything else reaches mainnet.

| Element | Rule |
|---|---|
| Bias filter (4h) | Long bias if EMA50 > EMA200; short bias if EMA50 < EMA200; otherwise no trade |
| Pullback (1h) | Within the last 5 closed candles, the low (long) or high (short) came within 0.5 × ATR(14) of EMA20 |
| Trigger (1h) | On the closing candle, RSI(14) crosses from below 40 to at or above 40 (long), or from above 60 to at or below 60 (short) |
| Volatility gate | ATR(14) ÷ close must fall in [0.3%, 5.0%] |
| Entry | PostOnly limit at the EMA20 value on the signal candle's close, in the 4h bias direction only; cancelled if unfilled after 3 closed candles |
| Stop | The further from the entry limit price of: the lowest low (long) or highest high (short) of the last 10 closed candles, or 1.5 × ATR(14) |
| Target | 2R (1:2 risk:reward), limit order; stop moved to breakeven once price reaches 1R |
| Exit | Exchange-side stop or target only — no discretionary exit logic in Phase 1 |

"R" is the distance between the **entry limit price** and the stop price, not the
signal candle's close — so sizing, stop and target are all computed from the price
the bot actually intends to pay, and are known before the order is placed.

All thresholds (EMA periods, RSI levels, ATR multiple and band, lookback lengths, R
multiple, offsets, expiry) are config values, not constants, so Phase 2 can sweep
them without code changes. The values above are the defaults, chosen as conventional
starting points — they are not tuned, and tuning them is Phase 2 work.

### Universe selection

Re-ranked daily at 00:00 UTC by 24h turnover from `GET /v5/market/tickers`, taking
the **top 20** symbols that pass all filters: 24h turnover ≥ 50,000,000 USDT, listed
at least 30 days, and `status == "Trading"`. Symbols with an open position or a
resting entry order are never dropped from the universe mid-trade regardless of
ranking changes. Universe size and filter thresholds are config values.

Phase 2 backtests pin a fixed universe snapshot instead of re-ranking, because
dynamic ranking over historical data embeds survivorship bias.

---

## 7. State, recovery and error handling

### 7.1 Persistence — Turso

The trade journal uses the `turso` crate with the `sync` feature:

```rust
let db = Builder::new_remote("data/bot.db")
    .with_remote_url(&env::var("TURSO_DATABASE_URL")?)
    .with_auth_token(&env::var("TURSO_AUTH_TOKEN")?)
    .build().await?;
```

All reads and writes hit the **local** file. A background task calls `push()` on an
interval (default 30s) and immediately after any terminal trade event (fill, stop,
target, cancel). Cloud sync provides durability and remote inspection, and will feed
the Phase 3 dashboard.

This local-first arrangement is what makes invariant 12 achievable: **Turso cloud is
never in the order path.** Sync failures are logged and retried with backoff, never
propagated to trading logic. If the cloud is unreachable indefinitely, the bot keeps
trading against the local file and syncs when connectivity returns.

Tables: `orders` (intent through terminal state, including resting/expired/partial),
`fills`, `positions`, `equity_snapshots`, `halt_state`, `daily_counters` (entries
filled per UTC day), and `candles` (rolling window for warmup after restart). Every
`orders` row carries the config hash from §5.2.

### 7.2 Startup reconciliation

The exchange is the source of truth in every disagreement. On every startup, before
any strategy evaluation:

1. Fetch live positions and open orders.
2. Diff against the journal.
3. Adopt positions the journal doesn't know about; mark journal positions the
   exchange doesn't have as closed, backfilling their outcome from execution history.
4. Adopt or cancel resting entry orders — anything past its expiry window is
   cancelled immediately.
5. Verify every open position has both a stop and a target attached. If either is
   missing, place it immediately.
6. Recompute drawdown state, today's filled-entry count, and re-apply any persisted
   halt.
7. Warm up indicators from stored plus refetched candles before accepting signals.

Step 5 differs from the pre-revision design: with limit-only stops there is no
market-order fallback, so a position discovered without a stop gets one placed
rather than being flattened.

### 7.3 Error classification

| Class | Examples | Handling |
|---|---|---|
| `Retryable` | Network error, 5xx, rate limit (`retCode` 10006/10018) | Exponential backoff with jitter, capped attempts; idempotent via `orderLinkId` |
| `Rejected` | Invalid params, insufficient margin, min-qty violation, `PostOnly` would-cross | Log with full context, skip the signal, continue |
| `Fatal` | Auth failure, invalid API key, account restricted | Halt trading, alert, require human intervention |

WebSocket disconnects trigger reconnect with backoff, re-subscription, and a REST
refetch of recent klines to fill any gap in the candle series. Rate limiting is
enforced client-side by a token-bucket limiter per endpoint group, sized below
Bybit's published limits, so retries never cascade into a ban.

---

## 8. Knowledge base

A graphify knowledge graph lives at `graphify-out/`, bootstrapped from this spec and
refreshed with `graphify . --update` after each implementation chunk. It indexes the
spec and, once they exist, the crates — giving later sessions a queryable map of the
codebase via `graphify query`, `graphify path` and `graphify explain`.

It is a development navigation aid. It is **not** part of the bot's runtime, is not
linked into the binary, and no trading decision consults it. `graphify-out/` is
git-ignored: it contains machine-specific absolute paths and is regenerable.

---

## 9. Testing strategy

| Layer | Approach |
|---|---|
| Indicators | Fixture tests against known-good EMA/RSI/ATR values; incremental output must equal batch-computed output |
| Sizing | `proptest` over random equity, price, stop distance, tick size and qty step, asserting realized risk never exceeds budget and quantities always satisfy exchange constraints |
| Order policy | A test asserting no code path can emit `orderType: "Market"`, including via reconciliation and escalation |
| Strategy | Golden-file tests: a fixed candle series produces an exact expected signal sequence |
| Limit lifecycle | `MockExchange` driving unfilled expiry, partial fill then expiry, `PostOnly` rejection, and the full stop-escalation ladder including exhaustion |
| Engine | `MockExchange` driving the pipeline through order rejects, WebSocket disconnects, duplicate acks, and restarts while a position is open |
| Reconciliation | Explicit tests per divergence case: orphan position, phantom position, missing stop, stale resting order, stale halt |
| Caps | Tests that the 6th entry of a UTC day is refused, and that cancelled/expired orders do not consume the budget |
| Persistence | Journal writes succeed with the Turso cloud unreachable; sync resumes and reconciles after reconnection |
| End-to-end | Testnet smoke run, then a two-week unattended soak with restart injection |

Tests assert observable behaviour — orders placed, state transitions, invariants
upheld — not internal call sequences.

---

## 10. Configuration and secrets

Configuration is TOML with per-profile files (`config/testnet.toml`,
`config/mainnet.toml`) covering symbol filters, strategy parameters, risk limits,
leverage, order offsets and expiry windows. **Credentials come from environment
variables only** — `BYBIT_API_KEY`, `BYBIT_API_SECRET`, `TURSO_DATABASE_URL`,
`TURSO_AUTH_TOKEN` — and are never written to a config file or committed.
`.gitignore` excludes `.env`, the local database and `graphify-out/`.

The testnet API key must be created with trade permission and **withdrawal disabled**.
Selecting the mainnet profile requires both an explicit `--profile mainnet` argument
and a confirmation environment variable, so no single mistake can route orders to
real money.

Deployment target is a systemd service with automatic restart; the reconciliation
step in §7.2 is what makes restarts safe.

---

## 11. Risks and open questions

- **Limit-only stops can leave a loss unbounded.** If price gaps through the stop
  level without trading there — exchange outage, extreme news, thin weekend book —
  the stop-limit does not fill and the position stays open. The escalation ladder
  (§4.3) and the 3× liquidation buffer (invariant 3) make this unlikely and
  survivable, but they cannot eliminate it. This is an accepted, deliberate
  consequence of the limit-only rule, chosen with the trade-off stated.
- **Missed fills change realized performance.** A limit entry at EMA20 will miss
  setups that run without retracing, and the ones it misses are not a random sample —
  strong moves are exactly the ones that don't come back. Phase 2's backtest must
  model fills pessimistically (require price to trade *through* the limit, not merely
  touch it) or results will be badly optimistic.
- **No demonstrated edge.** The baseline strategy is unvalidated. Phase 1 proves the
  machinery, nothing more. The honest answer from Phase 2 may be "no candidate
  strategy is worth real capital."
- **Testnet fidelity is poor**, and worse for limit orders than market ones: thin
  testnet books mean fill behaviour there says almost nothing about mainnet.
- **Funding costs are not modelled in Phase 1.** Multi-day perpetual holds pay
  funding; Phase 2's backtester must include it.
- **3–5 trades/day may be unreachable** under a 4-position concurrent cap if holds
  run long (§5.1). This is a measurement for Phase 2, not a Phase 1 blocker.
- **Dynamic universe complicates attribution.** A changing symbol set makes live
  results harder to compare against a fixed-universe backtest.
