//! A scriptable `ExchangeClient` for proving what the executor does when the
//! exchange misbehaves.
//!
//! Every interesting failure here is one that has actually cost someone money:
//! a leg that fills while its sibling is rejected, a placement that times out
//! after the exchange accepted it, an order that only partially fills. A mock
//! that always succeeds proves nothing about an executor whose entire job is
//! the failure path.

use std::collections::{HashMap, HashSet, VecDeque};
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

const MAX_LINK_ID_LEN: usize = 36;

#[allow(dead_code)]
#[derive(Debug, Clone)]
pub enum LegAction {
    Fills { price: Decimal },
    PartiallyFills { qty: Decimal, price: Decimal },
    Rejected,
    Rests,
    TimesOutButFills { price: Decimal },
}

#[derive(Default)]
pub struct FaultExchange {
    actions: Mutex<HashMap<String, VecDeque<LegAction>>>,
    orders: Mutex<HashMap<String, OrderStatus>>,
    pub placed: Mutex<Vec<LimitLeg>>,
    pub cancelled: Mutex<Vec<String>>,
    quotes: Mutex<HashMap<String, (Decimal, Decimal)>>,
    positions: Mutex<HashMap<String, Decimal>>,
    candles: Mutex<HashMap<String, Vec<Candle>>>,
}

#[allow(dead_code)]
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

    pub fn position(&self, symbol: &str, signed_size: Decimal) -> &Self {
        self.positions
            .lock()
            .unwrap()
            .insert(symbol.into(), signed_size);
        self
    }

    pub fn candles(
        &self,
        symbol: &str,
        last_open_ms: i64,
        n: usize,
        f: impl Fn(usize) -> Decimal,
    ) -> &Self {
        let step = Timeframe::H1.duration_ms();
        let first = last_open_ms - (n as i64 - 1) * step;
        let series = (0..n)
            .map(|i| {
                let close = f(i);
                Candle {
                    open_time_ms: first + i as i64 * step,
                    open: close,
                    high: close,
                    low: close,
                    close,
                    volume: Decimal::ZERO,
                    turnover: Decimal::ZERO,
                }
            })
            .collect();
        self.candles.lock().unwrap().insert(symbol.into(), series);
        self
    }

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

    fn instrument_for(symbol: &str) -> Instrument {
        Instrument {
            symbol: Symbol::new(symbol),
            tick_size: dec!(0.01),
            qty_step: dec!(0.01),
            min_order_qty: dec!(0.01),
            min_notional: dec!(5),
            launch_time_ms: 0,
        }
    }
}

#[async_trait]
impl ExchangeClient for FaultExchange {
    async fn place_limit_leg(&self, req: LimitLeg) -> Result<OrderAck, ExchangeError> {
        self.placed.lock().unwrap().push(req.clone());

        if req.order_link_id.len() > MAX_LINK_ID_LEN {
            return Err(ExchangeError::Api {
                code: 10001,
                msg: format!(
                    "orderLinkId is {} characters, over the {MAX_LINK_ID_LEN} limit: {}",
                    req.order_link_id.len(),
                    req.order_link_id
                ),
            });
        }
        let duplicate = self
            .orders
            .lock()
            .unwrap()
            .contains_key(&req.order_link_id);
        if duplicate {
            return Err(ExchangeError::Api {
                code: 110072,
                msg: format!("orderLinkId already exists: {}", req.order_link_id),
            });
        }

        let reducible = if req.reduce_only {
            let net = self.net_exposure(req.symbol.as_str());
            match req.side {
                Side::Buy if net < Decimal::ZERO => -net,
                Side::Sell if net > Decimal::ZERO => net,
                _ => Decimal::ZERO,
            }
        } else {
            req.qty
        };
        let clamp = |q: Decimal| if q < reducible { q } else { reducible };

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
            LegAction::Fills { price } => {
                let exec = clamp(req.qty);
                let state = if exec >= req.qty {
                    OrderState::Filled
                } else {
                    OrderState::Cancelled
                };
                (
                    Some(record(state, exec, price)),
                    Ok(OrderAck {
                        order_id: format!("oid-{}", req.order_link_id),
                        order_link_id: req.order_link_id.clone(),
                    }),
                )
            }
            LegAction::PartiallyFills { qty, price } => (
                Some(record(OrderState::Cancelled, clamp(qty), price)),
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
                Some(record(OrderState::Filled, clamp(req.qty), price)),
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
        let symbols: HashSet<String> = self
            .orders
            .lock()
            .unwrap()
            .values()
            .map(|o| o.symbol.as_str().to_string())
            .collect();
        let mut sizes: HashMap<String, Decimal> = symbols
            .into_iter()
            .map(|s| {
                let size = self.net_exposure(&s);
                (s, size)
            })
            .collect();
        for (symbol, size) in self.positions.lock().unwrap().iter() {
            sizes.insert(symbol.clone(), *size);
        }

        Ok(sizes
            .into_iter()
            .filter(|(_, size)| !size.is_zero())
            .map(|(symbol, size)| Position {
                symbol: Symbol::new(symbol),
                side: if size > Decimal::ZERO { Side::Buy } else { Side::Sell },
                size: size.abs(),
                entry_price: dec!(100),
                liq_price: None,
                unrealized_pnl: Decimal::ZERO,
            })
            .collect())
    }

    async fn instruments(&self) -> Result<Vec<Instrument>, ExchangeError> {
        let mut syms: HashSet<String> = self.candles.lock().unwrap().keys().cloned().collect();
        syms.extend(self.quotes.lock().unwrap().keys().cloned());
        syms.extend(self.positions.lock().unwrap().keys().cloned());
        if syms.is_empty() {
            syms.extend(["AAVEUSDT", "ETHUSDT", "ENAUSDT", "XRPUSDT", "BNBUSDT", "XAUTUSDT"].into_iter().map(str::to_string));
        }
        Ok(syms.into_iter().map(|s| Self::instrument_for(&s)).collect())
    }

    async fn tickers(&self) -> Result<Vec<Ticker>, ExchangeError> {
        let syms: Vec<String> = self.quotes.lock().unwrap().keys().cloned().collect();
        Ok(syms
            .into_iter()
            .map(|s| {
                let (bid, ask) = self.quotes.lock().unwrap().get(&s).copied().unwrap_or((dec!(99.9), dec!(100.1)));
                Ticker {
                    symbol: Symbol::new(s),
                    turnover_24h: Decimal::ZERO,
                    last_price: (bid + ask) / dec!(2),
                    bid1: bid,
                    ask1: ask,
                }
            })
            .collect())
    }

    async fn klines(
        &self,
        s: &Symbol,
        _tf: Timeframe,
        l: u16,
    ) -> Result<Vec<Candle>, ExchangeError> {
        let mut out = self
            .candles
            .lock()
            .unwrap()
            .get(s.as_str())
            .cloned()
            .unwrap_or_default();
        if out.len() > l as usize {
            out = out[out.len() - l as usize..].to_vec();
        }
        Ok(out)
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
