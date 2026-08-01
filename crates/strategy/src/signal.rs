use botcore::{Candle, Instrument, Side, Symbol, Timeframe};
use rust_decimal::Decimal;

/// A strategy's decision to enter, expressed in prices only.
///
/// There is deliberately no quantity field. The strategy never sees account
/// equity; `RiskManager` is the only component that computes size. That
/// separation keeps sizing bugs out of strategy code and lets any strategy be
/// replayed against any equity curve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Signal {
    pub symbol: Symbol,
    pub side: Side,
    /// The limit price the entry will rest at.
    pub entry_price: Decimal,
    pub stop_price: Decimal,
    pub target_price: Decimal,
    /// ATR at the signal candle's close. Carried because execution derives the
    /// stop-limit offset and the escalation ladder's widths from it — those are
    /// fractions of ATR, and recomputing ATR outside the strategy would mean a
    /// second implementation free to drift from this one.
    pub atr: Decimal,
    /// Open time of the candle that produced this signal. Feeds the
    /// deterministic `orderLinkId`, so a retry cannot open a second position.
    pub signal_candle_open_ms: i64,
}

impl Signal {
    /// "R" — the distance between the entry limit and the stop.
    ///
    /// Always non-negative: a short's stop sits above its entry, so a raw
    /// subtraction would invert every downstream size calculation.
    pub fn risk_distance(&self) -> Decimal {
        (self.entry_price - self.stop_price).abs()
    }

    /// How many R the target sits away from entry. `None` when the stop
    /// distance is zero, which callers must treat as an invalid signal
    /// rather than dividing by it.
    pub fn reward_multiple(&self) -> Option<Decimal> {
        let r = self.risk_distance();
        if r.is_zero() {
            return None;
        }
        Some((self.target_price - self.entry_price).abs() / r)
    }
}

/// Everything a strategy sees about one closed candle.
///
/// Deliberately one candle, not a slice: strategies hold incremental
/// indicator state, so evaluation is O(1) per candle. Passing history would
/// make a backtest over years of data quadratic.
#[derive(Debug)]
pub struct MarketContext<'a> {
    pub symbol: &'a Symbol,
    pub timeframe: Timeframe,
    pub candle: &'a Candle,
    pub instrument: &'a Instrument,
}

#[cfg(test)]
mod tests {
    use super::*;
    use botcore::Symbol;
    use rust_decimal_macros::dec;

    #[test]
    fn risk_distance_is_absolute_for_a_long() {
        let s = Signal {
            symbol: Symbol::new("BTCUSDT"),
            side: Side::Buy,
            entry_price: dec!(100),
            stop_price: dec!(95),
            target_price: dec!(110),
            atr: dec!(2),
            signal_candle_open_ms: 0,
        };
        assert_eq!(s.risk_distance(), dec!(5));
    }

    #[test]
    fn risk_distance_is_absolute_for_a_short() {
        // A short's stop sits ABOVE entry, so a naive entry - stop would be
        // negative and every downstream size calculation would invert.
        let s = Signal {
            symbol: Symbol::new("BTCUSDT"),
            side: Side::Sell,
            entry_price: dec!(100),
            stop_price: dec!(105),
            target_price: dec!(90),
            atr: dec!(2),
            signal_candle_open_ms: 0,
        };
        assert_eq!(s.risk_distance(), dec!(5));
    }

    #[test]
    fn reward_multiple_is_two_for_a_correctly_built_long() {
        let s = Signal {
            symbol: Symbol::new("BTCUSDT"),
            side: Side::Buy,
            entry_price: dec!(100),
            stop_price: dec!(95),
            target_price: dec!(110),
            atr: dec!(2),
            signal_candle_open_ms: 0,
        };
        assert_eq!(s.reward_multiple(), Some(dec!(2)));
    }

    #[test]
    fn reward_multiple_is_two_for_a_correctly_built_short() {
        let s = Signal {
            symbol: Symbol::new("BTCUSDT"),
            side: Side::Sell,
            entry_price: dec!(100),
            stop_price: dec!(105),
            target_price: dec!(90),
            atr: dec!(2),
            signal_candle_open_ms: 0,
        };
        assert_eq!(s.reward_multiple(), Some(dec!(2)));
    }

    #[test]
    fn reward_multiple_is_none_when_stop_equals_entry() {
        // A zero-distance stop would divide by zero downstream; callers must
        // be able to detect it rather than panic.
        let s = Signal {
            symbol: Symbol::new("BTCUSDT"),
            side: Side::Buy,
            entry_price: dec!(100),
            stop_price: dec!(100),
            target_price: dec!(110),
            atr: dec!(2),
            signal_candle_open_ms: 0,
        };
        assert_eq!(s.reward_multiple(), None);
    }
}
