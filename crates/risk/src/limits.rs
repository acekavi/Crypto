use botcore::{Position, Symbol};
use rust_decimal::Decimal;

use crate::sizing::RiskParams;

/// Everything the risk layer needs to know about the account right now.
///
/// Assembled by the engine from the exchange (positions, equity) and the
/// journal (today's fill count, the persisted halt, the high-water mark).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccountState {
    pub equity: Decimal,
    pub available: Decimal,
    pub open_positions: Vec<Position>,
    /// Equity at the most recent 00:00 UTC boundary.
    pub day_start_equity: Decimal,
    /// All-time peak equity, persisted across restarts.
    pub high_water_mark: Decimal,
    /// Entries FILLED so far this UTC day. Cancelled and expired limit orders
    /// do not count, which is why this is a fill count rather than a
    /// placement count.
    pub entries_filled_today: u32,
    /// Set when a halt is in force. Persisted, so a restart cannot clear it.
    pub halt_reason: Option<String>,
}

/// Why an entry was refused. Every refusal is nameable — the engine logs the
/// reason rather than silently skipping.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum Refusal {
    #[error("trading is halted: {reason}")]
    Halted { reason: String },

    #[error("daily entry cap reached: {filled} of {cap} filled today")]
    DailyCapReached { filled: u32, cap: u32 },

    #[error("already at the concurrent position limit: {open} of {limit}")]
    TooManyPositions { open: usize, limit: usize },

    #[error("already holding a position in {symbol}")]
    AlreadyInSymbol { symbol: String },

    #[error("daily drawdown {pct} breached the {limit} limit")]
    DailyDrawdown { pct: Decimal, limit: Decimal },

    #[error("total drawdown {pct} from the high-water mark breached the {limit} limit")]
    TotalDrawdown { pct: Decimal, limit: Decimal },

    #[error("liquidation price sits nearer than {multiple}x the stop distance")]
    LiquidationTooClose { multiple: Decimal },

    #[error("position size rounded to zero or equity is non-positive")]
    SizeTooSmall,

    #[error("order notional {notional} is below the instrument minimum {minimum}")]
    BelowMinimumQty { notional: Decimal, minimum: Decimal },

    #[error("order notional {notional} exceeds available margin {available}")]
    InsufficientMargin {
        notional: Decimal,
        available: Decimal,
    },

    #[error(
        "computed stop-limit price {price} is not positive; ATR {atr} is too large relative to the stop"
    )]
    NonPositiveStopLimit { price: Decimal, atr: Decimal },

    #[error(
        "computed target price {price} is not positive; the {multiple}x reward multiple is too large relative to entry"
    )]
    NonPositiveTargetPrice { price: Decimal, multiple: Decimal },
}

/// Whether a new entry in `symbol` is permitted right now.
///
/// Checks the cheap, certain refusals: the persisted halt, the daily cap, the
/// concurrency limit, and one-position-per-symbol. Sizing-dependent refusals
/// live in the manager, which knows the prices.
pub fn check_entry_allowed(
    state: &AccountState,
    params: &RiskParams,
    symbol: &Symbol,
) -> Result<(), Refusal> {
    if let Some(reason) = &state.halt_reason {
        return Err(Refusal::Halted {
            reason: reason.clone(),
        });
    }

    if state.entries_filled_today >= params.max_daily_entries {
        return Err(Refusal::DailyCapReached {
            filled: state.entries_filled_today,
            cap: params.max_daily_entries,
        });
    }

    if state
        .open_positions
        .iter()
        .any(|p| p.symbol.as_str() == symbol.as_str())
    {
        return Err(Refusal::AlreadyInSymbol {
            symbol: symbol.as_str().to_string(),
        });
    }

    if state.open_positions.len() >= params.max_concurrent_positions {
        return Err(Refusal::TooManyPositions {
            open: state.open_positions.len(),
            limit: params.max_concurrent_positions,
        });
    }

    Ok(())
}

