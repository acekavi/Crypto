use async_trait::async_trait;
use botcore::{Balance, Candle, Instrument, LimitEntry, OpenOrder, OrderAck, Position, Symbol, Timeframe};
use rust_decimal::Decimal;
use tokio::sync::broadcast;

use crate::bybit::transport::ExchangeError;
use crate::bybit::wire::Ticker;

/// Everything the engine may ask of an exchange.
///
/// There is deliberately no `place_market_order`. Phase 2's SimulatedExchange
/// implements this same trait, which is what lets the backtester drive the
/// identical pipeline as live trading.
///
/// These are the *only* definitions of these operations — `BybitRest` has no
/// inherent duplicates of them. Callers import the trait.
#[async_trait]
pub trait ExchangeClient: Send + Sync {
    async fn instruments(&self) -> Result<Vec<Instrument>, ExchangeError>;
    async fn tickers(&self) -> Result<Vec<Ticker>, ExchangeError>;
    async fn klines(
        &self,
        symbol: &Symbol,
        tf: Timeframe,
        limit: u16,
    ) -> Result<Vec<Candle>, ExchangeError>;
    async fn place_limit_entry(&self, req: LimitEntry) -> Result<OrderAck, ExchangeError>;
    async fn amend_stop(
        &self,
        symbol: &Symbol,
        trigger: Decimal,
        limit_price: Decimal,
    ) -> Result<(), ExchangeError>;
    async fn cancel_order(&self, symbol: &Symbol, link_id: &str) -> Result<(), ExchangeError>;
    async fn positions(&self) -> Result<Vec<Position>, ExchangeError>;
    async fn open_orders(&self) -> Result<Vec<OpenOrder>, ExchangeError>;
    async fn set_leverage(&self, symbol: &Symbol, leverage: Decimal) -> Result<(), ExchangeError>;
    async fn balance(&self) -> Result<Balance, ExchangeError>;
}

/// A market data event delivered by a feed.
#[derive(Debug, Clone)]
pub enum MarketEvent {
    /// A candle that has closed and will not change again.
    CandleClosed { symbol: Symbol, tf: Timeframe, candle: Candle },
    /// The feed reconnected and refilled a gap; indicators should be rewarmed.
    GapFilled { symbol: Symbol, tf: Timeframe, candles: Vec<Candle> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subscription {
    pub symbol: Symbol,
    pub timeframe: Timeframe,
}

#[async_trait]
pub trait MarketFeed: Send + Sync {
    async fn subscribe(
        &self,
        subs: &[Subscription],
    ) -> Result<broadcast::Receiver<MarketEvent>, ExchangeError>;
}
