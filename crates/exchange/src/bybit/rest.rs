use botcore::{Candle, Instrument, Symbol, Timeframe};
use serde::de::DeserializeOwned;
use tracing::warn;

use super::rate_limit::RateLimiter;
use super::sign::{local_now_ms, sign_rest, ClockOffset, Credentials};
use super::transport::{backoff_delay, ExchangeError};
use super::wire::{
    Envelope, InstrumentRow, KlineResult, ListResult, Ticker, TickerRow,
};

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

    /// All linear perpetual instruments currently in `Trading` status.
    pub async fn instruments(&self) -> Result<Vec<Instrument>, ExchangeError> {
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
    pub async fn tickers(&self) -> Result<Vec<Ticker>, ExchangeError> {
        let res: ListResult<TickerRow> = self
            .get("/v5/market/tickers", &[("category", "linear".into())])
            .await?;
        res.list.into_iter().map(TickerRow::into_ticker).collect()
    }

    /// Recent klines, returned **oldest first** regardless of Bybit's ordering,
    /// because indicators must be fed chronologically.
    pub async fn klines(
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
}
