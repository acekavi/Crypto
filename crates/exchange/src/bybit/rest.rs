use std::sync::Arc;

use async_trait::async_trait;
use botcore::{
    Balance, Candle, Instrument, LimitEntry, LimitLeg, OpenOrder, OrderAck, OrderStatus, Position,
    Symbol, Timeframe,
};
use rust_decimal::Decimal;
use serde::de::DeserializeOwned;
use serde_json::json;
use tracing::warn;

use super::rate_limit::RateLimiter;
use super::sign::{ClockOffset, Credentials, local_now_ms, sign_rest};
use super::transport::{ExchangeError, backoff_delay};
use super::wire::{
    Envelope, FundingRate, FundingRateRow, FundingResult, InstrumentRow, KlineResult, KlineRow,
    ListResult, OpenOrderRow, OrderCreateResult, PositionRow, Ticker, TickerRow, WalletRow,
};
use crate::traits::ExchangeClient;

// Bybit rejects a signed request whose timestamp falls outside this window
// (retCode 10002). Testnet round trips were measured at 4.5-10s, which leaves
// the clock-offset estimate uncertain by several seconds — well outside the
// 5s default. Bybit's own guidance for persistent 10002 is to widen it.
const RECV_WINDOW: u32 = 20_000;
const MAX_ATTEMPTS: u32 = 5;
const BACKOFF_BASE_MS: u64 = 200;
const BACKOFF_JITTER: f64 = 0.25;

// Confirmed live against api-testnet.bybit.com/v5/market/kline (2026-08-07):
// requesting limit=1001 or limit=2000 both still return exactly 1000 rows
// with retCode 0 — the server silently clamps rather than rejecting, so 1000
// is the real per-page ceiling.
const KLINE_PAGE_LIMIT: u16 = 1000;

// Confirmed live against api-testnet.bybit.com/v5/market/funding/history
// (2026-08-07): limit=201 and limit=250 both still return exactly 200 rows
// with retCode 0 — a lower silent clamp than kline's 1000, and confirmed
// rather than assumed per the plan's instruction not to guess.
const FUNDING_PAGE_LIMIT: u16 = 200;

/// Drop the candle that is still forming.
///
/// Bybit's kline REST returns the in-progress candle as its newest row and
/// gives no `confirm` flag to identify it — that exists only on the WebSocket
/// feed. Keeping it does two kinds of damage: a partial bar's OHLC is fed to
/// the strategy's indicators during warm-up, and its open time is recorded in
/// the `CandleStore`, so the same candle's real close arrives over the feed and
/// is rejected as a duplicate.
///
/// A candle is closed once its whole span is in the past, measured against the
/// clock-corrected server time rather than the local clock.
pub fn drop_unclosed(tf: Timeframe, now_ms: i64, mut candles: Vec<Candle>) -> Vec<Candle> {
    candles.retain(|c| c.open_time_ms + tf.duration_ms() <= now_ms);
    candles
}

/// Bybit V5 REST client.
///
/// Every response updates the clock offset, so signing stays valid even on a
/// machine whose local clock drifts.
pub struct BybitRest {
    base_url: String,
    creds: Credentials,
    http: reqwest::Client,
    clock: Arc<ClockOffset>,
    limiter: RateLimiter,
}

impl BybitRest {
    pub fn new(base_url: String, creds: Credentials) -> Self {
        BybitRest {
            base_url: base_url.trim_end_matches('/').to_string(),
            creds,
            http: reqwest::Client::builder()
                // `/v5/market/tickers` for all linear symbols was measured at
                // 4.5-10s on testnet, so a 10s ceiling turned a slow-but-fine
                // response into "error decoding response body" mid-body.
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("reqwest client builds with default TLS"),
            clock: Arc::new(ClockOffset::new()),
            limiter: RateLimiter::new(30, 10),
        }
    }

    pub fn clock(&self) -> &ClockOffset {
        &self.clock
    }

