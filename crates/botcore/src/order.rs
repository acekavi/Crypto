use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::symbol::Symbol;

/// Order direction. `Buy` opens a long, `Sell` opens a short.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Side {
    Buy,
    Sell,
}

impl Side {
    /// Bybit V5 wire representation.
    pub fn as_bybit(self) -> &'static str {
        match self {
            Side::Buy => "Buy",
            Side::Sell => "Sell",
        }
    }

    pub fn opposite(self) -> Self {
        match self {
            Side::Buy => Side::Sell,
            Side::Sell => Side::Buy,
        }
    }
}

/// A limit entry order with protection attached.
///
/// There is deliberately no market-order equivalent of this type anywhere in
/// the workspace: the limit-only rule is enforced by what can be constructed,
/// not by a runtime check that could be forgotten.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LimitEntry {
    pub symbol: Symbol,
    pub side: Side,
    pub qty: Decimal,
    pub price: Decimal,
    /// Deterministic idempotency key, max 36 characters.
    pub order_link_id: String,
    pub stop_loss: Decimal,
    pub stop_limit_price: Decimal,
    pub take_profit: Decimal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderAck {
    pub order_id: String,
    pub order_link_id: String,
}

/// Lifecycle state of a resting or completed order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum OrderState {
    New,
    PartiallyFilled,
    Filled,
    Cancelled,
    Rejected,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenOrder {
    pub symbol: Symbol,
    pub order_id: String,
    pub order_link_id: String,
    pub side: Side,
    pub price: Decimal,
    pub qty: Decimal,
    pub cum_exec_qty: Decimal,
    pub state: OrderState,
    pub created_time_ms: i64,
    /// When Bybit last updated this order — the moment a fill actually
    /// happened, as opposed to `created_time_ms` (when it was placed). The
    /// daily fill cap must key off this: an order placed at 23:50 UTC that
    /// fills at 00:05 belongs to the day it filled, not the day it was
    /// placed.
    pub updated_time_ms: i64,
}
