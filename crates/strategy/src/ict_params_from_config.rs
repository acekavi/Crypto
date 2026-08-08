//! Validated `IctParams` from the primitive values a config file carries.
//!
//! Mirrors `params_from_config` for the ICT strategy: takes primitives rather
//! than the config struct, because `bot` depends on `strategy` and accepting
//! the struct would invert that.

use botcore::Timeframe;
use rust_decimal::Decimal;

use crate::ict::IctParams;
use crate::params_from_config::{ParamError, dec};

/// Build validated ICT parameters from the primitive values the config file
/// carries.
///
/// Validation here turns what would otherwise be a panic deep inside indicator
/// construction — `Ema::new` asserts `period > 0` — into a clear startup error.
///
/// The timeframes are deliberately NOT config-driven. The strategy was
/// measured sweeping H4 and executing M15; a config that could change either
/// could silently run a strategy nobody has measured, and nothing asks for it.
#[allow(clippy::too_many_arguments)]
pub fn ict_params_from_config(
    bias_ema: usize,
    swing_lookback: usize,
    atr_period: usize,
    ob_lookback: usize,
    fvg_entry_fraction: f64,
    stop_buffer_atr: f64,
    stop_widen_multiple: f64,
    reward_multiple: f64,
    breakeven_at_r: Option<f64>,
    use_pdh_pdl: bool,
    use_session_levels: bool,
    use_order_block: bool,
    require_mss: bool,
    session_filter: bool,
    allow_long: bool,
    allow_short: bool,
) -> Result<IctParams, ParamError> {
    for (value, name) in [
        (bias_ema, "bias_ema"),
        (swing_lookback, "swing_lookback"),
        (atr_period, "atr_period"),
        (ob_lookback, "ob_lookback"),
    ] {
        if value == 0 {
            return Err(ParamError::ZeroPeriod(name));
        }
    }

    // Converted before the range checks so a NaN reports as not-finite rather
    // than as a zero.
    let fvg_entry_fraction = dec(fvg_entry_fraction, "fvg_entry_fraction")?;
    let stop_buffer_atr = dec(stop_buffer_atr, "stop_buffer_atr")?;
    let stop_widen_multiple = dec(stop_widen_multiple, "stop_widen_multiple")?;
    let reward_multiple = dec(reward_multiple, "reward_multiple")?;
    let breakeven_at_r = breakeven_at_r
        .map(|v| dec(v, "breakeven_at_r"))
        .transpose()?;

    // A zero target would place the take-profit at the entry, and a zero widen
    // multiple would collapse the entry-to-stop distance to nothing — which is
    // the divisor position sizing uses.
    if reward_multiple <= Decimal::ZERO {
        return Err(ParamError::ZeroPeriod("reward_multiple"));
    }
    if stop_widen_multiple <= Decimal::ZERO {
        return Err(ParamError::ZeroPeriod("stop_widen_multiple"));
    }
    // `None` means "never move the stop". Zero would mean "move it to entry
    // immediately", a completely different strategy, so it is refused rather
    // than accepted as a synonym for absence.
    if let Some(b) = breakeven_at_r
        && b <= Decimal::ZERO
    {
        return Err(ParamError::ZeroPeriod("breakeven_at_r"));
    }
    if stop_buffer_atr < Decimal::ZERO {
        return Err(ParamError::ZeroPeriod("stop_buffer_atr"));
    }

    Ok(IctParams {
        bias_ema,
        swing_lookback,
        atr_period,
        ob_lookback,
        fvg_entry_fraction,
        stop_buffer_atr,
        stop_widen_multiple,
        reward_multiple,
        breakeven_at_r,
        use_pdh_pdl,
        use_session_levels,
        use_order_block,
        require_mss,
        session_filter,
        allow_long,
        allow_short,
        structure_tf: Timeframe::H4,
        execution_tf: Timeframe::M15,
        // `mss_window` and the NY session bounds are inert while `require_mss`
        // and `session_filter` are false, and neither is config-driven; they
        // come from the same defaults every other constructor inherits.
        ..IctParams::variant_a()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    /// The frozen `liquidity_sweep_v2` values, as a config file states them.
    fn frozen() -> Result<IctParams, ParamError> {
        ict_params_from_config(
            50,
            5,
            14,
            5,
            0.5,
            0.0,
            1.0,
            5.0,
            Some(2.0),
            true,
            false,
            true,
            false,
            false,
            true,
            true,
        )
    }

    #[test]
    fn a_valid_config_produces_the_frozen_v2_parameters() {
        let p = frozen().expect("valid config");
        assert_eq!(p, IctParams::liquidity_sweep_v2());
    }

    #[test]
    fn a_zero_period_is_rejected_rather_than_panicking_inside_an_indicator() {
        // Ema::new asserts period > 0; without this the failure is a panic deep
        // in indicator construction instead of a clear startup error.
        let e = ict_params_from_config(
            0, 5, 14, 5, 0.5, 0.0, 1.0, 5.0, None, true, false, true, false, false, true, true,
        )
        .expect_err("a zero period must be rejected");
        assert_eq!(e, ParamError::ZeroPeriod("bias_ema"));
    }

    #[test]
    fn a_non_finite_value_is_rejected() {
        let e = ict_params_from_config(
            50,
            5,
            14,
            5,
            f64::NAN,
            0.0,
            1.0,
            5.0,
            Some(2.0),
            true,
            false,
            true,
            false,
            false,
            true,
            true,
        )
        .expect_err("NaN must be rejected");
        assert_eq!(e, ParamError::NotFinite("fvg_entry_fraction"));
    }

    #[test]
    fn a_config_with_no_breakeven_yields_none_not_zero() {
        // Zero would mean "move the stop to entry immediately", which is a very
        // different strategy from "never move it".
        let p = ict_params_from_config(
            50, 5, 14, 5, 0.5, 0.0, 1.0, 5.0, None, true, false, true, false, false, true, true,
        )
        .expect("valid config");
        assert_eq!(p.breakeven_at_r, None);
    }

    #[test]
    fn a_non_positive_reward_multiple_is_rejected() {
        let e = ict_params_from_config(
            50,
            5,
            14,
            5,
            0.5,
            0.0,
            1.0,
            0.0,
            Some(2.0),
            true,
            false,
            true,
            false,
            false,
            true,
            true,
        )
        .expect_err("a zero target must be rejected");
        assert_eq!(e, ParamError::ZeroPeriod("reward_multiple"));
    }

    #[test]
    fn a_zero_breakeven_multiple_is_rejected_rather_than_read_as_absence() {
        let e = ict_params_from_config(
            50,
            5,
            14,
            5,
            0.5,
            0.0,
            1.0,
            5.0,
            Some(0.0),
            true,
            false,
            true,
            false,
            false,
            true,
            true,
        )
        .expect_err("a zero breakeven multiple must be rejected");
        assert_eq!(e, ParamError::ZeroPeriod("breakeven_at_r"));
    }

    #[test]
    fn a_zero_stop_widen_multiple_is_rejected() {
        // It multiplies the entry-to-stop distance, which is the divisor
        // position sizing uses; zero would size an infinite position.
        let e = ict_params_from_config(
            50,
            5,
            14,
            5,
            0.5,
            0.0,
            0.0,
            5.0,
            Some(2.0),
            true,
            false,
            true,
            false,
            false,
            true,
            true,
        )
        .expect_err("a zero widen multiple must be rejected");
        assert_eq!(e, ParamError::ZeroPeriod("stop_widen_multiple"));
    }

    #[test]
    fn the_timeframes_are_fixed_regardless_of_config() {
        // H4/M15 are what the strategy was measured on and are deliberately
        // not reachable from a config file.
        let p = frozen().expect("valid config");
        assert_eq!(p.structure_tf, Timeframe::H4);
        assert_eq!(p.execution_tf, Timeframe::M15);
    }

    #[test]
    fn the_f64_knobs_arrive_as_exact_decimals() {
        let p = frozen().expect("valid config");
        assert_eq!(p.fvg_entry_fraction, dec!(0.5));
        assert_eq!(p.stop_buffer_atr, dec!(0));
        assert_eq!(p.stop_widen_multiple, dec!(1));
        assert_eq!(p.reward_multiple, dec!(5));
        assert_eq!(p.breakeven_at_r, Some(dec!(2)));
    }
}
