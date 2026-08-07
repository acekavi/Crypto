//! Closed-trade summary — pure, no I/O.
//!
//! This is deliberately small. Expectancy, profit factor, drawdown and Sharpe
//! belong to Plan 2c; this exists only to let a single run be eyeballed: how
//! many trades, how many won, and — critically — costs broken out as their
//! own line rather than folded into `net_pnl` where a losing edge could hide.

use rust_decimal::Decimal;

use crate::replay::BacktestResult;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunSummary {
    pub trades: usize,
    pub wins: usize,
    pub losses: usize,
    pub gross_pnl: Decimal,
    /// Maker fees only. Kept apart from `total_funding` — netting the two
    /// would hide which cost a strategy is actually paying away its edge to.
    pub total_fees: Decimal,
    pub total_funding: Decimal,
    pub net_pnl: Decimal,
    pub ambiguous_exits: usize,
}

/// A win is `net_pnl > 0`; every non-positive outcome, including an exact
/// breakeven, counts as a loss. There is no third bucket — a trade that
/// costs nothing to open and closes flat is not survivable as "neither",
/// since fees alone virtually never let a real trade land on exactly zero.
pub fn summarise(result: &BacktestResult) -> RunSummary {
    let mut wins = 0usize;
    let mut losses = 0usize;
    let mut gross_pnl = Decimal::ZERO;
    let mut total_fees = Decimal::ZERO;
    let mut total_funding = Decimal::ZERO;

    for t in &result.trades {
        if t.net_pnl > Decimal::ZERO {
            wins += 1;
        } else {
            losses += 1;
        }
        gross_pnl += t.gross_pnl;
        total_fees += t.fees;
        total_funding += t.funding;
    }

    // Derived from the summed components, not summed from each trade's own
    // `net_pnl`, so this equality is true by construction rather than by
    // coincidence of every trade being internally consistent.
    let net_pnl = gross_pnl - total_fees - total_funding;

    RunSummary {
        trades: result.trades.len(),
        wins,
        losses,
        gross_pnl,
        total_fees,
        total_funding,
        net_pnl,
        ambiguous_exits: result.ambiguous_exits,
    }
}
