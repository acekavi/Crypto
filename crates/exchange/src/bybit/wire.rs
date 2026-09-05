use botcore::{Candle, Instrument, Symbol};
use rust_decimal::Decimal;
use serde::Deserialize;

use super::transport::ExchangeError;

/// Every Bybit V5 response shares this envelope. `time` is the server clock,
/// which feeds ClockOffset on every single call.
#[derive(Debug, Deserialize)]
pub struct Envelope<T> {
    #[serde(rename = "retCode")]
    pub ret_code: i32,
    #[serde(rename = "retMsg")]
    pub ret_msg: String,
    pub result: T,
    pub time: i64,
}

impl<T> Envelope<T> {
    pub fn into_result(self) -> Result<T, ExchangeError> {
        if self.ret_code == 0 {
            Ok(self.result)
        } else {
            Err(ExchangeError::Api {
                code: self.ret_code,
                msg: self.ret_msg,
            })
        }
    }
}

/// A kline as Bybit transmits it: a positional array of strings.
#[derive(Debug, Deserialize)]
pub struct KlineRow(
    pub String, // start time ms
    pub String, // open
    pub String, // high
    pub String, // low
    pub String, // close
    pub String, // volume
    pub String, // turnover
);

impl KlineRow {
    pub fn into_candle(self) -> Result<Candle, ExchangeError> {
        let parse = |s: &str, field: &str| -> Result<Decimal, ExchangeError> {
            s.parse::<Decimal>()
                .map_err(|e| ExchangeError::Decode(format!("kline {field}: {e}")))
        };
        Ok(Candle {
            open_time_ms: self
                .0
                .parse::<i64>()
                .map_err(|e| ExchangeError::Decode(format!("kline start: {e}")))?,
            open: parse(&self.1, "open")?,
            high: parse(&self.2, "high")?,
            low: parse(&self.3, "low")?,
            close: parse(&self.4, "close")?,
            volume: parse(&self.5, "volume")?,
            turnover: parse(&self.6, "turnover")?,
        })
    }
}

#[derive(Debug, Deserialize)]
pub struct KlineResult {
    // Bybit sends `"result": {}` on error responses (nonzero retCode), which
    // has no `list` key at all. Without `#[serde(default)]` that fails to
    // deserialize, and the whole `Envelope<T>` parse fails before the retCode
    // can be surfaced as `ExchangeError::Api` and before the clock offset is
    // observed from the response's `time` field.
    #[serde(default)]
    pub list: Vec<KlineRow>,
}