    /// A shared handle to the exact same clock offset every signed REST call
    /// uses. `ClockOffset` cannot be cloned — cloning would fork the offset,
    /// so a second holder would stop tracking corrections this client
    /// observes on every response. Returning the same `Arc` instead lets a
    /// caller (the private WebSocket feed, which also needs clock-corrected
    /// timestamps for its auth frames) share the one instance rather than
    /// starting from an uncorrected zero offset.
    pub fn clock_handle(&self) -> Arc<ClockOffset> {
        Arc::clone(&self.clock)
    }

    /// Signed GET with query parameters, retried on `Retryable` failures.
    pub(crate) async fn get<T: DeserializeOwned>(
        &self,
        path: &str,
        params: &[(&str, String)],
    ) -> Result<T, ExchangeError> {
        let query = params
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join("&");

        self.with_retry(|| async {
            self.limiter.acquire().await;
            let ts = self.clock.now_ms();
            let sign = sign_rest(
                &self.creds.api_secret,
                ts,
                &self.creds.api_key,
                RECV_WINDOW,
                &query,
            );
            let url = format!("{}{}?{}", self.base_url, path, query);

            let sent_at = local_now_ms();
            let resp = self
                .http
                .get(&url)
                .header("X-BAPI-API-KEY", &self.creds.api_key)
                .header("X-BAPI-TIMESTAMP", ts.to_string())
                .header("X-BAPI-RECV-WINDOW", RECV_WINDOW.to_string())
                .header("X-BAPI-SIGN", sign)
                .send()
                .await?;

            let text = resp.text().await?;
            let env: Envelope<T> = serde_json::from_str(&text)
                .map_err(|e| ExchangeError::Decode(format!("{e}: {text}")))?;
            self.clock
                .observe_round_trip(env.time, sent_at, local_now_ms());
            env.into_result()
        })
        .await
    }

    /// Signed POST. The body is serialised once and both signed and sent
    /// byte-identically — signing a different string than we transmit is the
    /// classic source of intermittent auth failures.
    pub(crate) async fn post<T: DeserializeOwned>(
        &self,
        path: &str,
        body: serde_json::Value,
    ) -> Result<T, ExchangeError> {
        let body_str = serde_json::to_string(&body)
            .map_err(|e| ExchangeError::Decode(format!("serialising request: {e}")))?;

        self.with_retry(|| {
            let body_str = body_str.clone();
            async move {
                self.limiter.acquire().await;
                let ts = self.clock.now_ms();
                let sign = sign_rest(
                    &self.creds.api_secret,
                    ts,
                    &self.creds.api_key,
                    RECV_WINDOW,
                    &body_str,
                );
                let url = format!("{}{}", self.base_url, path);

                let sent_at = local_now_ms();
                let resp = self
                    .http
                    .post(&url)
                    .header("X-BAPI-API-KEY", &self.creds.api_key)
                    .header("X-BAPI-TIMESTAMP", ts.to_string())
                    .header("X-BAPI-RECV-WINDOW", RECV_WINDOW.to_string())
                    .header("X-BAPI-SIGN", sign)
                    .header("Content-Type", "application/json")
                    .body(body_str)
                    .send()
                    .await?;

                let text = resp.text().await?;
                let env: Envelope<T> = serde_json::from_str(&text)
                    .map_err(|e| ExchangeError::Decode(format!("{e}: {text}")))?;
                self.clock
                    .observe_round_trip(env.time, sent_at, local_now_ms());
                env.into_result()
            }
        })
        .await
    }

