use botcore::{Symbol, Timeframe};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// Which way the spread is held.
///
/// `LongSpread` is long leg A and short leg B; `ShortSpread` is the mirror.
/// Named for the spread rather than for the legs because every threshold in
/// this module is expressed in spread z-units.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PairSide {
    LongSpread,
    ShortSpread,
}

impl PairSide {
    /// Has this side's target been reached at `z`?
    ///
    /// The long-spread thresholds are the negation of the short-spread ones,
    /// and that negation is written here — once — on purpose. The live config
    /// runs `target_z = -4.5` on BNB/XAUT, which puts the *long*-spread target
    /// at `z >= +4.5`: a deliberate "hold until the spread overshoots to the
    /// opposite extreme", and something that reads exactly like a sign error
    /// at any call site that re-derives it.
    pub fn target_reached(self, z: f64, target_z: f64) -> bool {
        match self {
            PairSide::ShortSpread => z <= target_z,
            PairSide::LongSpread => z >= -target_z,
        }
    }

    /// Has this side's stop been hit at `z`? The spread moved further against
    /// the position rather than reverting.
    pub fn stop_hit(self, z: f64, stop_z: f64) -> bool {
        match self {
            PairSide::ShortSpread => z >= stop_z,
            PairSide::LongSpread => z <= -stop_z,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            PairSide::LongSpread => "long_spread",
            PairSide::ShortSpread => "short_spread",
        }
    }
}

/// Everything that defines one pair's behaviour.
///
/// `Clone` rather than `Copy` because it carries two `Symbol`s; it is cloned
/// once per bot at startup and never in a hot path.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PairParams {
    pub leg_a: Symbol,
    pub leg_b: Symbol,
    pub timeframe: Timeframe,
    pub rolling_window: usize,
    pub entry_z: f64,
    pub stop_z: f64,
    pub target_z: f64,
    pub max_hold_bars: i64,
    pub fee_per_leg: f64,
    /// Used only when `risk_pct_of_equity` is zero, which is the backtest's
    /// fixed-size mode. Every live profile runs risk-based sizing.
    pub per_leg_notional_usdt: Decimal,
    pub risk_pct_of_equity: Decimal,
    /// Ceiling on per-leg notional as a multiple of total equity. Defaults to
    /// `1`, which reproduces the Python's behaviour exactly — see
    /// `sizing::per_leg_notional` for why that default is worth arguing about.
    pub max_notional_multiple_of_equity: Decimal,
    pub enable_breakeven: bool,
    pub breakeven_r_multiple: Decimal,
}

impl PairParams {
    pub fn display_pair(&self) -> String {
        format!("{}/{}", self.leg_a, self.leg_b)
    }

    /// `|entry - target| / |stop - entry|`. Reported at startup so a
    /// misconfigured pair is visible in the first log line.
    pub fn reward_risk_ratio(&self) -> f64 {
        (self.entry_z - self.target_z).abs() / (self.stop_z - self.entry_z).abs()
    }

    /// `doge_xrp` for DOGEUSDT/XRPUSDT. Used as the `orderLinkId` prefix, so
    /// every order this bot places is attributable to it from Bybit's UI.
    pub fn slug(&self) -> String {
        fn norm(s: &Symbol) -> String {
            s.as_str().trim_end_matches("USDT").to_lowercase()
        }
        format!("{}_{}", norm(&self.leg_a), norm(&self.leg_b))
    }
}

/// Why a position was closed. The string forms are what the journal and the
/// Python dashboard already read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExitReason {
    Breakeven,
    Time,
    Target,
    Stop,
}

impl ExitReason {
    pub fn as_str(self) -> &'static str {
        match self {
            ExitReason::Breakeven => "breakeven",
            ExitReason::Time => "time",
            ExitReason::Target => "target",
            ExitReason::Stop => "stop",
        }
    }
}

/// The entry and exit rules. Pure: no clock, no I/O, no interior mutability,
/// so every decision is reproducible from its arguments alone.
#[derive(Debug, Clone)]
pub struct SignalEngine {
    params: PairParams,
}

impl SignalEngine {
    pub fn new(params: PairParams) -> Self {
        Self { params }
    }

    pub fn params(&self) -> &PairParams {
        &self.params
    }

    /// A z at or beyond the entry band, or nothing.
    ///
    /// The comparison is inclusive, matching the Python: an exclusive one
    /// would skip a bar that landed exactly on the threshold.
    pub fn entry_signal(&self, z: f64) -> Option<PairSide> {
        if z >= self.params.entry_z {
            Some(PairSide::ShortSpread)
        } else if z <= -self.params.entry_z {
            Some(PairSide::LongSpread)
        } else {
            None
        }
    }

