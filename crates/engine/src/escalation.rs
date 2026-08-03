use botcore::{Side, Symbol};
use rust_decimal::Decimal;

/// How a triggered stop widens when it does not fill.
///
/// The owner's rule forbids market orders anywhere, including stops. A
/// stop-limit that does not fill is therefore widened rather than converted —
/// each rung places the limit further into the move, which is where liquidity
/// is. If every rung is exhausted the position stays open and a human must
/// intervene; that residual gap risk is accepted deliberately.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EscalationLadder {
    /// Offsets in multiples of ATR, ascending.
    pub offsets_atr: Vec<Decimal>,
    /// How long a rung is given to fill before widening.
    pub timeout_ms: i64,
}

impl EscalationLadder {
    /// The spec's ladder: 0.3, 0.6, then 1.2 ATR, 30 seconds per rung.
    pub fn defaults() -> Self {
        EscalationLadder {
            offsets_atr: vec![
                Decimal::new(3, 1),
                Decimal::new(6, 1),
                Decimal::new(12, 1),
            ],
            timeout_ms: 30_000,
        }
    }
}

/// A stop that has triggered and is waiting to fill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TriggeredStop {
    pub symbol: Symbol,
    /// The side of the POSITION, not of the closing order. A long position's
    /// stop sells, so its limit sits below the trigger.
    pub side: Side,
    pub trigger: Decimal,
    pub atr: Decimal,
    /// Index into `offsets_atr` currently in force.
    pub rung: usize,
    pub rung_started_ms: i64,
}

/// What to do with a triggered stop right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EscalationAction {
    /// The current rung still has time to fill.
    Wait,
    /// Move the limit to `rung`'s offset.
    Widen { rung: usize, limit_price: Decimal },
    /// Every rung has been tried. Nothing further is possible without a
    /// market order, which the rules forbid — alert and halt new entries.
    Exhausted,
}

/// Where a stop-limit sits for a given offset.
///
/// Placed BEYOND the trigger, in the direction the position is closing: a long
/// closes by selling, so its limit goes below; a short closes by buying, so
/// its limit goes above. Sitting beyond the trigger is what lets it fill into
/// the move rather than at its edge.
pub fn stop_limit_for(
    trigger: Decimal,
    atr: Decimal,
    side: Side,
    offset_atr: Decimal,
) -> Decimal {
    let offset = atr * offset_atr;
    match side {
        Side::Buy => trigger - offset,
        Side::Sell => trigger + offset,
    }
}

/// Decide the next escalation step.
///
/// `Exhausted` is returned once the current rung is the last one and its
/// timeout has passed — and stays `Exhausted` however long the caller waits,
/// so a stuck stop cannot silently look like it is still progressing.
pub fn next_escalation(
    stop: &TriggeredStop,
    ladder: &EscalationLadder,
    now_ms: i64,
) -> EscalationAction {
    if now_ms - stop.rung_started_ms < ladder.timeout_ms {
        return EscalationAction::Wait;
    }
    let next = stop.rung + 1;
    match ladder.offsets_atr.get(next) {
        Some(offset) => EscalationAction::Widen {
            rung: next,
            limit_price: stop_limit_for(stop.trigger, stop.atr, stop.side, *offset),
        },
        None => EscalationAction::Exhausted,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use botcore::{Side, Symbol};
    use rust_decimal_macros::dec;

    fn stop(rung: usize, rung_started_ms: i64, side: Side) -> TriggeredStop {
        TriggeredStop {
            symbol: Symbol::new("BTCUSDT"),
            side,
            trigger: dec!(100),
            atr: dec!(2),
            rung,
            rung_started_ms,
        }
    }

    #[test]
    fn defaults_widen_through_three_rungs() {
        let l = EscalationLadder::defaults();
        assert_eq!(l.offsets_atr, vec![dec!(0.3), dec!(0.6), dec!(1.2)]);
        assert_eq!(l.timeout_ms, 30_000);
    }

    #[test]
    fn a_long_stop_limit_sits_below_the_trigger() {
        // The position is being SOLD to close, so the limit must sit below the
        // trigger to fill into the move rather than at its edge.
        assert_eq!(
            stop_limit_for(dec!(100), dec!(2), Side::Buy, dec!(0.3)),
            dec!(99.4)
        );
    }

    #[test]
    fn a_short_stop_limit_sits_above_the_trigger() {
        assert_eq!(
            stop_limit_for(dec!(100), dec!(2), Side::Sell, dec!(0.3)),
            dec!(100.6)
        );
    }

    #[test]
    fn a_wider_rung_sits_further_from_the_trigger() {
        let near = stop_limit_for(dec!(100), dec!(2), Side::Buy, dec!(0.3));
        let far = stop_limit_for(dec!(100), dec!(2), Side::Buy, dec!(1.2));
        assert!(far < near, "rung 2 ({far}) was not further out than rung 0 ({near})");
    }

    #[test]
    fn before_the_timeout_the_action_is_to_wait() {
        let l = EscalationLadder::defaults();
        let s = stop(0, 1_000_000, Side::Buy);
        assert_eq!(next_escalation(&s, &l, 1_000_000 + 29_999), EscalationAction::Wait);
    }

    #[test]
    fn at_the_timeout_the_stop_widens_to_the_next_rung() {
        let l = EscalationLadder::defaults();
        let s = stop(0, 1_000_000, Side::Buy);
        assert_eq!(
            next_escalation(&s, &l, 1_000_000 + 30_000),
            EscalationAction::Widen {
                rung: 1,
                limit_price: dec!(98.8), // 100 - 0.6 * 2
            }
        );
    }

    #[test]
    fn the_last_rung_widens_to_the_widest_offset() {
        let l = EscalationLadder::defaults();
        let s = stop(1, 1_000_000, Side::Buy);
        assert_eq!(
            next_escalation(&s, &l, 1_000_000 + 30_000),
            EscalationAction::Widen {
                rung: 2,
                limit_price: dec!(97.6), // 100 - 1.2 * 2
            }
        );
    }

    #[test]
    fn past_the_last_rung_the_ladder_is_exhausted() {
        // Nothing further can be tried WITHOUT a market order, which the
        // owner's rules forbid. The caller must alert and halt new entries.
        let l = EscalationLadder::defaults();
        let s = stop(2, 1_000_000, Side::Buy);
        assert_eq!(
            next_escalation(&s, &l, 1_000_000 + 30_000),
            EscalationAction::Exhausted
        );
    }

    #[test]
    fn an_exhausted_ladder_stays_exhausted_however_long_it_waits() {
        let l = EscalationLadder::defaults();
        let s = stop(2, 1_000_000, Side::Buy);
        assert_eq!(
            next_escalation(&s, &l, 1_000_000 + 86_400_000),
            EscalationAction::Exhausted
        );
    }

    #[test]
    fn a_short_widens_upward() {
        let l = EscalationLadder::defaults();
        let s = stop(0, 1_000_000, Side::Sell);
        assert_eq!(
            next_escalation(&s, &l, 1_000_000 + 30_000),
            EscalationAction::Widen {
                rung: 1,
                limit_price: dec!(101.2), // 100 + 0.6 * 2
            }
        );
    }
}
