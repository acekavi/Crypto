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
            Err(ExchangeError::Api { code: self.ret_code, msg: self.ret_msg })
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
    pub list: Vec<KlineRow>,
}

#[derive(Debug, Deserialize)]
pub struct ListResult<T> {
    pub list: Vec<T>,
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

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn kline_rows_parse_from_bybit_string_arrays() {
        // Bybit returns klines as arrays of strings:
        // [startTime, open, high, low, close, volume, turnover]
        let raw = r#"["1700000000000","42000.5","42500.0","41800.25","42100.75","123.45","5200000.5"]"#;
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
        let err = env.into_result().expect_err("nonzero retCode must be an error");
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
}