    /// Has the spread retraced far enough to justify protecting the trade?
    ///
    /// Measured in risk bands (`stop_z - entry_z`) rather than in absolute z,
    /// so the rule transfers across pairs with different thresholds.
    pub fn should_arm_breakeven(&self, side: PairSide, z: f64) -> bool {
        if !self.params.enable_breakeven {
            return false;
        }
        let risk_band = self.params.stop_z - self.params.entry_z;
        let arm_multiple: f64 = self
            .params
            .breakeven_r_multiple
            .try_into()
            .unwrap_or(f64::INFINITY);
        match side {
            PairSide::ShortSpread => z <= self.params.entry_z - arm_multiple * risk_band,
            PairSide::LongSpread => z >= -self.params.entry_z + arm_multiple * risk_band,
        }
    }

    /// Why this position should close now, if it should.
    ///
    /// Precedence is `breakeven → time → target → stop`, matching the Python
    /// exactly. It looks wrong — a bar that trips both the stop and max-hold
    /// is labelled `time` — but all four exits resolve on the same bar at the
    /// same price, so the order changes the label and nothing else. Keeping it
    /// is what lets a Rust backtest be compared to a Python one line by line.
    pub fn exit_reason(
        &self,
        side: PairSide,
        z: f64,
        age_bars: i64,
        breakeven_armed: bool,
        pnl_fraction: Option<Decimal>,
    ) -> Option<ExitReason> {
        if breakeven_armed && pnl_fraction.is_some_and(|p| p <= Decimal::ZERO) {
            return Some(ExitReason::Breakeven);
        }
        if age_bars > self.params.max_hold_bars {
            return Some(ExitReason::Time);
        }
        if side.target_reached(z, self.params.target_z) {
            return Some(ExitReason::Target);
        }
        if side.stop_hit(z, self.params.stop_z) {
            return Some(ExitReason::Stop);
        }
        None
    }
}

/// Unrealised PnL as a fraction of one leg's notional, net of all four fee legs
/// (two in, two out).
///
/// Assumes the two legs carry equal notional. They do not exactly — `sized_qty`
/// rounds each leg up independently to its own `qty_step` and `min_order_qty` —
/// so on a small account this is an approximation, and it is the input to the
/// breakeven exit. Task 7 journals the realised per-leg notionals so the size
/// of that approximation is measurable rather than assumed.
pub fn unrealized_pnl_fraction(
    side: PairSide,
    a_entry: Decimal,
    b_entry: Decimal,
    a_now: Decimal,
    b_now: Decimal,
    fee_per_leg: f64,
) -> Decimal {
    let a_ret = a_now / a_entry - Decimal::ONE;
    let b_ret = b_now / b_entry - Decimal::ONE;
    let gross = match side {
        PairSide::LongSpread => a_ret - b_ret,
        PairSide::ShortSpread => b_ret - a_ret,
    };
    let fees = Decimal::try_from(4.0 * fee_per_leg).unwrap_or(Decimal::ZERO);
    gross - fees
}

