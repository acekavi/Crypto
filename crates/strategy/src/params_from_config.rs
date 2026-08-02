use rust_decimal::Decimal;
use rust_decimal::prelude::FromPrimitive;

use crate::pullback::StrategyParams;

/// Why a config could not become usable strategy parameters.
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum ParamError {
    #[error("{0} must be greater than zero")]
    ZeroPeriod(&'static str),

    #[error("ema_fast ({fast}) must be shorter than ema_slow ({slow})")]
    EmaOrder { fast: usize, slow: usize },

    #[error("atr_band_min_pct ({min}) must be below atr_band_max_pct ({max})")]
    BandOrder { min: f64, max: f64 },

    #[error("rsi_long_trigger ({long}) must be below rsi_short_trigger ({short})")]
    RsiTriggerOrder { long: f64, short: f64 },

    #[error("{0} is not a finite number")]
    NotFinite(&'static str),
}

fn dec(value: f64, field: &'static str) -> Result<Decimal, ParamError> {
    if !value.is_finite() {
        return Err(ParamError::NotFinite(field));
    }
    Decimal::from_f64(value).ok_or(ParamError::NotFinite(field))
}

/// Build validated strategy parameters from the primitive values the config
/// file carries.
///
/// Takes primitives rather than the config struct because `bot` depends on
/// `strategy`; accepting the struct would invert that and create a cycle.
///
/// Validation here turns what would otherwise be a panic deep inside indicator
/// construction — `Ema::new` asserts `period > 0` — into a clear startup error.
#[allow(clippy::too_many_arguments)]
pub fn params_from_f64_config(
    ema_fast: usize,
    ema_slow: usize,
    ema_entry: usize,
    rsi_period: usize,
    rsi_long_trigger: f64,
    rsi_short_trigger: f64,
    atr_period: usize,
    atr_band_min_pct: f64,
    atr_band_max_pct: f64,
    swing_lookback: usize,
    atr_stop_multiple: f64,
    reward_multiple: f64,
) -> Result<StrategyParams, ParamError> {
    for (value, name) in [
        (ema_fast, "ema_fast"),
        (ema_slow, "ema_slow"),
        (ema_entry, "ema_entry"),
        (rsi_period, "rsi_period"),
        (atr_period, "atr_period"),
        (swing_lookback, "swing_lookback"),
    ] {
        if value == 0 {
            return Err(ParamError::ZeroPeriod(name));
        }
    }

    if ema_fast >= ema_slow {
        return Err(ParamError::EmaOrder {
            fast: ema_fast,
            slow: ema_slow,
        });
    }
    if atr_band_min_pct >= atr_band_max_pct {
        return Err(ParamError::BandOrder {
            min: atr_band_min_pct,
            max: atr_band_max_pct,
        });
    }
    if rsi_long_trigger >= rsi_short_trigger {
        return Err(ParamError::RsiTriggerOrder {
            long: rsi_long_trigger,
            short: rsi_short_trigger,
        });
    }

    Ok(StrategyParams {
        ema_fast,
        ema_slow,
        ema_entry,
        rsi_period,
        rsi_long_trigger: dec(rsi_long_trigger, "rsi_long_trigger")?,
        rsi_short_trigger: dec(rsi_short_trigger, "rsi_short_trigger")?,
        atr_period,
        atr_band_min_pct: dec(atr_band_min_pct, "atr_band_min_pct")?,
        atr_band_max_pct: dec(atr_band_max_pct, "atr_band_max_pct")?,
        swing_lookback,
        atr_stop_multiple: dec(atr_stop_multiple, "atr_stop_multiple")?,
        reward_multiple: dec(reward_multiple, "reward_multiple")?,
        pullback_lookback: 5,
        pullback_atr_fraction: Decimal::new(5, 1),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn valid() -> Result<StrategyParams, ParamError> {
        params_from_f64_config(50, 200, 20, 14, 40.0, 60.0, 14, 0.003, 0.05, 10, 1.5, 2.0)
    }

    #[test]
    fn valid_config_converts_f64_knobs_to_decimal() {
        let p = valid().expect("valid config converts");
        assert_eq!(p.rsi_long_trigger, dec!(40));
        assert_eq!(p.atr_band_min_pct, dec!(0.003));
        assert_eq!(p.atr_stop_multiple, dec!(1.5));
        assert_eq!(p.reward_multiple, dec!(2));
    }

    #[test]
    fn zero_period_is_rejected() {
        // Ema::new/Rsi::new/Atr::new assert on period > 0; catching it here
        // turns a panic deep in indicator construction into a startup error.
        let e = params_from_f64_config(0, 200, 20, 14, 40.0, 60.0, 14, 0.003, 0.05, 10, 1.5, 2.0)
            .expect_err("zero period must be rejected");
        assert!(matches!(e, ParamError::ZeroPeriod(_)));
    }

    #[test]
    fn fast_ema_must_be_shorter_than_slow() {
        let e = params_from_f64_config(200, 50, 20, 14, 40.0, 60.0, 14, 0.003, 0.05, 10, 1.5, 2.0)
            .expect_err("inverted EMAs must be rejected");
        assert!(matches!(e, ParamError::EmaOrder { .. }));
    }

    #[test]
    fn volatility_band_must_be_ordered() {
        let e = params_from_f64_config(50, 200, 20, 14, 40.0, 60.0, 14, 0.05, 0.003, 10, 1.5, 2.0)
            .expect_err("inverted ATR band must be rejected");
        assert!(matches!(e, ParamError::BandOrder { .. }));
    }

    #[test]
    fn rsi_triggers_must_leave_room_between_them() {
        // A long trigger at or above the short trigger means both sides could
        // fire on the same candle.
        let e = params_from_f64_config(50, 200, 20, 14, 60.0, 40.0, 14, 0.003, 0.05, 10, 1.5, 2.0)
            .expect_err("overlapping RSI triggers must be rejected");
        assert!(matches!(e, ParamError::RsiTriggerOrder { .. }));
    }

    #[test]
    fn non_finite_f64_is_rejected_rather_than_producing_a_garbage_decimal() {
        let e = params_from_f64_config(
            50,
            200,
            20,
            14,
            f64::NAN,
            60.0,
            14,
            0.003,
            0.05,
            10,
            1.5,
            2.0,
        )
        .expect_err("NaN must be rejected");
        assert!(matches!(e, ParamError::NotFinite(_)));
    }
}
