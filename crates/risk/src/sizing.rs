use botcore::money::round_down_to_step;
use rust_decimal::Decimal;

/// The owner's hard risk envelope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RiskParams {
    /// Fraction of equity risked per trade, e.g. 0.01 for 1%.
    pub risk_pct: Decimal,
    pub max_concurrent_positions: usize,
    pub max_daily_entries: u32,
    /// Fraction, e.g. 0.05 for a −5% daily halt.
    pub daily_drawdown_halt_pct: Decimal,
    /// Fraction, e.g. 0.15 for a −15% total halt.
    pub total_drawdown_halt_pct: Decimal,
    /// Liquidation must sit at least this many stop-distances from entry.
    pub liq_buffer_multiple: Decimal,
}

impl RiskParams {
    /// The spec's envelope.
    pub fn defaults() -> Self {
        RiskParams {
            risk_pct: Decimal::new(1, 2),              // 0.01
            max_concurrent_positions: 4,
            max_daily_entries: 5,
            daily_drawdown_halt_pct: Decimal::new(5, 2),  // 0.05
            total_drawdown_halt_pct: Decimal::new(15, 2), // 0.15
            liq_buffer_multiple: Decimal::from(3),
        }
    }
}

/// Position size from the risk budget and the stop distance.
///
/// `size = (risk_pct × equity) ÷ stop_distance`, rounded DOWN to `qty_step`.
/// Deriving size from stop distance rather than from available margin is what
/// makes leverage a margin-efficiency setting rather than a risk multiplier:
/// the amount at risk is the same at 1x as at 10x.
///
/// Returns `None` — never a zero-size order — when equity is non-positive, the
/// stop distance is zero, or the result rounds away to nothing.
pub fn position_size(
    equity: Decimal,
    risk_pct: Decimal,
    stop_distance: Decimal,
    qty_step: Decimal,
) -> Option<Decimal> {
    if equity <= Decimal::ZERO || stop_distance <= Decimal::ZERO || risk_pct <= Decimal::ZERO {
        return None;
    }
    let budget = equity * risk_pct;
    let raw = budget / stop_distance;
    let stepped = round_down_to_step(raw, qty_step);
    if stepped <= Decimal::ZERO {
        return None;
    }
    Some(stepped)
}

