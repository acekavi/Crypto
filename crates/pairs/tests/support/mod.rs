//! A scriptable `ExchangeClient` for proving what the executor does when the
//! exchange misbehaves.
//!
//! Every interesting failure here is one that has actually cost someone money:
//! a leg that fills while its sibling is rejected, a placement that times out
//! after the exchange accepted it, an order that only partially fills. A mock
//! that always succeeds proves nothing about an executor whose entire job is
//! the failure path.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use async_trait::async_trait;
use botcore::{
    Balance, Candle, Instrument, LimitEntry, LimitLeg, OpenOrder, OrderAck, OrderState,
    OrderStatus, Position, Side, Symbol, Timeframe,
};
use exchange::ExchangeClient;
use exchange::bybit::transport::ExchangeError;
use exchange::bybit::wire::Ticker;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// What the exchange does with the next order placed on a given symbol.
#[derive(Debug, Clone)]
pub enum LegAction {
    /// Accepted and fully filled at `price`.
    Fills { price: Decimal },
    /// Accepted, then fills only `qty` before being cancelled.
    PartiallyFills { qty: Decimal, price: Decimal },
    /// Refused outright. `place` returns `Err`, and no order exists.
    Rejected,
    /// Accepted and left resting. `place` succeeds and the order never fills.
    Rests,
    /// `place` returns a transport error **but the exchange accepted and
    /// filled it anyway**. The executor must discover this by querying, not
    /// assume the leg is absent.
    TimesOutButFills { price: Decimal },
}

#[derive(Default)]
pub struct FaultExchange {
    /// Per-symbol script, consumed one action per placement, so the unwind
    /// ladder's successive attempts can behave differently.
    actions: Mutex<HashMap<String, VecDeque<LegAction>>>,
    orders: Mutex<HashMap<String, OrderStatus>>,
    pub placed: Mutex<Vec<LimitLeg>>,
    pub cancelled: Mutex<Vec<String>>,
    quotes: Mutex<HashMap<String, (Decimal, Decimal)>>,
}

impl FaultExchange {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn script(&self, symbol: &str, actions: Vec<LegAction>) -> &Self {
        self.actions
            .lock()
            .unwrap()
            .insert(symbol.into(), actions.into());
        self
    }

    pub fn quote(&self, symbol: &str, bid: Decimal, ask: Decimal) -> &Self {
        self.quotes.lock().unwrap().insert(symbol.into(), (bid, ask));
        self
    }

    /// Total executed quantity the account is left holding on `symbol`, signed
    /// by direction. The assertion every failure-path test ends with.
    pub fn net_exposure(&self, symbol: &str) -> Decimal {
        self.orders
            .lock()
            .unwrap()
            .values()
            .filter(|o| o.symbol.as_str() == symbol)
            .map(|o| match o.side {
                Side::Buy => o.cum_exec_qty,
                Side::Sell => -o.cum_exec_qty,
            })
            .sum()
    }

    fn next_action(&self, symbol: &str) -> LegAction {
        self.actions
            .lock()
            .unwrap()
            .get_mut(symbol)
            .and_then(|q| q.pop_front())
            .unwrap_or(LegAction::Fills { price: dec!(100) })
    }
}

