use botcore::{OrderState, Side, Symbol};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// Which leg of the pair. `A` is the numerator of the log spread.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Leg {
    A,
    B,
}

/// What one leg's order actually did, as the exchange reports it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LegReport {
    pub leg: Leg,
    pub symbol: Symbol,
    pub side: Side,
    pub requested_qty: Decimal,
    pub executed_qty: Decimal,
    /// Volume-weighted fill price. Zero when nothing executed.
    pub avg_price: Decimal,
    pub order_id: String,
    /// `None` when the exchange has no record of the order at all — a
    /// placement that never landed, which is not the same as a rejection.
    pub state: Option<OrderState>,
}

impl LegReport {
    /// Did this leg get everything it asked for?
    pub fn is_fully_filled(&self) -> bool {
        self.executed_qty > Decimal::ZERO && self.executed_qty >= self.requested_qty
    }

    /// Is the account carrying a position because of this leg?
    ///
    /// Keyed on executed quantity and never on `state`, because a `Cancelled`
    /// order can carry a partial fill. Reading the status instead is how a
    /// live position becomes invisible to its own bot.
    pub fn has_exposure(&self) -> bool {
        self.executed_qty > Decimal::ZERO
    }
}

/// What the two leg reports add up to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Settlement {
    /// Both legs fully filled — the only state in which a pair position exists.
    Opened { a: LegReport, b: LegReport },
    /// Neither leg executed anything. Cancel any remnant and stay flat.
    Flat,
    /// The pair is not whole and at least one leg carries exposure. Every
    /// listed leg must be flattened before this bot does anything else.
    Unwind(Vec<LegReport>),
}

/// Classify a two-leg placement outcome.
///
/// Pure and total: every combination of leg reports maps to exactly one of
/// three states, and there is no path that returns success while leaving
/// exposure behind. That totality is the point — the Python handled this case
/// in an `except Exception: pass` and could leave a naked leg open indefinitely.
///
/// The strict rule (anything short of both-legs-full gets unwound) costs one
/// round-turn of fees on a rare partial fill in a thin book. Carrying a
/// lopsided pair instead would mean running directional risk on a strategy
/// that has no directional edge, which is a far worse trade.
pub fn settle(a: LegReport, b: LegReport) -> Settlement {
    if a.is_fully_filled() && b.is_fully_filled() {
        return Settlement::Opened { a, b };
    }
    let exposed: Vec<LegReport> = [a, b].into_iter().filter(LegReport::has_exposure).collect();
    if exposed.is_empty() {
        Settlement::Flat
    } else {
        Settlement::Unwind(exposed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use botcore::{OrderState, Side, Symbol};
    use rust_decimal_macros::dec;

    fn report(leg: Leg, requested: Decimal, executed: Decimal, state: OrderState) -> LegReport {
        LegReport {
            leg,
            symbol: Symbol::new(if leg == Leg::A { "AAVEUSDT" } else { "ETHUSDT" }),
            side: if leg == Leg::A { Side::Buy } else { Side::Sell },
            requested_qty: requested,
            executed_qty: executed,
            avg_price: dec!(300),
            order_id: "oid".into(),
            state: Some(state),
        }
    }

    #[test]
    fn two_fully_filled_legs_open_the_position() {
        let s = settle(
            report(Leg::A, dec!(1), dec!(1), OrderState::Filled),
            report(Leg::B, dec!(2), dec!(2), OrderState::Filled),
        );
        assert!(matches!(s, Settlement::Opened { .. }));
    }

    #[test]
    fn two_untouched_legs_leave_the_account_flat() {
        let s = settle(
            report(Leg::A, dec!(1), dec!(0), OrderState::Cancelled),
            report(Leg::B, dec!(2), dec!(0), OrderState::Rejected),
        );
        assert_eq!(s, Settlement::Flat);
    }

    #[test]
    fn one_filled_leg_and_one_rejected_leg_is_the_orphan_case() {
        // The exact shape of the Python's worst bug: leg A fills, leg B is
        // rejected, and the account is left holding naked directional risk.
        let s = settle(
            report(Leg::A, dec!(1), dec!(1), OrderState::Filled),
            report(Leg::B, dec!(2), dec!(0), OrderState::Rejected),
        );
        match s {
            Settlement::Unwind(legs) => {
                assert_eq!(legs.len(), 1);
                assert_eq!(legs[0].leg, Leg::A);
                assert_eq!(legs[0].executed_qty, dec!(1));
            }
            other => panic!("expected Unwind, got {other:?}"),
        }
    }

    #[test]
    fn a_cancelled_order_that_partially_filled_still_counts_as_exposure() {
        // Judging by orderStatus rather than by executed quantity is how a
        // position goes unnoticed: Cancelled reads as "nothing happened" and
        // cumExecQty says otherwise.
        let s = settle(
            report(Leg::A, dec!(1), dec!(0.4), OrderState::Cancelled),
            report(Leg::B, dec!(2), dec!(0), OrderState::Cancelled),
        );
        match s {
            Settlement::Unwind(legs) => {
                assert_eq!(legs.len(), 1);
                assert_eq!(legs[0].executed_qty, dec!(0.4));
            }
            other => panic!("expected Unwind, got {other:?}"),
        }
    }

    #[test]
    fn a_lopsided_pair_is_unwound_entirely_rather_than_carried() {
        // Leg A full, leg B a tenth: carrying this is 90% naked directional
        // risk on a strategy with no directional edge. Both sides are flattened.
        let s = settle(
            report(Leg::A, dec!(1), dec!(1), OrderState::Filled),
            report(Leg::B, dec!(2), dec!(0.2), OrderState::Cancelled),
        );
        match s {
            Settlement::Unwind(legs) => assert_eq!(legs.len(), 2),
            other => panic!("expected Unwind, got {other:?}"),
        }
    }

    #[test]
    fn an_order_the_exchange_never_heard_of_carries_no_exposure() {
        let mut a = report(Leg::A, dec!(1), dec!(0), OrderState::New);
        a.state = None;
        let s = settle(a, report(Leg::B, dec!(2), dec!(0), OrderState::Cancelled));
        assert_eq!(s, Settlement::Flat);
    }
}