#[cfg(test)]
mod tests {
    use super::*;
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
            fee_per_leg: 0.0002,
            per_leg_notional_usdt: dec!(25),
            risk_pct_of_equity: dec!(0.02),
            max_notional_multiple_of_equity: dec!(1),
            enable_breakeven: true,
            breakeven_r_multiple: dec!(2),
        }
    }

    #[test]
    fn a_high_positive_z_is_a_short_spread_signal() {
        let e = SignalEngine::new(params());
        assert_eq!(e.entry_signal(3.6), Some(PairSide::ShortSpread));
    }

    #[test]
    fn a_low_negative_z_is_a_long_spread_signal() {
        let e = SignalEngine::new(params());
        assert_eq!(e.entry_signal(-3.6), Some(PairSide::LongSpread));
    }

    #[test]
    fn no_signal_is_produced_inside_the_band() {
        let e = SignalEngine::new(params());
        assert_eq!(e.entry_signal(1.2), None);
        assert_eq!(e.entry_signal(-3.4), None);
    }

    #[test]
    fn the_band_edge_is_inclusive() {
        // Python uses >= and <=; an exclusive comparison would silently skip
        // the exact-threshold bar.
        let e = SignalEngine::new(params());
        assert_eq!(e.entry_signal(3.5), Some(PairSide::ShortSpread));
        assert_eq!(e.entry_signal(-3.5), Some(PairSide::LongSpread));
    }

    #[test]
    fn a_short_spread_takes_target_when_z_reverts() {
        let e = SignalEngine::new(params());
        assert_eq!(
            e.exit_reason(PairSide::ShortSpread, 0.4, 1, false, None),
            Some(ExitReason::Target)
        );
    }

    #[test]
    fn a_long_spread_takes_the_stop_when_z_keeps_widening() {
        let e = SignalEngine::new(params());
        assert_eq!(
            e.exit_reason(PairSide::LongSpread, -4.6, 1, false, None),
            Some(ExitReason::Stop)
        );
    }

    #[test]
    fn the_time_stop_fires_after_max_hold_bars() {
        let e = SignalEngine::new(params());
        assert_eq!(
            e.exit_reason(PairSide::ShortSpread, 2.0, 73, false, None),
            Some(ExitReason::Time)
        );
        assert_eq!(e.exit_reason(PairSide::ShortSpread, 2.0, 72, false, None), None);
    }

    #[test]
    fn time_outranks_stop_on_a_bar_that_hits_both() {
        // Deliberate parity with the Python. Both exits resolve on the same
        // bar at the same price, so this changes the label and nothing else —
        // and matching the label keeps backtest comparisons honest.
        let e = SignalEngine::new(params());
        assert_eq!(
            e.exit_reason(PairSide::ShortSpread, 5.0, 100, false, None),
            Some(ExitReason::Time)
        );
    }

    #[test]
    fn a_negative_target_z_puts_the_long_spread_target_at_the_opposite_extreme() {
        // BNB/XAUT runs target_z = -4.5. Long-spread targets at +4.5. This
        // reads like a sign error and is not one.
        let p = PairParams {
            target_z: -4.5,
            ..params()
        };
        let e = SignalEngine::new(p);
        assert_eq!(e.exit_reason(PairSide::LongSpread, 4.6, 1, false, None), Some(ExitReason::Target));
        assert_eq!(e.exit_reason(PairSide::LongSpread, 4.4, 1, false, None), None);
        assert_eq!(e.exit_reason(PairSide::ShortSpread, -4.6, 1, false, None), Some(ExitReason::Target));
    }

    #[test]
    fn breakeven_arms_at_two_risk_bands_of_favourable_movement() {
        // entry 3.5, stop 4.5 -> risk band 1.0; 2R of retracement is z <= 1.5.
        let e = SignalEngine::new(params());
        assert!(e.should_arm_breakeven(PairSide::ShortSpread, 1.5));
        assert!(!e.should_arm_breakeven(PairSide::ShortSpread, 1.6));
        assert!(e.should_arm_breakeven(PairSide::LongSpread, -1.5));
        assert!(!e.should_arm_breakeven(PairSide::LongSpread, -1.6));
    }

    #[test]
    fn breakeven_never_arms_when_the_feature_is_disabled() {
        // All three live profiles run with breakeven off.
        let p = PairParams {
            enable_breakeven: false,
            ..params()
        };
        let e = SignalEngine::new(p);
        assert!(!e.should_arm_breakeven(PairSide::ShortSpread, 0.0));
    }

    #[test]
    fn an_armed_breakeven_exits_the_moment_pnl_turns_non_positive() {
        let e = SignalEngine::new(params());
        let pnl = unrealized_pnl_fraction(
            PairSide::ShortSpread,
            dec!(10),
            dec!(1),
            dec!(10.3),
            dec!(0.99),
            0.0002,
        );
        assert!(pnl <= dec!(0), "pnl fraction was {pnl}");
        assert_eq!(
            e.exit_reason(PairSide::ShortSpread, 1.6, 5, true, Some(pnl)),
            Some(ExitReason::Breakeven)
        );
    }

    #[test]
    fn an_unarmed_breakeven_ignores_a_losing_pnl() {
        let e = SignalEngine::new(params());
        assert_eq!(
            e.exit_reason(PairSide::ShortSpread, 1.6, 5, false, Some(dec!(-0.5))),
            None
        );
    }

    #[test]
    fn pnl_fraction_nets_four_legs_of_fees() {
        // Two legs in, two legs out.
        let pnl = unrealized_pnl_fraction(
            PairSide::LongSpread,
            dec!(100),
            dec!(100),
            dec!(100),
            dec!(100),
            0.0002,
        );
        assert_eq!(pnl, dec!(-0.0008));
    }

    #[test]
    fn pair_side_serialises_to_the_python_strings() {
        // The journal and the Python dashboard both read these.
        assert_eq!(
            serde_json::to_string(&PairSide::LongSpread).unwrap(),
            "\"long_spread\""
        );
        assert_eq!(
            serde_json::to_string(&PairSide::ShortSpread).unwrap(),
            "\"short_spread\""
        );
    }
}