// `bound(deserialize = ...)` overrides serde's derive-macro bound inference,
// which conservatively adds `T: Default` for any generic field carrying
// `#[serde(default)]` even though `Vec<T>: Default` holds for every `T`
// unconditionally. Only the bound actually required — `T: Deserialize<'de>`,
// needed to deserialize the elements — is declared here.
#[derive(Debug, Deserialize)]
#[serde(bound(deserialize = "T: Deserialize<'de>"))]
pub struct ListResult<T> {
    // See the comment on `KlineResult::list`: Bybit's `result: {}` on error
    // responses must still deserialize so the envelope's retCode/time survive.
    #[serde(default)]
    pub list: Vec<T>,
    /// Opaque cursor for the next page, absent or empty on the last one.
    ///
    /// Endpoints that page (instruments-info caps at 500 rows) silently return
    /// a truncated list without it. `/v5/market/tickers` does not page and
    /// always omits it.
    #[serde(rename = "nextPageCursor", default)]
    pub next_page_cursor: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct InstrumentRow {
    pub symbol: String,
    pub status: String,
    #[serde(rename = "launchTime")]
    pub launch_time: String,
    #[serde(rename = "priceFilter")]
    pub price_filter: PriceFilter,
    #[serde(rename = "lotSizeFilter")]
    pub lot_size_filter: LotSizeFilter,
}

#[derive(Debug, Deserialize)]
pub struct PriceFilter {
    #[serde(rename = "tickSize")]
    pub tick_size: String,
}

#[derive(Debug, Deserialize)]
pub struct LotSizeFilter {
    #[serde(rename = "qtyStep")]
    pub qty_step: String,
    #[serde(rename = "minOrderQty")]
    pub min_order_qty: String,
    // Absent on some symbols, so it defaults rather than failing the decode:
    // a symbol with no minimum is a normal state, not a malformed response.
    #[serde(rename = "minNotionalValue", default)]
    pub min_notional_value: Option<String>,
}

impl InstrumentRow {
    /// Only instruments with `status == "Trading"` are convertible; anything
    /// else is filtered out rather than silently traded.
    pub fn into_instrument(self) -> Result<Option<Instrument>, ExchangeError> {
        if self.status != "Trading" {
            return Ok(None);
        }
        let parse = |s: &str, field: &str| -> Result<Decimal, ExchangeError> {
            s.parse::<Decimal>()
                .map_err(|e| ExchangeError::Decode(format!("instrument {field}: {e}")))
        };
        Ok(Some(Instrument {
            symbol: Symbol::new(self.symbol),
            tick_size: parse(&self.price_filter.tick_size, "tickSize")?,
            qty_step: parse(&self.lot_size_filter.qty_step, "qtyStep")?,
            min_order_qty: parse(&self.lot_size_filter.min_order_qty, "minOrderQty")?,
            min_notional: match self.lot_size_filter.min_notional_value.as_deref() {
                None | Some("") => Decimal::ZERO,
                Some(s) => parse(s, "minNotionalValue")?,
            },
            launch_time_ms: self
                .launch_time
                .parse::<i64>()
                .map_err(|e| ExchangeError::Decode(format!("instrument launchTime: {e}")))?,
        }))
    }
}

#[derive(Debug, Deserialize)]
pub struct TickerRow {
    pub symbol: String,
    #[serde(rename = "turnover24h")]
    pub turnover_24h: String,
    #[serde(rename = "lastPrice")]
    pub last_price: String,
}

/// 24h market statistics, used for universe ranking.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ticker {
    pub symbol: Symbol,
    pub turnover_24h: Decimal,
    pub last_price: Decimal,
}

impl TickerRow {
    pub fn into_ticker(self) -> Result<Ticker, ExchangeError> {
        let parse = |s: &str, field: &str| -> Result<Decimal, ExchangeError> {
            s.parse::<Decimal>()
                .map_err(|e| ExchangeError::Decode(format!("ticker {field}: {e}")))
        };
        Ok(Ticker {
            symbol: Symbol::new(self.symbol),
            turnover_24h: parse(&self.turnover_24h, "turnover24h")?,
            last_price: parse(&self.last_price, "lastPrice")?,
        })
    }
}

/// A funding-rate print as Bybit's `/v5/market/funding/history` returns it.
///
/// Confirmed live against `api-testnet.bybit.com` (2026-08-07, via curl):
/// field names are `fundingRate` and `fundingRateTimestamp`, both
/// string-encoded, matching the plan's expectation.
#[derive(Debug, Deserialize)]
pub struct FundingRateRow {
    pub symbol: String,
    #[serde(rename = "fundingRate")]
    pub funding_rate: String,
    #[serde(rename = "fundingRateTimestamp")]
    pub funding_rate_timestamp: String,
}

#[derive(Debug, Deserialize)]
pub struct FundingResult {
    // See the comment on `KlineResult::list`: an error response's
    // `result: {}` must still deserialize so retCode/time survive.
    #[serde(default)]
    pub list: Vec<FundingRateRow>,
}

/// One funding settlement for a symbol. `rate` carries its sign: negative
/// means shorts were paid that period, not longs — inverting or dropping it
/// silently inverts the cost of every short in a backtest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FundingRate {
    pub symbol: Symbol,
    pub funding_time_ms: i64,
    pub rate: Decimal,
}

