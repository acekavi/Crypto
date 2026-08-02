use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use botcore::{
    Balance, Candle, Instrument, LimitEntry, OpenOrder, OrderAck, Position, Symbol, Timeframe,
};
use exchange::ExchangeClient;
use exchange::bybit::transport::ExchangeError;
use exchange::bybit::wire::Ticker;
use rust_decimal::Decimal;

/// Build a simple candle for tests.
pub fn test_candle(open_time_ms: i64) -> Candle {
    Candle {
        open_time_ms,
        open: Decimal::from(100),
        high: Decimal::from(101),
        low: Decimal::from(99),
        close: Decimal::from(100),
        volume: Decimal::ZERO,
        turnover: Decimal::ZERO,
    }
}

#[derive(Debug, Default)]
struct Recorded {
    placed: Vec<LimitEntry>,
    cancelled: Vec<String>,
    amended: Vec<(Symbol, Decimal, Decimal)>,
    place_entry_calls: usize,
}

/// Programmable failure for `place_limit_entry`.
enum PlaceFailure {
    None,
    Once(ExchangeError),
    Always(ExchangeError),
}

/// An in-memory `ExchangeClient` for driving the execution layer through
/// failure modes a real exchange only produces by accident.
///
/// Uses interior mutability so it can sit behind an `Arc` and still record
/// calls through a shared reference — which is how the executor will hold it.
pub struct MockExchange {
    balance: Balance,
    positions: Vec<Position>,
    instruments: Vec<Instrument>,
    tickers: Vec<Ticker>,
    klines: HashMap<(String, Timeframe), Vec<Candle>>,
    open_orders: Vec<OpenOrder>,
    place_failure: Mutex<PlaceFailure>,
    recorded: Mutex<Recorded>,
}

impl Default for MockExchange {
    fn default() -> Self {
        Self::new()
    }
}

impl MockExchange {
    pub fn new() -> Self {
        MockExchange {
            balance: Balance {
                equity: Decimal::from(10_000),
                available: Decimal::from(9_000),
            },
            positions: Vec::new(),
            instruments: Vec::new(),
            tickers: Vec::new(),
            klines: HashMap::new(),
            open_orders: Vec::new(),
            place_failure: Mutex::new(PlaceFailure::None),
            recorded: Mutex::new(Recorded::default()),
        }
    }

    pub fn with_balance(mut self, balance: Balance) -> Self {
        self.balance = balance;
        self
    }

    pub fn with_positions(mut self, positions: Vec<Position>) -> Self {
        self.positions = positions;
        self
    }

    pub fn with_instruments(mut self, instruments: Vec<Instrument>) -> Self {
        self.instruments = instruments;
        self
    }

    pub fn with_tickers(mut self, tickers: Vec<Ticker>) -> Self {
        self.tickers = tickers;
        self
    }

    pub fn with_klines(mut self, symbol: &Symbol, tf: Timeframe, candles: Vec<Candle>) -> Self {
        self.klines
            .insert((symbol.as_str().to_string(), tf), candles);
        self
    }

    pub fn with_open_orders(mut self, orders: Vec<OpenOrder>) -> Self {
        self.open_orders = orders;
        self
    }

    /// Fail the next `place_limit_entry` only. Models a request that timed out
    /// and will be retried.
    pub fn fail_place_entry_once(self, err: ExchangeError) -> Self {
        *self.place_failure.lock().expect("mock lock") = PlaceFailure::Once(err);
        self
    }

    pub fn fail_place_entry_always(self, err: ExchangeError) -> Self {
        *self.place_failure.lock().expect("mock lock") = PlaceFailure::Always(err);
        self
    }

    pub fn placed_orders(&self) -> Vec<LimitEntry> {
        self.recorded.lock().expect("mock lock").placed.clone()
    }

    pub fn cancelled(&self) -> Vec<String> {
        self.recorded.lock().expect("mock lock").cancelled.clone()
    }

    pub fn amended_stops(&self) -> Vec<(Symbol, Decimal, Decimal)> {
        self.recorded.lock().expect("mock lock").amended.clone()
    }

    pub fn place_entry_call_count(&self) -> usize {
        self.recorded.lock().expect("mock lock").place_entry_calls
    }
}

#[async_trait]
impl ExchangeClient for MockExchange {
    async fn instruments(&self) -> Result<Vec<Instrument>, ExchangeError> {
        Ok(self.instruments.clone())
    }

    async fn tickers(&self) -> Result<Vec<Ticker>, ExchangeError> {
        Ok(self.tickers.clone())
    }

    async fn klines(
        &self,
        symbol: &Symbol,
        tf: Timeframe,
        _limit: u16,
    ) -> Result<Vec<Candle>, ExchangeError> {
        Ok(self
            .klines
            .get(&(symbol.as_str().to_string(), tf))
            .cloned()
            .unwrap_or_default())
    }

    async fn place_limit_entry(&self, req: LimitEntry) -> Result<OrderAck, ExchangeError> {
        {
            let mut rec = self.recorded.lock().expect("mock lock");
            rec.place_entry_calls += 1;
        }

        // A failed placement is deliberately NOT recorded as placed: the
        // execution layer's retry tests depend on distinguishing "the exchange
        // accepted this" from "we asked".
        let mut failure = self.place_failure.lock().expect("mock lock");
        match &*failure {
            PlaceFailure::Always(e) => return Err(clone_error(e)),
            PlaceFailure::Once(e) => {
                let err = clone_error(e);
                *failure = PlaceFailure::None;
                return Err(err);
            }
            PlaceFailure::None => {}
        }
        drop(failure);

        let ack = OrderAck {
            order_id: format!("mock-{}", req.order_link_id),
            order_link_id: req.order_link_id.clone(),
        };
        self.recorded.lock().expect("mock lock").placed.push(req);
        Ok(ack)
    }

    async fn amend_stop(
        &self,
        symbol: &Symbol,
        trigger: Decimal,
        limit_price: Decimal,
    ) -> Result<(), ExchangeError> {
        self.recorded.lock().expect("mock lock").amended.push((
            symbol.clone(),
            trigger,
            limit_price,
        ));
        Ok(())
    }

    async fn cancel_order(&self, _symbol: &Symbol, link_id: &str) -> Result<(), ExchangeError> {
        self.recorded
            .lock()
            .expect("mock lock")
            .cancelled
            .push(link_id.to_string());
        Ok(())
    }

    async fn positions(&self) -> Result<Vec<Position>, ExchangeError> {
        Ok(self.positions.clone())
    }

    async fn open_orders(&self) -> Result<Vec<OpenOrder>, ExchangeError> {
        Ok(self.open_orders.clone())
    }

    async fn set_leverage(
        &self,
        _symbol: &Symbol,
        _leverage: Decimal,
    ) -> Result<(), ExchangeError> {
        Ok(())
    }

    async fn balance(&self) -> Result<Balance, ExchangeError> {
        Ok(self.balance.clone())
    }
}

/// `ExchangeError` is not `Clone` (its `Http` variant wraps a `reqwest::Error`),
/// so injected failures are reproduced by variant rather than cloned.
fn clone_error(e: &ExchangeError) -> ExchangeError {
    match e {
        ExchangeError::Api { code, msg } => ExchangeError::Api {
            code: *code,
            msg: msg.clone(),
        },
        ExchangeError::Decode(m) => ExchangeError::Decode(m.clone()),
        ExchangeError::WebSocket(m) => ExchangeError::WebSocket(m.clone()),
        other => ExchangeError::Decode(format!("injected: {other}")),
    }
}