    /// Retry loop honouring the error classification from Task 5.
    pub(crate) async fn with_retry<T, F, Fut>(&self, mut op: F) -> Result<T, ExchangeError>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<T, ExchangeError>>,
    {
        use botcore::ErrorClass;

        let mut last: Option<ExchangeError> = None;
        for attempt in 0..MAX_ATTEMPTS {
            match op().await {
                Ok(v) => return Ok(v),
                Err(e) => {
                    if e.class() != ErrorClass::Retryable {
                        return Err(e);
                    }
                    warn!(attempt, error = %e, "retryable exchange error");
                    last = Some(e);
                    tokio::time::sleep(backoff_delay(attempt, BACKOFF_BASE_MS, BACKOFF_JITTER))
                        .await;
                }
            }
        }
        Err(ExchangeError::RetriesExhausted {
            attempts: MAX_ATTEMPTS,
            last: Box::new(last.expect("loop ran at least once")),
        })
    }

    /// Fetches every candle in `[start_ms, end_ms]`, paging through Bybit's
    /// kline endpoint as needed.
    ///
    /// Deliberately **not** on `ExchangeClient` — see that trait's doc
    /// comment. A historical range download is an inherent capability of the
    /// real REST client, not something `SimulatedExchange` (Phase 2b) has any
    /// business implementing.
    ///
    /// Confirmed live against `api-testnet.bybit.com/v5/market/kline`
    /// (2026-08-07, via `curl`): the `list` comes back **newest first** — for
    /// `interval=60`, `open_time_ms` strictly decreases through the array —
    /// and `start`/`end` are inclusive-range query params in epoch
    /// milliseconds. This confirms what `klines`'s post-fetch ascending sort
    /// already implied. `limit` is silently clamped to 1000 (see
    /// `KLINE_PAGE_LIMIT`), never rejected.
    ///
    /// Reuses `self.get`, so every page goes through the same rate limiter
    /// and retry/backoff as every other endpoint — no second HTTP path.
    pub async fn klines_range(
        &self,
        symbol: &Symbol,
        tf: Timeframe,
        start_ms: i64,
        end_ms: i64,
    ) -> Result<Vec<Candle>, ExchangeError> {
        let symbol = symbol.clone();
        let interval = tf.as_bybit_interval().to_string();
        walk_kline_pages(start_ms, end_ms, |page_start, page_end| {
            let symbol = symbol.clone();
            let interval = interval.clone();
            async move {
                let res: KlineResult = self
                    .get(
                        "/v5/market/kline",
                        &[
                            ("category", "linear".into()),
                            ("symbol", symbol.as_str().to_string()),
                            ("interval", interval),
                            ("start", page_start.to_string()),
                            ("end", page_end.to_string()),
                            ("limit", KLINE_PAGE_LIMIT.to_string()),
                        ],
                    )
                    .await?;
                res.list.into_iter().map(KlineRow::into_candle).collect()
            }
        })
        .await
        .map(|c| drop_unclosed(tf, self.clock.now_ms(), c))
    }

    /// Fetches every funding-rate print in `[start_ms, end_ms]`, paging
    /// through Bybit's funding-history endpoint as needed.
    ///
    /// Confirmed live against `api-testnet.bybit.com/v5/market/funding/history`
    /// (2026-08-07, via `curl`): field names are `fundingRate` and
    /// `fundingRateTimestamp` (both strings), the `list` comes back **newest
    /// first** — same ordering as `/v5/market/kline` — and `limit` is
    /// silently clamped to 200 (see `FUNDING_PAGE_LIMIT`), never rejected.
    /// Unlike kline's `start`/`end`, this endpoint's range params are
    /// `startTime`/`endTime`; passing `start`/`end` here was verified to be
    /// silently ignored (identical output with and without them).
    ///
    /// Reuses `self.get` (rate limiter + retry/backoff) and the same
    /// non-advancing-cursor guard as `klines_range`, via the shared
    /// `walk_pages_newest_first` — no second hand-written pagination loop.
    pub async fn funding_history(
        &self,
        symbol: &Symbol,
        start_ms: i64,
        end_ms: i64,
    ) -> Result<Vec<FundingRate>, ExchangeError> {
        let symbol = symbol.clone();
        walk_pages_newest_first(
            start_ms,
            end_ms,
            |page_start, page_end| {
                let symbol = symbol.clone();
                async move {
                    let res: FundingResult = self
                        .get(
                            "/v5/market/funding/history",
                            &[
                                ("category", "linear".into()),
                                ("symbol", symbol.as_str().to_string()),
                                ("startTime", page_start.to_string()),
                                ("endTime", page_end.to_string()),
                                ("limit", FUNDING_PAGE_LIMIT.to_string()),
                            ],
                        )
                        .await?;
                    res.list
                        .into_iter()
                        .map(FundingRateRow::into_funding_rate)
                        .collect()
                }
            },
            |r: &FundingRate| r.funding_time_ms,
        )
        .await
    }
}