impl FundingRateRow {
    pub fn into_funding_rate(self) -> Result<FundingRate, ExchangeError> {
        Ok(FundingRate {
            symbol: Symbol::new(self.symbol),
            funding_time_ms: self
                .funding_rate_timestamp
                .parse::<i64>()
                .map_err(|e| ExchangeError::Decode(format!("fundingRateTimestamp: {e}")))?,
            rate: self
                .funding_rate
                .parse::<Decimal>()
                .map_err(|e| ExchangeError::Decode(format!("fundingRate: {e}")))?,
        })
    }
}

use botcore::{Balance, OpenOrder, OrderState, OrderStatus, Position, Side};

#[derive(Debug, Deserialize)]
pub struct OrderCreateResult {
    #[serde(rename = "orderId")]
    pub order_id: String,
    #[serde(rename = "orderLinkId")]
    pub order_link_id: String,
}

#[derive(Debug, Deserialize)]
pub struct PositionRow {
    pub symbol: String,
    pub side: String,
    pub size: String,
    #[serde(rename = "avgPrice")]
    pub avg_price: String,
    #[serde(rename = "liqPrice")]
    pub liq_price: String,
    #[serde(rename = "unrealisedPnl")]
    pub unrealised_pnl: String,
}

/// Parse a Decimal field, treating an empty string as absent.
fn opt_decimal(s: &str) -> Result<Option<Decimal>, ExchangeError> {
    if s.trim().is_empty() {
        return Ok(None);
    }
    s.parse::<Decimal>()
        .map(Some)
        .map_err(|e| ExchangeError::Decode(format!("decimal field: {e}")))
}

fn req_decimal(s: &str, field: &str) -> Result<Decimal, ExchangeError> {
    s.parse::<Decimal>()
        .map_err(|e| ExchangeError::Decode(format!("{field}: {e}")))
}

fn parse_side(s: &str) -> Result<Side, ExchangeError> {
    match s {
        "Buy" => Ok(Side::Buy),
        "Sell" => Ok(Side::Sell),
        other => Err(ExchangeError::Decode(format!("unknown side {other}"))),
    }
}

impl PositionRow {
    /// Returns `None` for flat positions — Bybit reports closed positions with
    /// size 0, and treating those as open would create phantom state.
    pub fn into_position(self) -> Result<Option<Position>, ExchangeError> {
        let size = req_decimal(&self.size, "position size")?;
        if size.is_zero() {
            return Ok(None);
        }
        Ok(Some(Position {
            symbol: Symbol::new(self.symbol),
            side: parse_side(&self.side)?,
            size,
            entry_price: req_decimal(&self.avg_price, "avgPrice")?,
            liq_price: opt_decimal(&self.liq_price)?,
            unrealized_pnl: req_decimal(&self.unrealised_pnl, "unrealisedPnl")?,
        }))
    }
}

#[derive(Debug, Deserialize)]
pub struct OpenOrderRow {
    pub symbol: String,
    #[serde(rename = "orderId")]
    pub order_id: String,
    #[serde(rename = "orderLinkId")]
    pub order_link_id: String,
    pub side: String,
    pub price: String,
    pub qty: String,
    #[serde(rename = "cumExecQty")]
    pub cum_exec_qty: String,
    #[serde(rename = "orderStatus")]
    pub order_status: String,
    #[serde(rename = "createdTime")]
    pub created_time: String,
    // Optional because not every caller of this row type is guaranteed to
    // populate it (e.g. a hand-built fixture); `into_open_order` falls back
    // to `created_time` when it is missing or fails to parse, so decoding
    // never fails an order over this field.
    #[serde(rename = "updatedTime", default)]
    pub updated_time: Option<String>,
    // Empty string when nothing has filled, which is why this is optional and
    // decodes to zero rather than failing: an unfilled order is a normal state,
    // not a decode error.
    #[serde(rename = "avgPrice", default)]
    pub avg_price: Option<String>,
}

