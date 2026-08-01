use async_trait::async_trait;
use botcore::{Balance, Candle, Instrument, LimitEntry, OpenOrder, OrderAck, Position, Symbol, Timeframe};
use rust_decimal::Decimal;
use serde::de::DeserializeOwned;
use serde_json::json;
use tracing::warn;

use super::rate_limit::RateLimiter;
use super::sign::{local_now_ms, sign_rest, ClockOffset, Credentials};
use super::transport::{backoff_delay, ExchangeError};
use super::wire::{
    Envelope, InstrumentRow, KlineResult, ListResult, OpenOrderRow, OrderCreateResult,
    PositionRow, Ticker, TickerRow, WalletRow,
};
use crate::traits::ExchangeClient;

const RECV_WINDOW: u32 = 5_000;
const MAX_ATTEMPTS: u32 = 5;
const BACKOFF_BASE_MS: u64 = 200;
const BACKOFF_JITTER: f64 = 0.25;

/// Bybit V5 REST client.
///
/// Every response updates the clock offset, so signing stays valid even on a
/// machine whose local clock drifts.
pub struct BybitRest {
    base_url: String,
    creds: Credentials,
    http: reqwest::Client,
    clock: ClockOffset,
    limiter: RateLimiter,
}

impl BybitRest {
    pub fn new(base_url: String, creds: Credentials) -> Self {
        BybitRest {
            base_url: base_url.trim_end_matches('/').to_string(),
            creds,
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("reqwest client builds with default TLS"),
            clock: ClockOffset::new(),
            limiter: RateLimiter::new(30, 10),
        }
    }

    pub fn clock(&self) -> &ClockOffset {
        &self.clock
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
            let sign = sign_rest(&self.creds.api_secret, ts, &self.creds.api_key, RECV_WINDOW, &query);
            let url = format!("{}{}?{}", self.base_url, path, query);

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
            self.clock.observe(env.time, local_now_ms());
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
                let sign =
                    sign_rest(&self.creds.api_secret, ts, &self.creds.api_key, RECV_WINDOW, &body_str);
                let url = format!("{}{}", self.base_url, path);

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
                self.clock.observe(env.time, local_now_ms());
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
                    tokio::time::sleep(backoff_delay(attempt, BACKOFF_BASE_MS, BACKOFF_JITTER)).await;
                }
            }
        }
        Err(ExchangeError::RetriesExhausted {
            attempts: MAX_ATTEMPTS,
            last: Box::new(last.expect("loop ran at least once")),
        })
    }
}

// Endpoint methods live here and nowhere else — there are no inherent
// duplicates to keep in sync. `impl BybitRest` above holds only the
// constructor and the shared get/post/with_retry plumbing.
#[async_trait]
impl ExchangeClient for BybitRest {
    /// All linear perpetual instruments currently in `Trading` status.
    async fn instruments(&self) -> Result<Vec<Instrument>, ExchangeError> {
        let res: ListResult<InstrumentRow> = self
            .get("/v5/market/instruments-info", &[("category", "linear".into())])
            .await?;
        let mut out = Vec::with_capacity(res.list.len());
        for row in res.list {
            if let Some(i) = row.into_instrument()? {
                out.push(i);
            }
        }
        Ok(out)
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

        let mut candles: Vec<Candle> =
            res.list.into_iter().map(|r| r.into_candle()).collect::<Result<_, _>>()?;
        candles.sort_by_key(|c| c.open_time_ms);
        Ok(candles)
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
        Ok(OrderAck { order_id: res.order_id, order_link_id: res.order_link_id })
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
            .get("/v5/position/list", &[("category", "linear".into()), ("settleCoin", "USDT".into())])
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
            .get("/v5/order/realtime", &[("category", "linear".into()), ("settleCoin", "USDT".into())])
            .await?;
        res.list.into_iter().map(OpenOrderRow::into_open_order).collect()
    }

    async fn balance(&self) -> Result<Balance, ExchangeError> {
        let res: ListResult<WalletRow> = self
            .get("/v5/account/wallet-balance", &[("accountType", "UNIFIED".into())])
            .await?;
        res.list
            .into_iter()
            .next()
            .ok_or_else(|| ExchangeError::Decode("wallet-balance returned no accounts".into()))?
            .into_balance()
    }
}