/// Whether liquidation sits far enough beyond the stop.
///
/// This is what makes a limit-only stop survivable: by requiring liquidation to
/// be at least `buffer_multiple` stop-distances away, a gap through the stop has
/// room for the escalation ladder to fill before the exchange force-closes.
/// In practice it caps the leverage any individual setup can use — setups
/// needing more are rejected outright rather than quietly resized.
///
/// The direction is inferred from stop versus entry: a stop below entry is a
/// long, above is a short. An absent liquidation price means the exchange sees
/// no liquidation risk and is treated as safe.
pub fn liquidation_is_safe(
    entry: Decimal,
    stop: Decimal,
    liq_price: Option<Decimal>,
    buffer_multiple: Decimal,
) -> bool {
    let Some(liq) = liq_price else {
        return true;
    };
    let distance = (entry - stop).abs();
    if distance.is_zero() {
        return false;
    }
    let required = distance * buffer_multiple;
    if stop < entry {
        // Long: liquidation lies below, and must be at least `required` below.
        liq <= entry - required
    } else {
        // Short: liquidation lies above.
        liq >= entry + required
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn size_is_risk_budget_divided_by_stop_distance() {
        // 1% of 10,000 = 100 risked; a $5 stop distance buys 20 units.
        let size = position_size(dec!(10000), dec!(0.01), dec!(5), dec!(0.001))
            .expect("valid inputs produce a size");
        assert_eq!(size, dec!(20));
    }

    #[test]
    fn size_rounds_down_to_the_quantity_step() {
        // 100 / 3 = 33.333...; with a 0.01 step that must become 33.33,
        // never 33.34 — rounding up would risk more than the budget.
        let size = position_size(dec!(10000), dec!(0.01), dec!(3), dec!(0.01))
            .expect("valid inputs produce a size");
        assert_eq!(size, dec!(33.33));
    }

    #[test]
    fn zero_stop_distance_yields_none_rather_than_dividing_by_zero() {
        assert_eq!(position_size(dec!(10000), dec!(0.01), dec!(0), dec!(0.001)), None);
    }

    #[test]
    fn non_positive_equity_yields_none() {
        // A zero-equity account (an unfunded testnet account, for instance)
        // must produce no position rather than a zero-size order.
        assert_eq!(position_size(dec!(0), dec!(0.01), dec!(5), dec!(0.001)), None);
        assert_eq!(position_size(dec!(-100), dec!(0.01), dec!(5), dec!(0.001)), None);
    }

    #[test]
    fn a_size_rounding_to_zero_yields_none() {
        // Tiny equity against a wide stop and a coarse step rounds to nothing;
        // that must be None, not an order for zero units.
        assert_eq!(position_size(dec!(10), dec!(0.01), dec!(5000), dec!(1)), None);
    }

    #[test]
    fn liquidation_far_beyond_the_stop_is_safe() {
        // Long at 100, stop at 95 (distance 5), buffer 3 => liquidation must
        // be at or below 85. At 80 it is comfortably clear.
        assert!(liquidation_is_safe(dec!(100), dec!(95), Some(dec!(80)), dec!(3)));
    }

    #[test]
    fn liquidation_inside_the_buffer_is_unsafe() {
        // Liquidation at 90 is only 2 stop-distances away, inside the 3x rule.
        assert!(!liquidation_is_safe(dec!(100), dec!(95), Some(dec!(90)), dec!(3)));
    }

    #[test]
    fn liquidation_between_entry_and_stop_is_unsafe() {
        // The exchange would close the position before the stop ever triggers.
        assert!(!liquidation_is_safe(dec!(100), dec!(95), Some(dec!(97)), dec!(3)));
    }

    #[test]
    fn short_side_buffer_is_measured_upward() {
        // Short at 100, stop at 105 (distance 5), buffer 3 => liquidation must
        // be at or above 115.
        assert!(liquidation_is_safe(dec!(100), dec!(105), Some(dec!(120)), dec!(3)));
        assert!(!liquidation_is_safe(dec!(100), dec!(105), Some(dec!(110)), dec!(3)));
    }

    #[test]
    fn absent_liquidation_price_is_treated_as_safe() {
        // Bybit reports no liquidation price when there is no liquidation risk
        // on the position. Absent must not be read as zero, which would look
        // infinitely far away on a short and adjacent on a long.
        assert!(liquidation_is_safe(dec!(100), dec!(95), None, dec!(3)));
    }

    use proptest::prelude::*;

    proptest! {
        /// The realized risk of a position — size × stop distance — must never
        /// exceed the budget, for any combination of equity, stop distance and
        /// quantity step. This is the invariant the whole risk model rests on.
        #[test]
        fn realized_risk_never_exceeds_the_budget(
            equity_cents in 1_000i64..1_000_000_000i64,
            stop_cents in 1i64..10_000_000i64,
            step_choice in 0usize..4usize,
        ) {
            let equity = Decimal::new(equity_cents, 2);
            let stop_distance = Decimal::new(stop_cents, 2);
            let qty_step = [dec!(0.0001), dec!(0.001), dec!(0.01), dec!(1)][step_choice];
            let risk_pct = dec!(0.01);

            if let Some(size) = position_size(equity, risk_pct, stop_distance, qty_step) {
                let budget = equity * risk_pct;
                let realized = size * stop_distance;
                prop_assert!(
                    realized <= budget,
                    "realized risk {realized} exceeded budget {budget} \
                     (equity {equity}, stop {stop_distance}, step {qty_step})"
                );
                prop_assert!(size > Decimal::ZERO, "a Some size must be positive");
            }
        }
    }
}
