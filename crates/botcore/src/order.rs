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
    /// Multiple of initial risk at which the stop moves to entry, carried from
    /// the `Signal` that produced this order. `None` leaves the stop fixed for
    /// the life of the trade.
    ///
    /// Not sent to the exchange — Bybit has no such order type, so the rule is
    /// driven by amending the stop. It rides here because this is the only
    /// record that survives from the signal to the moment the entry fills, and
    /// whoever manages the position afterwards (the live engine, or the
    /// simulator) must read the same number the strategy chose rather than a
    /// second copy of it kept somewhere else.
    pub breakeven_at_r: Option<Decimal>,
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

/// One leg of a pair trade: a plain limit order with no protection attached.
///
/// Separate from [`LimitEntry`] because the two are genuinely different orders.
/// A `LimitEntry` is PostOnly and carries a stop and target, which is right for
/// a directional setup. A pair leg is priced *through* the book so it fills now
/// — PostOnly would be rejected — and has no per-leg stop, because the pair's
/// stop is a spread z-score that neither leg's price can express.
///
/// There is still no market-order equivalent anywhere in the workspace. The
/// limit-only rule stays enforced by what can be constructed, including on the
/// unwind path where a market order would be most tempting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LimitLeg {
    pub symbol: Symbol,
    pub side: Side,
    pub qty: Decimal,
    pub price: Decimal,
    /// Deterministic idempotency key, max 36 characters.
    pub order_link_id: String,
    /// `true` for a leg that closes an existing position. The exchange then
    /// refuses to let it open one in the opposite direction, which is what
    /// makes a duplicate close attempt harmless rather than a new position.
    pub reduce_only: bool,
}

/// The exchange's view of one order, resting or terminal.
///
/// Distinct from [`OpenOrder`] because it carries `avg_price` — the volume
/// weighted fill price, which is what a pair position's PnL is computed
/// against. `OpenOrder::price` is the price the order was *placed* at, and
/// using it as the entry price would quietly understate slippage on every
/// trade.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderStatus {
    pub symbol: Symbol,
    pub order_id: String,
    pub order_link_id: String,
    pub side: Side,
    pub state: OrderState,
    pub qty: Decimal,
    pub cum_exec_qty: Decimal,
    /// Zero when nothing has filled. Bybit sends `""` in that case.
    pub avg_price: Decimal,
    pub updated_time_ms: i64,
}

impl OrderState {
    /// Whether this order will never change again.
    ///
    /// The executor polls until this is true. `PartiallyFilled` is
    /// deliberately *not* terminal: a partial fill is still working, and
    /// treating it as done would record a pair position at the wrong size.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            OrderState::Filled | OrderState::Cancelled | OrderState::Rejected
        )
    }
}
