//! Performance metrics over a set of closed trades.
//!
//! Pure and I/O-free, so every figure here can be pinned by a hand-computed
//! oracle. These are the numbers the pre-registered gate reads, which is why
//! the definitions below are stated precisely rather than left to intuition —
//! a plausible-but-different definition of drawdown or profit factor would
//! change a PASS into a FAIL without anything looking wrong.

use rust_decimal::Decimal;

use crate::sim_exchange::ClosedTrade;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Metrics {
    pub trade_count: usize,
    pub wins: usize,
    pub losses: usize,
    pub win_rate: Decimal,
    /// Mean net PnL per trade, in quote currency, net of fees and funding.
    pub expectancy: Decimal,
    pub gross_profit: Decimal,
    pub gross_loss: Decimal,
    /// `gross_profit / gross_loss`, or `None` when nothing was lost.
    ///
    /// Deliberately not infinity and not a sentinel: a sample with no losing
    /// trades has not demonstrated a profit factor, it has demonstrated that
    /// the sample is too small. The gate fails on `None` rather than seeing a
    /// flattering number.
    pub profit_factor: Option<Decimal>,
    /// Deepest peak-to-trough decline of the equity curve, as a percentage of
    /// the RUNNING PEAK.
    pub max_drawdown_pct: Decimal,
    pub total_fees: Decimal,
    pub total_funding: Decimal,
    pub net_pnl: Decimal,
    pub ambiguous_exits: usize,
}

/// Equity after each trade closes, ordered by `exit_ms`.
///
/// Ordered by exit time rather than by input order because a trade affects
/// equity when it CLOSES, and trades arrive interleaved across symbols.
/// Walking them in arrival order would produce dips and recoveries in a
/// sequence that never happened, inventing drawdowns out of bookkeeping.
pub fn equity_curve(trades: &[ClosedTrade], starting_equity: Decimal) -> Vec<(i64, Decimal)> {
    let mut ordered: Vec<&ClosedTrade> = trades.iter().collect();
    // Ties break on the order given, which is already deterministic upstream:
    // `sort_by_key` is stable, so two trades closing on the same millisecond
    // keep their replay sequence rather than depending on comparison details.
    ordered.sort_by_key(|t| t.exit_ms);

    let mut equity = starting_equity;
    ordered
        .into_iter()
        .map(|t| {
            equity += t.net_pnl;
            (t.exit_ms, equity)
        })
        .collect()
}

pub fn compute(trades: &[ClosedTrade], starting_equity: Decimal) -> Metrics {
    let trade_count = trades.len();

    // A scratch trade is not a win. Counting one as a win would inflate the
    // rate on exactly the strategies that scratch most.
    let wins = trades.iter().filter(|t| t.net_pnl > Decimal::ZERO).count();
    let losses = trades.iter().filter(|t| t.net_pnl < Decimal::ZERO).count();

    let net_pnl: Decimal = trades.iter().map(|t| t.net_pnl).sum();
    let gross_profit: Decimal = trades
        .iter()
        .map(|t| t.net_pnl)
        .filter(|p| *p > Decimal::ZERO)
        .sum();
    let gross_loss: Decimal = trades
        .iter()
        .map(|t| t.net_pnl)
        .filter(|p| *p < Decimal::ZERO)
        .sum::<Decimal>()
        .abs();

    let (win_rate, expectancy) = if trade_count == 0 {
        (Decimal::ZERO, Decimal::ZERO)
    } else {
        let n = Decimal::from(trade_count);
        (Decimal::from(wins) / n, net_pnl / n)
    };

    let profit_factor = if gross_loss.is_zero() {
        None
    } else {
        Some(gross_profit / gross_loss)
    };

    Metrics {
        trade_count,
        wins,
        losses,
        win_rate,
        expectancy,
        gross_profit,
        gross_loss,
        profit_factor,
        max_drawdown_pct: max_drawdown_pct(trades, starting_equity),
        total_fees: trades.iter().map(|t| t.fees).sum(),
        total_funding: trades.iter().map(|t| t.funding).sum(),
        net_pnl,
        ambiguous_exits: trades.iter().filter(|t| t.was_ambiguous).count(),
    }
}

/// Deepest decline from a running peak, as a percentage OF THAT PEAK.
///
/// Measuring against starting equity instead would report a 15% fall after a
/// doubling as a 30% gain and no drawdown at all — hiding precisely the risk
/// that matters once a strategy has run up. The live bot's total-drawdown
/// halt measures against its own high-water mark for the same reason.
fn max_drawdown_pct(trades: &[ClosedTrade], starting_equity: Decimal) -> Decimal {
    let mut peak = starting_equity;
    let mut worst = Decimal::ZERO;

    for (_, equity) in equity_curve(trades, starting_equity) {
        if equity > peak {
            peak = equity;
            continue;
        }
        // A non-positive peak cannot express a meaningful percentage, and
        // dividing by it would panic or produce nonsense.
        if peak > Decimal::ZERO {
            let decline = (peak - equity) / peak * Decimal::ONE_HUNDRED;
            if decline > worst {
                worst = decline;
            }
        }
    }

    worst
}
