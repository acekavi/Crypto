//! Maker fee and funding accounting — pure functions, no I/O.
//!
//! The fee rate itself is not invented here: it lives in `config/backtest.toml`,
//! checked against Bybit's published schedule and dated, because a stale
//! figure would silently scale every backtest result.

use botcore::Side;
use exchange::bybit::wire::FundingRate;
use rust_decimal::Decimal;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CostModel {
    pub maker_fee_rate: Decimal,
}

impl CostModel {
    /// Maker fee on one fill: notional times the configured rate.
    pub fn maker_fee(&self, qty: Decimal, price: Decimal) -> Decimal {
        qty * price * self.maker_fee_rate
    }
}

/// Funding owed for one settlement. Positive always means "this position
/// PAYS", for both sides — a positive rate means longs pay shorts, so a Buy
/// position pays a positive amount and a Sell position is credited (negative
/// charge) the same magnitude. Returning the sign this way keeps every call
/// site from having to re-derive it; getting it backwards would silently
/// invert the cost of every short in every backtest.
///
/// Notional is `qty * mark_price`. Replay has no intra-candle mark, so
/// callers pass the close of the candle containing the funding timestamp —
/// an honest, stable approximation, unlike entry notional which drifts
/// further from reality the longer a position is held.
pub fn funding_charge(side: Side, qty: Decimal, mark_price: Decimal, rate: Decimal) -> Decimal {
    let notional = qty * mark_price;
    match side {
        Side::Buy => notional * rate,
        Side::Sell => -(notional * rate),
    }
}

/// Funding timestamps strictly inside `(from_ms, to_ms)` — both bounds
/// excluded. A position opened exactly at a funding timestamp has not held
/// through it, and one closed exactly at one is already out, so neither
/// boundary is charged.
pub fn funding_timestamps_in(from_ms: i64, to_ms: i64, rates: &[FundingRate]) -> Vec<&FundingRate> {
    rates
        .iter()
        .filter(|r| from_ms < r.funding_time_ms && r.funding_time_ms < to_ms)
        .collect()
}