/// Pure page-walking logic behind both `klines_range` and `funding_history`,
/// independent of HTTP and of the item type, so it can be driven by a fake
/// `fetch` in tests without touching the network and carries exactly one
/// copy of the non-advancing-cursor guard.
///
/// `fetch(page_start, page_end)` must behave like Bybit: return items in
/// `[page_start, page_end]` **newest first**, keyed by `key`. The walk
/// starts with `page_end = end_ms` and, after each non-empty page, moves
/// `page_end` to just before the oldest item that page returned — i.e. it
/// pages backward through time, matching the API's own ordering.
///
/// Stops when:
/// - a page comes back empty,
/// - the oldest item received reaches `start_ms` (the range is covered), or
/// - **a page fails to move the cursor backward at all** — collected so far
///   is returned and a warning logged, rather than retrying the same window
///   forever against a rate-limited API.
///
/// Deduplicates by `key` (overlapping pages are normal), drops anything
/// outside `[start_ms, end_ms]`, and returns ascending.
pub async fn walk_pages_newest_first<T, F, Fut, K>(
    start_ms: i64,
    end_ms: i64,
    mut fetch: F,
    key: K,
) -> Result<Vec<T>, ExchangeError>
where
    F: FnMut(i64, i64) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<T>, ExchangeError>>,
    K: Fn(&T) -> i64,
{
    let mut collected: Vec<T> = Vec::new();
    let mut cursor_end = end_ms;

    loop {
        let page = fetch(start_ms, cursor_end).await?;
        if page.is_empty() {
            break;
        }

        let page_oldest = page
            .iter()
            .map(&key)
            .min()
            .expect("checked non-empty above");
        collected.extend(page);

        if page_oldest <= start_ms {
            break;
        }

        let next_cursor = page_oldest - 1;
        if next_cursor >= cursor_end {
            warn!(
                cursor_end,
                "page fetch did not advance the cursor; stopping instead of looping forever"
            );
            break;
        }
        cursor_end = next_cursor;
    }

    collected.retain(|item| {
        let k = key(item);
        k >= start_ms && k <= end_ms
    });
    collected.sort_by_key(|item| key(item));
    // `dedup_by_key`'s closure takes `&mut T` (an artifact of how `Vec`
    // dedup is implemented); `&mut T` coerces to `&T` at the call, so `key`
    // — written once against `&T` — still works unmodified here.
    collected.dedup_by_key(|item| key(item));
    Ok(collected)
}

/// `Candle`-specialised entry point kept for `klines_range` and its existing
/// pagination tests; the guard and looping logic itself lives once in
/// `walk_pages_newest_first`.
pub async fn walk_kline_pages<F, Fut>(
    start_ms: i64,
    end_ms: i64,
    fetch: F,
) -> Result<Vec<Candle>, ExchangeError>
where
    F: FnMut(i64, i64) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<Candle>, ExchangeError>>,
{
    walk_pages_newest_first(start_ms, end_ms, fetch, |c: &Candle| c.open_time_ms).await
}

