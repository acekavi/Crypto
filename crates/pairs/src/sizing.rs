use std::str::FromStr;

use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

use crate::signal::PairParams;

/// What stopped the risk formula from getting the size it asked for.
///
/// Recorded rather than discarded because in this strategy the cap is not an
/// edge case: at realistic spread volatility the risk formula asks for several
/// times equity, so the cap *is* the size on most entries. A `min()` that
/// silently decides position size should be legible in the journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapReason {
    AvailableEquity,
    MaxNotionalMultiple,
}

/// A sizing decision, including the figure that was asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Sizing {
    /// What to actually send, per leg.
    pub notional: Decimal,
    /// What the risk formula asked for before any cap.
    pub uncapped: Decimal,
    pub capped_by: Option<CapReason>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum SizingError {
    #[error("spread sigma must be positive")]
    NonPositiveSigma,
    #[error("per-leg notional resolved to zero or less")]
    NonPositiveNotional,
}

/// Per-leg notional in USDT: risk a fixed fraction of equity per unit of
/// spread volatility, then cap.
///
/// Parity note: the Python converts the `f64` sigma with `Decimal(str(sigma))`.
/// Rust's `f64` `Display` emits the same shortest-round-trip decimal as
/// Python's `str()`, so going through the string is what reproduces it.
/// `Decimal::try_from(f64)` rounds differently and would put this out of
/// parity on the third significant figure.
pub fn per_leg_notional(
    params: &PairParams,
    total_equity: Decimal,
    available_equity: Decimal,
    spread_sigma: f64,
) -> Result<Sizing, SizingError> {
    if spread_sigma <= 0.0 {
        return Err(SizingError::NonPositiveSigma);
    }
    let sigma =
        Decimal::from_str(&spread_sigma.to_string()).map_err(|_| SizingError::NonPositiveSigma)?;
    if sigma <= Decimal::ZERO {
        return Err(SizingError::NonPositiveSigma);
    }

    let uncapped = total_equity * params.risk_pct_of_equity / sigma;

    let multiple_cap = total_equity * params.max_notional_multiple_of_equity;
    let (notional, capped_by) = if uncapped <= available_equity && uncapped <= multiple_cap {
        (uncapped, None)
    } else if available_equity <= multiple_cap {
        (available_equity, Some(CapReason::AvailableEquity))
    } else {
        (multiple_cap, Some(CapReason::MaxNotionalMultiple))
    };

    if notional <= Decimal::ZERO {
        return Err(SizingError::NonPositiveNotional);
    }
    Ok(Sizing {
        notional,
        uncapped,
        capped_by,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use botcore::{Symbol, Timeframe};
    use rust_decimal_macros::dec;

    fn params() -> PairParams {
        PairParams {
            leg_a: Symbol::new("DOGEUSDT"),
            leg_b: Symbol::new("XRPUSDT"),
            timeframe: Timeframe::H1,
            rolling_window: 240,
            entry_z: 3.5,
            stop_z: 4.5,
            target_z: 0.5,
            max_hold_bars: 72,
            fee_per_leg: dec!(0.0002),
            per_leg_notional_usdt: dec!(25),
            risk_pct_of_equity: dec!(0.02),
            max_notional_multiple_of_equity: dec!(1),
            enable_breakeven: true,
            breakeven_r_multiple: dec!(2),
        }
    }

    #[test]
    fn size_scales_with_equity_and_inversely_with_spread_volatility() {
        // Python: 10000 * 0.02 / 0.05 == 4000, under the cap.
        let s = per_leg_notional(&params(), dec!(10000), dec!(10000), 0.05).unwrap();
        assert_eq!(s.notional, dec!(4000));
        assert_eq!(s.capped_by, None);
    }

    #[test]
    fn available_equity_caps_the_size_and_says_so() {
        // Python: 10000 * 0.02 / 0.0004 == 500000, capped to available 8000.
        let s = per_leg_notional(&params(), dec!(10000), dec!(8000), 0.0004).unwrap();
        assert_eq!(s.notional, dec!(8000));
        assert_eq!(s.uncapped, dec!(500000));
        assert_eq!(s.capped_by, Some(CapReason::AvailableEquity));
    }

    #[test]
    fn the_notional_multiple_can_bind_before_available_equity_does() {
        // The knob that exists so 2x gross is a choice rather than an accident.
        let p = PairParams {
            max_notional_multiple_of_equity: dec!(0.25),
            ..params()
        };
        let s = per_leg_notional(&p, dec!(10000), dec!(10000), 0.0004).unwrap();
        assert_eq!(s.notional, dec!(2500));
        assert_eq!(s.capped_by, Some(CapReason::MaxNotionalMultiple));
    }

    #[test]
    fn the_default_multiple_of_one_reproduces_the_python_exactly() {
        // available <= total in every real account state, so with the default
        // multiple the available-equity cap is always the binding one — which
        // is precisely the Python's behaviour.
        let s = per_leg_notional(&params(), dec!(10000), dec!(9000), 0.0001).unwrap();
        assert_eq!(s.notional, dec!(9000));
        assert_eq!(s.capped_by, Some(CapReason::AvailableEquity));
    }

    #[test]
    fn a_non_positive_sigma_is_refused_rather_than_dividing_by_zero() {
        assert_eq!(
            per_leg_notional(&params(), dec!(10000), dec!(10000), 0.0),
            Err(SizingError::NonPositiveSigma)
        );
        assert_eq!(
            per_leg_notional(&params(), dec!(10000), dec!(10000), -0.01),
            Err(SizingError::NonPositiveSigma)
        );
    }

    #[test]
    fn a_drained_account_is_refused_rather_than_sending_a_zero_qty_order() {
        assert_eq!(
            per_leg_notional(&params(), dec!(10000), dec!(0), 0.05),
            Err(SizingError::NonPositiveNotional)
        );
    }

    #[test]
    fn sigma_is_converted_the_way_python_converts_it() {
        // Decimal(str(0.1)) is 0.1, not the binary-expansion 0.1000000000000000055.
        // Decimal::try_from(0.1_f64) would not reproduce this.
        let s = per_leg_notional(&params(), dec!(100), dec!(1000000), 0.1).unwrap();
        assert_eq!(s.notional, dec!(20));
    }
}
