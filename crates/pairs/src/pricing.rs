use botcore::{Instrument, Side};
use rust_decimal::Decimal;

/// Largest multiple of `step` not exceeding `x`.
pub fn floor_step(x: Decimal, step: Decimal) -> Decimal {
    (x / step).floor() * step
}

/// Smallest multiple of `step` not below `x`.
pub fn ceil_step(x: Decimal, step: Decimal) -> Decimal {
    (x / step).ceil() * step
}

/// A limit price placed `ticks_through` ticks on the far side of the book.
///
/// Deliberately crossing. Both legs of a pair must fill on the same bar or the
/// position is directional rather than market-neutral, so a resting price that
/// might miss is the wrong trade-off — the spread being captured is worth far
/// more than a few ticks of crossing cost. It is still a limit order: the
/// limit-only rule holds, and the price bounds the worst fill.
///
/// `ticks_through` is a parameter rather than a constant so the unwind ladder
/// can escalate it without a second pricing function that could drift.
pub fn aggressive_limit_price(
    side: Side,
    bid: Decimal,
    ask: Decimal,
    tick: Decimal,
    ticks_through: Decimal,
) -> Decimal {
    match side {
        Side::Buy => ceil_step(ask + tick * ticks_through, tick),
        Side::Sell => {
            let out = floor_step(bid - tick * ticks_through, tick);
            // A sub-penny asset with a wide ladder step can drive this to zero
            // or below, which the exchange rejects outright. One tick is the
            // lowest orderable price.
            if out <= Decimal::ZERO { tick } else { out }
        }
    }
}

/// Quantity for a target notional, lifted to satisfy every exchange floor.
///
/// Always rounds *up* to the quantity step, matching the Python. Rounding down
/// can land under `min_order_qty` and get the order rejected, and a rejected
/// leg is far more expensive than a fraction of a step of extra size.
///
/// The caller must know this: the two legs of a pair are rounded up
/// independently against their own steps and floors, so their notionals are
/// only approximately equal. On a small account the difference is material,
/// and it is why Task 7 journals the realised per-leg notionals rather than
/// assuming the requested ones.
pub fn sized_qty(notional: Decimal, price: Decimal, instrument: &Instrument) -> Decimal {
    let mut qty = notional / price;
    if instrument.min_notional > Decimal::ZERO && qty * price < instrument.min_notional {
        qty = instrument.min_notional / price;
    }
    qty = ceil_step(qty, instrument.qty_step);
    if qty < instrument.min_order_qty {
        qty = instrument.min_order_qty;
    }
    qty
}

#[cfg(test)]
mod tests {
    use super::*;
    use botcore::{Side, Symbol};
    use rust_decimal_macros::dec;

    fn instrument(qty_step: Decimal, min_qty: Decimal, min_notional: Decimal) -> Instrument {
        Instrument {
            symbol: Symbol::new("AAVEUSDT"),
            tick_size: dec!(0.01),
            qty_step,
            min_order_qty: min_qty,
            min_notional,
            launch_time_ms: 0,
        }
    }

    #[test]
    fn steps_round_toward_and_away_from_zero() {
        assert_eq!(floor_step(dec!(10.27), dec!(0.1)), dec!(10.2));
        assert_eq!(ceil_step(dec!(10.21), dec!(0.1)), dec!(10.3));
        assert_eq!(floor_step(dec!(10.2), dec!(0.1)), dec!(10.2));
        assert_eq!(ceil_step(dec!(10.2), dec!(0.1)), dec!(10.2));
    }

    #[test]
    fn a_buy_is_priced_through_the_ask_and_a_sell_through_the_bid() {
        // Crossing the book on purpose: the pair must fill on this bar, and a
        // resting order that misses the move leaves one leg naked.
        let buy = aggressive_limit_price(Side::Buy, dec!(299.90), dec!(300.00), dec!(0.01), dec!(5));
        assert_eq!(buy, dec!(300.05));
        let sell =
            aggressive_limit_price(Side::Sell, dec!(299.90), dec!(300.00), dec!(0.01), dec!(5));
        assert_eq!(sell, dec!(299.85));
    }

    #[test]
    fn a_wider_ladder_step_prices_further_through_the_book() {
        // What the unwind ladder in Task 6 escalates on.
        let sell =
            aggressive_limit_price(Side::Sell, dec!(299.90), dec!(300.00), dec!(0.01), dec!(25));
        assert_eq!(sell, dec!(299.65));
    }

    #[test]
    fn a_sell_price_can_never_be_driven_to_zero_or_below() {
        // A sub-penny asset with a wide ladder step would otherwise produce a
        // non-positive price, which the exchange rejects.
        let sell = aggressive_limit_price(Side::Sell, dec!(0.02), dec!(0.03), dec!(0.01), dec!(5));
        assert_eq!(sell, dec!(0.01));
        assert!(sell > Decimal::ZERO);
    }

    #[test]
    fn quantity_is_the_notional_over_the_price_rounded_up_to_the_step() {
        // Rounding up, matching the Python: an order rounded down below the
        // exchange minimum is rejected outright, which is worse than being a
        // fraction of a step large.
        let q = sized_qty(dec!(100), dec!(300), &instrument(dec!(0.01), dec!(0.01), dec!(0)));
        assert_eq!(q, dec!(0.34));
    }

    #[test]
    fn a_notional_below_the_exchange_minimum_is_raised_to_it() {
        let q = sized_qty(dec!(2), dec!(300), &instrument(dec!(0.01), dec!(0.01), dec!(5)));
        // 5 / 300 == 0.01666..., stepped up to 0.02.
        assert_eq!(q, dec!(0.02));
    }

    #[test]
    fn a_quantity_below_the_minimum_lot_is_raised_to_the_minimum_lot() {
        let q = sized_qty(dec!(1), dec!(300), &instrument(dec!(0.1), dec!(0.1), dec!(0)));
        assert_eq!(q, dec!(0.1));
    }
}