// Endpoint methods live here and nowhere else — there are no inherent
// duplicates to keep in sync. `impl BybitRest` above holds only the
// constructor and the shared get/post/with_retry plumbing.
#[async_trait]
impl ExchangeClient for BybitRest {
    /// All linear perpetual instruments currently in `Trading` status.
    /// Every tradable linear instrument, following Bybit's page cursor.
    ///
    /// This endpoint caps a page at 500 rows and signals more with
    /// `nextPageCursor`. A single un-paged call silently returns the
    /// alphabetically-first 500 — which both hid symbols from the universe
    /// screen and made pinned symbols look unlisted.
    async fn instruments(&self) -> Result<Vec<Instrument>, ExchangeError> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut params = vec![("category", "linear".to_string()), ("limit", "1000".into())];
            if let Some(c) = &cursor {
                params.push(("cursor", c.clone()));
            }
            let res: ListResult<InstrumentRow> =
                self.get("/v5/market/instruments-info", &params).await?;
            let page_len = res.list.len();
            for row in res.list {
                if let Some(i) = row.into_instrument()? {
                    out.push(i);
                }
            }
            // An unchanged cursor would loop forever; an empty page cannot
            // carry a useful one. Same guard as `klines_range`.
            match res.next_page_cursor {
                Some(next)
                    if !next.is_empty() && page_len > 0 && Some(&next) != cursor.as_ref() =>
                {
                    cursor = Some(next);
                }
                _ => return Ok(out),
            }
        }
    }

    /// 24h statistics for every linear perpetual, used for universe ranking.
    async fn tickers(&self) -> Result<Vec<Ticker>, ExchangeError> {
        let res: ListResult<TickerRow> = self
            .get("/v5/market/tickers", &[("category", "linear".into())])
            .await?;
        res.list.into_iter().map(TickerRow::into_ticker).collect()
    }

    /// Recent klines, returned **oldest first** regardless of Bybit's ordering,
    /// because indicators must be fed chronologically.
    async fn klines(
        &self,
        symbol: &Symbol,
        tf: Timeframe,
        limit: u16,
    ) -> Result<Vec<Candle>, ExchangeError> {
        let res: KlineResult = self
            .get(
                "/v5/market/kline",
                &[
                    ("category", "linear".into()),
                    ("symbol", symbol.as_str().to_string()),
                    ("interval", tf.as_bybit_interval().to_string()),
                    ("limit", limit.to_string()),
                ],
            )
            .await?;

        let mut candles: Vec<Candle> = res
            .list
            .into_iter()
            .map(|r| r.into_candle())
            .collect::<Result<_, _>>()?;
        candles.sort_by_key(|c| c.open_time_ms);
        Ok(drop_unclosed(tf, self.clock.now_ms(), candles))
    }

    /// Place a PostOnly limit entry with stop and target attached.
    ///
    /// This is the only order-placing method in the workspace. `orderType` is
    /// hard-coded to `"Limit"` and no parameter can change it.
    async fn place_limit_entry(&self, req: LimitEntry) -> Result<OrderAck, ExchangeError> {
        let body = json!({
            "category": "linear",
            "symbol": req.symbol.as_str(),
            "side": req.side.as_bybit(),
            "orderType": "Limit",
            "timeInForce": "PostOnly",
            "positionIdx": 0,
            "qty": req.qty.normalize().to_string(),
            "price": req.price.normalize().to_string(),
            "orderLinkId": req.order_link_id,
            "stopLoss": req.stop_loss.normalize().to_string(),
            "slLimitPrice": req.stop_limit_price.normalize().to_string(),
            "slOrderType": "Limit",
            "slTriggerBy": "MarkPrice",
            "takeProfit": req.take_profit.normalize().to_string(),
            "tpOrderType": "Limit",
        });

        let res: OrderCreateResult = self.post("/v5/order/create", body).await?;
        Ok(OrderAck {
            order_id: res.order_id,
            order_link_id: res.order_link_id,
        })
    }

    async fn place_limit_leg(&self, req: LimitLeg) -> Result<OrderAck, ExchangeError> {
        let body = json!({
            "category": "linear",
            "symbol": req.symbol.as_str(),
            "side": req.side.as_bybit(),
            "orderType": "Limit",
            // GTC and not PostOnly: the price crosses the book deliberately.
            // GTC and not IOC: a partial fill that cancels its own remainder
            // would leave the pair carrying mismatched leg sizes with nothing
            // recording the intent.
            "timeInForce": "GTC",
            "positionIdx": 0,
            "qty": req.qty.normalize().to_string(),
            "price": req.price.normalize().to_string(),
            "orderLinkId": req.order_link_id,
            "reduceOnly": req.reduce_only,
        });
        let res: OrderCreateResult = self.post("/v5/order/create", body).await?;
        Ok(OrderAck {
            order_id: res.order_id,
            order_link_id: res.order_link_id,
        })
    }

    async fn order_by_link_id(
        &self,
        symbol: &Symbol,
        link_id: &str,
    ) -> Result<Option<OrderStatus>, ExchangeError> {
        for path in ["/v5/order/realtime", "/v5/order/history"] {
            let res: ListResult<OpenOrderRow> = self
                .get(
                    path,
                    &[
                        ("category", "linear".into()),
                        ("symbol", symbol.as_str().into()),
                        ("orderLinkId", link_id.into()),
                    ],
                )
                .await?;
            if let Some(row) = res.list.into_iter().next() {
                return Ok(Some(row.into_order_status()?));
            }
        }
        Ok(None)
    }

    async fn cancel_order(&self, symbol: &Symbol, link_id: &str) -> Result<(), ExchangeError> {
        let body = json!({
            "category": "linear",
            "symbol": symbol.as_str(),
            "orderLinkId": link_id,
        });
        let _: serde_json::Value = self.post("/v5/order/cancel", body).await?;
        Ok(())
    }

    /// Move a position's stop, keeping it a limit order.
    async fn amend_stop(
        &self,
        symbol: &Symbol,
        trigger: Decimal,
        limit_price: Decimal,
    ) -> Result<(), ExchangeError> {
        let body = json!({
            "category": "linear",
            "symbol": symbol.as_str(),
            "positionIdx": 0,
            "stopLoss": trigger.normalize().to_string(),
            "slLimitPrice": limit_price.normalize().to_string(),
            "slOrderType": "Limit",
            "slTriggerBy": "MarkPrice",
        });
        let _: serde_json::Value = self.post("/v5/position/trading-stop", body).await?;
        Ok(())
    }

    async fn set_leverage(&self, symbol: &Symbol, leverage: Decimal) -> Result<(), ExchangeError> {
        let lev = leverage.normalize().to_string();
        let body = json!({
            "category": "linear",
            "symbol": symbol.as_str(),
            "buyLeverage": lev,
            "sellLeverage": lev,
        });
        let _: serde_json::Value = self.post("/v5/position/set-leverage", body).await?;
        Ok(())
    }

    async fn positions(&self) -> Result<Vec<Position>, ExchangeError> {
        let res: ListResult<PositionRow> = self
            .get(
                "/v5/position/list",
                &[("category", "linear".into()), ("settleCoin", "USDT".into())],
            )
            .await?;
        let mut out = Vec::new();
        for row in res.list {
            if let Some(p) = row.into_position()? {
                out.push(p);
            }
        }
        Ok(out)
    }

    async fn open_orders(&self) -> Result<Vec<OpenOrder>, ExchangeError> {
        let res: ListResult<OpenOrderRow> = self
            .get(
                "/v5/order/realtime",
                &[("category", "linear".into()), ("settleCoin", "USDT".into())],
            )
            .await?;
        res.list
            .into_iter()
            .map(OpenOrderRow::into_open_order)
            .collect()
    }

    async fn balance(&self) -> Result<Balance, ExchangeError> {
        let res: ListResult<WalletRow> = self
            .get(
                "/v5/account/wallet-balance",
                &[("accountType", "UNIFIED".into())],
            )
            .await?;
        res.list
            .into_iter()
            .next()
            .ok_or_else(|| ExchangeError::Decode("wallet-balance returned no accounts".into()))?
            .into_balance()
    }
}