/// Whether equity has fallen far enough to trip a halt.
///
/// Daily drawdown measures from the 00:00 UTC equity mark; total drawdown from
/// the all-time high-water mark. A non-positive baseline yields `None` rather
/// than dividing by zero — an account with no recorded baseline has no
/// measurable drawdown.
///
/// We check total drawdown before daily: when both breach, the more severe
/// condition (structural, multi-session degradation from peak) is reported
/// because that reason string is what a human reads when deciding whether
/// clearing the halt is safe.
pub fn drawdown_breach(state: &AccountState, params: &RiskParams) -> Option<Refusal> {
    if state.high_water_mark > Decimal::ZERO {
        let fall = state.high_water_mark - state.equity;
        if fall > Decimal::ZERO {
            let pct = fall / state.high_water_mark;
            if pct >= params.total_drawdown_halt_pct {
                return Some(Refusal::TotalDrawdown {
                    pct,
                    limit: params.total_drawdown_halt_pct,
                });
            }
        }
    }

    if state.day_start_equity > Decimal::ZERO {
        let fall = state.day_start_equity - state.equity;
        if fall > Decimal::ZERO {
            let pct = fall / state.day_start_equity;
            if pct >= params.daily_drawdown_halt_pct {
                return Some(Refusal::DailyDrawdown {
                    pct,
                    limit: params.daily_drawdown_halt_pct,
                });
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use botcore::Side;
    use rust_decimal_macros::dec;

    fn position(sym: &str) -> Position {
        Position {
            symbol: Symbol::new(sym),
            side: Side::Buy,
            size: dec!(1),
            entry_price: dec!(100),
            liq_price: None,
            unrealized_pnl: dec!(0),
        }
    }

    fn healthy() -> AccountState {
        AccountState {
            equity: dec!(10000),
            available: dec!(9000),
            open_positions: vec![],
            day_start_equity: dec!(10000),
            high_water_mark: dec!(10000),
            entries_filled_today: 0,
            halt_reason: None,
        }
    }

    #[test]
    fn a_healthy_account_may_enter() {
        let p = RiskParams::defaults();
        assert!(check_entry_allowed(&healthy(), &p, &Symbol::new("BTCUSDT")).is_ok());
    }

    #[test]
    fn a_persisted_halt_blocks_entry() {
        let mut s = healthy();
        s.halt_reason = Some("daily drawdown".into());
        let e = check_entry_allowed(&s, &RiskParams::defaults(), &Symbol::new("BTCUSDT"))
            .expect_err("a halted account must refuse");
        assert!(matches!(e, Refusal::Halted { .. }));
    }

    #[test]
    fn the_fifth_entry_is_allowed_and_the_sixth_is_not() {
        let p = RiskParams::defaults();
        let mut s = healthy();
        s.entries_filled_today = 4;
        assert!(check_entry_allowed(&s, &p, &Symbol::new("BTCUSDT")).is_ok());

        s.entries_filled_today = 5;
        let e = check_entry_allowed(&s, &p, &Symbol::new("BTCUSDT"))
            .expect_err("the sixth entry of a day must refuse");
        assert!(matches!(e, Refusal::DailyCapReached { cap: 5, .. }));
    }

    #[test]
    fn a_fifth_concurrent_position_is_refused() {
        let p = RiskParams::defaults();
        let mut s = healthy();
        s.open_positions = vec![
            position("BTCUSDT"),
            position("ETHUSDT"),
            position("SOLUSDT"),
            position("XRPUSDT"),
        ];
        let e = check_entry_allowed(&s, &p, &Symbol::new("ADAUSDT"))
            .expect_err("a fifth concurrent position must refuse");
        assert!(matches!(e, Refusal::TooManyPositions { limit: 4, .. }));
    }

    #[test]
    fn a_second_position_in_the_same_symbol_is_refused() {
        let p = RiskParams::defaults();
        let mut s = healthy();
        s.open_positions = vec![position("BTCUSDT")];
        let e = check_entry_allowed(&s, &p, &Symbol::new("BTCUSDT"))
            .expect_err("doubling up on one symbol must refuse");
        assert!(matches!(e, Refusal::AlreadyInSymbol { .. }));
    }

    #[test]
    fn daily_drawdown_at_the_threshold_breaches() {
        let p = RiskParams::defaults();
        let mut s = healthy();
        // −5% exactly from the 00:00 UTC mark.
        s.equity = dec!(9500);
        let b = drawdown_breach(&s, &p).expect("−5% must breach");
        assert!(matches!(b, Refusal::DailyDrawdown { .. }));
    }

    #[test]
    fn daily_drawdown_just_inside_the_threshold_does_not_breach() {
        let p = RiskParams::defaults();
        let mut s = healthy();
        s.equity = dec!(9501);
        assert_eq!(drawdown_breach(&s, &p), None);
    }

    #[test]
    fn total_drawdown_is_measured_from_the_high_water_mark_not_the_day_start() {
        let p = RiskParams::defaults();
        let mut s = healthy();
        // The account peaked at 20,000 and is now at 17,000: −15% from peak,
        // even though it is up on the day.
        s.high_water_mark = dec!(20000);
        s.day_start_equity = dec!(16000);
        s.equity = dec!(17000);
        let b = drawdown_breach(&s, &p).expect("−15% from the peak must breach");
        assert!(matches!(b, Refusal::TotalDrawdown { .. }));
    }

    #[test]
    fn a_non_positive_day_start_cannot_produce_a_division_by_zero() {
        let p = RiskParams::defaults();
        let mut s = healthy();
        s.day_start_equity = dec!(0);
        s.high_water_mark = dec!(0);
        s.equity = dec!(0);
        // No baseline means no measurable drawdown, not a panic.
        assert_eq!(drawdown_breach(&s, &p), None);
    }

    #[test]
    fn a_simultaneous_breach_reports_the_more_severe_total_drawdown() {
        // Down 6% on the day AND 20.1% from the all-time peak. The human clearing
        // this halt must see the structural problem, not the daily one.
        let p = RiskParams::defaults();
        let mut s = healthy();
        s.high_water_mark = dec!(20000);
        s.day_start_equity = dec!(17000);
        s.equity = dec!(15980);
        let b = drawdown_breach(&s, &p).expect("both thresholds breached");
        assert!(
            matches!(b, Refusal::TotalDrawdown { .. }),
            "expected TotalDrawdown when both breach, got {b:?}"
        );
    }
}
