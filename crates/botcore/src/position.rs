use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::order::Side;
use crate::symbol::Symbol;

/// An open position as the exchange reports it. The exchange is the source of
/// truth for this type — the bot's own view is always reconciled against it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Position {
    pub symbol: Symbol,
    pub side: Side,
    pub size: Decimal,
    pub entry_price: Decimal,
    /// Liquidation price. `None` when the exchange reports no liquidation risk.
    pub liq_price: Option<Decimal>,
    pub unrealized_pnl: Decimal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Balance {
    /// Total account equity, including unrealized PnL.
    pub equity: Decimal,
    /// Margin available for new positions.
    pub available: Decimal,
}