impl OpenOrderRow {
    pub fn into_open_order(self) -> Result<OpenOrder, ExchangeError> {
        let state = match self.order_status.as_str() {
            "New" | "Untriggered" => OrderState::New,
            "PartiallyFilled" => OrderState::PartiallyFilled,
            "Filled" => OrderState::Filled,
            "Cancelled" | "Deactivated" => OrderState::Cancelled,
            "Rejected" => OrderState::Rejected,
            other => {
                return Err(ExchangeError::Decode(format!(
                    "unknown orderStatus {other}"
                )));
            }
        };
        let created_time_ms = self
            .created_time
            .parse::<i64>()
            .map_err(|e| ExchangeError::Decode(format!("createdTime: {e}")))?;
        // updatedTime absent or unparseable falls back to createdTime rather
        // than failing the whole order: losing same-day fill precision is far
        // cheaper than refusing to track an order the exchange already
        // accepted.
        let updated_time_ms = self
            .updated_time
            .as_deref()
            .and_then(|s| s.parse::<i64>().ok())
            .unwrap_or(created_time_ms);
        Ok(OpenOrder {
            symbol: Symbol::new(self.symbol),
            order_id: self.order_id,
            order_link_id: self.order_link_id,
            side: parse_side(&self.side)?,
            price: req_decimal(&self.price, "order price")?,
            qty: req_decimal(&self.qty, "order qty")?,
            cum_exec_qty: req_decimal(&self.cum_exec_qty, "cumExecQty")?,
            state,
            created_time_ms,
            updated_time_ms,
        })
    }

    /// Decode into the richer status shape the pair executor polls on.
    ///
    /// Shares `OpenOrderRow` with `into_open_order` rather than introducing a
    /// second row type, so a Bybit field rename can only break one decoder.
    pub fn into_order_status(self) -> Result<OrderStatus, ExchangeError> {
        let symbol = Symbol::new(self.symbol.clone());
        let side = parse_side(&self.side)?;
        let avg_price = match self.avg_price.as_deref() {
            None | Some("") => Decimal::ZERO,
            Some(s) => req_decimal(s, "avgPrice")?,
        };
        let open = self.into_open_order()?;
        Ok(OrderStatus {
            symbol,
            order_id: open.order_id,
            order_link_id: open.order_link_id,
            side,
            state: open.state,
            qty: open.qty,
            cum_exec_qty: open.cum_exec_qty,
            avg_price,
            updated_time_ms: open.updated_time_ms,
        })
    }
}

#[derive(Debug, Deserialize)]
pub struct WalletRow {
    #[serde(rename = "totalEquity")]
    pub total_equity: String,
    #[serde(rename = "totalAvailableBalance")]
    pub total_available_balance: String,
}

