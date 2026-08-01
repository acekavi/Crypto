use rust_decimal::Decimal;

use crate::order::Side;

/// Round `value` down to the nearest multiple of `step`.
///
/// Always rounds toward zero. Used for order quantities, where rounding up
/// would push realized risk above the configured budget.
pub fn round_down_to_step(value: Decimal, step: Decimal) -> Decimal {
    if step.is_zero() {
        return value;
    }
    (value / step).floor() * step
}

/// Round a limit price to a valid tick, moving away from the market.
///
/// A buy limit rests below market and rounds down; a sell limit rests above
/// market and rounds up. This can only make the order less likely to fill,
/// never more aggressive than intended.
pub fn round_price_away_from_market(price: Decimal, tick: Decimal, side: Side) -> Decimal {
    if tick.is_zero() {
        return price;
    }
    let ticks = price / tick;
    let rounded = match side {
        Side::Buy => ticks.floor(),
        Side::Sell => ticks.ceil(),
    };
    rounded * tick
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    #[test]
    fn round_down_to_step_never_rounds_up() {
        // 0.123 with a step of 0.01 must become 0.12, never 0.13 —
        // rounding up would increase position size and therefore risk.
        assert_eq!(round_down_to_step(dec!(0.123), dec!(0.01)), dec!(0.12));
        assert_eq!(round_down_to_step(dec!(0.129), dec!(0.01)), dec!(0.12));
        assert_eq!(round_down_to_step(dec!(1.0), dec!(0.001)), dec!(1.0));
    }

    #[test]
    fn round_down_to_step_handles_whole_number_steps() {
        assert_eq!(round_down_to_step(dec!(157.9), dec!(1)), dec!(157));
    }

    #[test]
    fn buy_limit_rounds_down_away_from_market() {
        // A buy limit rests below market, so "away from market" is downward.
        assert_eq!(
            round_price_away_from_market(dec!(100.567), dec!(0.01), Side::Buy),
            dec!(100.56)
        );
    }

    #[test]
    fn sell_limit_rounds_up_away_from_market() {
        // A sell limit rests above market, so "away from market" is upward.
        assert_eq!(
            round_price_away_from_market(dec!(100.561), dec!(0.01), Side::Sell),
            dec!(100.57)
        );
    }

    #[test]
    fn exact_multiples_are_unchanged_in_both_directions() {
        assert_eq!(
            round_price_away_from_market(dec!(100.56), dec!(0.01), Side::Buy),
            dec!(100.56)
        );
        assert_eq!(
            round_price_away_from_market(dec!(100.56), dec!(0.01), Side::Sell),
            dec!(100.56)
        );
    }

    use proptest::prelude::*;

    proptest! {
        #[test]
        fn round_down_never_exceeds_input(
            value_cents in 0i64..10_000_000i64,
            step_choice in 0usize..4usize,
        ) {
            let value = Decimal::new(value_cents, 4);
            let step = [dec!(0.0001), dec!(0.001), dec!(0.01), dec!(0.1)][step_choice];
            let rounded = round_down_to_step(value, step);
            prop_assert!(rounded <= value, "rounded {rounded} exceeded input {value}");
            prop_assert!(value - rounded < step, "rounded {rounded} lost more than one step");
        }
    }
}