#[async_trait]
impl ExchangeClient for FaultExchange {
    async fn place_limit_leg(&self, req: LimitLeg) -> Result<OrderAck, ExchangeError> {
        self.placed.lock().unwrap().push(req.clone());
        let action = self.next_action(req.symbol.as_str());

        let record = |state, exec: Decimal, price: Decimal| OrderStatus {
            symbol: req.symbol.clone(),
            order_id: format!("oid-{}", req.order_link_id),
            order_link_id: req.order_link_id.clone(),
            side: req.side,
            state,
            qty: req.qty,
            cum_exec_qty: exec,
            avg_price: price,
            updated_time_ms: 1_700_000_000_000,
        };

        let (status, result) = match action {
            LegAction::Fills { price } => (
                Some(record(OrderState::Filled, req.qty, price)),
                Ok(OrderAck {
                    order_id: format!("oid-{}", req.order_link_id),
                    order_link_id: req.order_link_id.clone(),
                }),
            ),
            LegAction::PartiallyFills { qty, price } => (
                Some(record(OrderState::Cancelled, qty, price)),
                Ok(OrderAck {
                    order_id: format!("oid-{}", req.order_link_id),
                    order_link_id: req.order_link_id.clone(),
                }),
            ),
            LegAction::Rests => (
                Some(record(OrderState::New, Decimal::ZERO, Decimal::ZERO)),
                Ok(OrderAck {
                    order_id: format!("oid-{}", req.order_link_id),
                    order_link_id: req.order_link_id.clone(),
                }),
            ),
            LegAction::Rejected => (
                None,
                Err(ExchangeError::Api {
                    code: 110007,
                    msg: "insufficient balance".into(),
                }),
            ),
            LegAction::TimesOutButFills { price } => (
                Some(record(OrderState::Filled, req.qty, price)),
                Err(ExchangeError::Decode("simulated transport timeout".into())),
            ),
        };
        if let Some(s) = status {
            self.orders.lock().unwrap().insert(req.order_link_id, s);
        }
        result
    }

    async fn order_by_link_id(
        &self,
        _symbol: &Symbol,
        link_id: &str,
    ) -> Result<Option<OrderStatus>, ExchangeError> {
        Ok(self.orders.lock().unwrap().get(link_id).cloned())
    }

    async fn cancel_order(&self, _symbol: &Symbol, link_id: &str) -> Result<(), ExchangeError> {
        self.cancelled.lock().unwrap().push(link_id.into());
        if let Some(o) = self.orders.lock().unwrap().get_mut(link_id)
            && o.state == OrderState::New
        {
            o.state = OrderState::Cancelled;
        }
        Ok(())
    }

    async fn ticker(&self, symbol: &Symbol) -> Result<Ticker, ExchangeError> {
        let (bid, ask) = self
            .quotes
            .lock()
            .unwrap()
            .get(symbol.as_str())
            .copied()
            .unwrap_or((dec!(99.9), dec!(100.1)));
        Ok(Ticker {
            symbol: symbol.clone(),
            turnover_24h: dec!(0),
            last_price: (bid + ask) / dec!(2),
            bid1: bid,
            ask1: ask,
        })
    }

    async fn positions(&self) -> Result<Vec<Position>, ExchangeError> {
        Ok(Vec::new())
    }

    // Not modelled. The executor never calls these, and a panicking stub is
    // better than a plausible-looking lie that hides a new dependency.
    async fn instruments(&self) -> Result<Vec<Instrument>, ExchangeError> {
        unimplemented!("FaultExchange does not model instruments")
    }
    async fn tickers(&self) -> Result<Vec<Ticker>, ExchangeError> {
        unimplemented!("FaultExchange does not model the all-symbols ticker")
    }
    async fn klines(
        &self,
        _s: &Symbol,
        _tf: Timeframe,
        _l: u16,
    ) -> Result<Vec<Candle>, ExchangeError> {
        unimplemented!("FaultExchange does not model klines")
    }
    async fn place_limit_entry(&self, _r: LimitEntry) -> Result<OrderAck, ExchangeError> {
        unimplemented!("pairs never places a directional LimitEntry")
    }
    async fn amend_stop(
        &self,
        _s: &Symbol,
        _t: Decimal,
        _l: Decimal,
    ) -> Result<(), ExchangeError> {
        unimplemented!("a pair's stop is a z-score, not an exchange stop")
    }
    async fn open_orders(&self) -> Result<Vec<OpenOrder>, ExchangeError> {
        Ok(Vec::new())
    }
    async fn set_leverage(&self, _s: &Symbol, _l: Decimal) -> Result<(), ExchangeError> {
        Ok(())
    }
    async fn balance(&self) -> Result<Balance, ExchangeError> {
        Ok(Balance {
            equity: dec!(10000),
            available: dec!(10000),
        })
    }
}