impl WalletRow {
    pub fn into_balance(self) -> Result<Balance, ExchangeError> {
        Ok(Balance {
            equity: req_decimal(&self.total_equity, "totalEquity")?,
            available: req_decimal(&self.total_available_balance, "totalAvailableBalance")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn a_list_result_carries_bybits_page_cursor() {
        // instruments-info caps a page at 500 rows and signals more with this
        // cursor. Dropping it silently truncated the instrument list to the
        // alphabetically-first 500, which hid SOLUSDT and XRPUSDT from an
        // 788-instrument testnet.
        let r: ListResult<serde_json::Value> = serde_json::from_str(
            r#"{"list":[],"nextPageCursor":"first%3D0GUSDT%26last%3DPAXGPERP"}"#,
        )
        .expect("parses");
        assert_eq!(
            r.next_page_cursor.as_deref(),
            Some("first%3D0GUSDT%26last%3DPAXGPERP")
        );
    }

    #[test]
    fn a_list_result_without_a_cursor_is_the_last_page() {
        // /v5/market/tickers never pages and omits the field entirely.
        let r: ListResult<serde_json::Value> =
            serde_json::from_str(r#"{"list":[]}"#).expect("parses");
        assert_eq!(r.next_page_cursor, None);
    }

    #[test]
    fn kline_rows_parse_from_bybit_string_arrays() {
        // Bybit returns klines as arrays of strings:
        // [startTime, open, high, low, close, volume, turnover]
        let raw =
            r#"["1700000000000","42000.5","42500.0","41800.25","42100.75","123.45","5200000.5"]"#;
        let row: KlineRow = serde_json::from_str(raw).expect("row parses");
        let candle = row.into_candle().expect("row converts");

        assert_eq!(candle.open_time_ms, 1_700_000_000_000);
        assert_eq!(candle.open, dec!(42000.5));
        assert_eq!(candle.high, dec!(42500.0));
        assert_eq!(candle.low, dec!(41800.25));
        assert_eq!(candle.close, dec!(42100.75));
        assert_eq!(candle.volume, dec!(123.45));
        assert_eq!(candle.turnover, dec!(5200000.5));
    }

    #[test]
    fn envelope_surfaces_nonzero_ret_code_as_api_error() {
        let raw = r#"{"retCode":110007,"retMsg":"insufficient balance","result":{},"time":1700000000000}"#;
        let env: Envelope<serde_json::Value> = serde_json::from_str(raw).expect("envelope parses");
        let err = env
            .into_result()
            .expect_err("nonzero retCode must be an error");
        match err {
            ExchangeError::Api { code, ref msg } => {
                assert_eq!(code, 110007);
                assert_eq!(msg, "insufficient balance");
            }
            other => panic!("expected Api error, got {other:?}"),
        }
    }

    #[test]
    fn envelope_returns_result_on_success() {
        let raw = r#"{"retCode":0,"retMsg":"OK","result":{"value":7},"time":1700000000000}"#;
        let env: Envelope<serde_json::Value> = serde_json::from_str(raw).expect("envelope parses");
        assert_eq!(env.time, 1_700_000_000_000);
        let value = env.into_result().expect("retCode 0 is success");
        assert_eq!(value["value"], 7);
    }

    fn open_order_json(updated_time_field: &str) -> String {
        format!(
            r#"{{"symbol":"BTCUSDT","orderId":"oid-1","orderLinkId":"link-1","side":"Buy",
                "price":"42000","qty":"0.01","cumExecQty":"0",
                "orderStatus":"New","createdTime":"1700000000000"{updated_time_field}}}"#
        )
    }

    #[test]
    fn open_order_row_uses_updated_time_when_present() {
        let raw = open_order_json(r#","updatedTime":"1700000005000""#);
        let row: OpenOrderRow = serde_json::from_str(&raw).expect("row parses");
        let order = row.into_open_order().expect("row converts");

        assert_eq!(order.created_time_ms, 1_700_000_000_000);
        assert_eq!(order.updated_time_ms, 1_700_000_005_000);
    }

    #[test]
    fn open_order_row_falls_back_to_created_time_when_updated_time_is_absent() {
        // Falling back rather than failing the order matters: losing
        // same-day fill precision is far cheaper than refusing to track an
        // order the exchange already accepted.
        let raw = open_order_json("");
        let row: OpenOrderRow = serde_json::from_str(&raw).expect("row parses");
        let order = row.into_open_order().expect("row converts");

        assert_eq!(order.updated_time_ms, order.created_time_ms);
    }

    #[test]
    fn open_order_row_falls_back_to_created_time_when_updated_time_is_unparseable() {
        let raw = open_order_json(r#","updatedTime":"not-a-number""#);
        let row: OpenOrderRow = serde_json::from_str(&raw).expect("row parses");
        let order = row.into_open_order().expect("row converts");

        assert_eq!(order.updated_time_ms, order.created_time_ms);
    }
}
