use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// Strategy timeframes. Only the two the baseline strategy needs exist;
/// adding more is a deliberate act, not an accident.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Timeframe {
    /// Finest execution timeframe. Carries a real cost: it triples the candle
    /// count against M15 and raises trade frequency, and the reversion study
    /// measured fees at 77% of that strategy's entire net loss. Worth it only
    /// where entry precision genuinely matters.
    M5,
    M15,
    H1,
    H4,
    D1,
}

impl Timeframe {
    /// Bybit V5 kline interval string, used in both REST params and WS topics.
    pub fn as_bybit_interval(self) -> &'static str {
        match self {
            Timeframe::H1 => "60",
            Timeframe::M5 => "5",
            Timeframe::M15 => "15",
            Timeframe::H4 => "240",
            Timeframe::D1 => "D",
        }
    }

    pub fn duration_ms(self) -> i64 {
        match self {
            Timeframe::H1 => 3_600_000,
            Timeframe::M5 => 300_000,
            Timeframe::M15 => 900_000,
            Timeframe::H4 => 14_400_000,
            Timeframe::D1 => 86_400_000,
        }
    }
}

/// A single closed candle. `open_time_ms` is the candle's start, in epoch
/// milliseconds, which is what Bybit keys klines by.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Candle {
    pub open_time_ms: i64,
    pub open: Decimal,
    pub high: Decimal,
    pub low: Decimal,
    pub close: Decimal,
    pub volume: Decimal,
    pub turnover: Decimal,
}

impl Candle {
    pub fn close_time_ms(&self, tf: Timeframe) -> i64 {
        self.open_time_ms + tf.duration_ms() - 1
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeframe_maps_to_bybit_interval_strings() {
        assert_eq!(Timeframe::H1.as_bybit_interval(), "60");
        assert_eq!(Timeframe::H4.as_bybit_interval(), "240");
    }

    #[test]
    fn timeframe_durations_are_in_milliseconds() {
        assert_eq!(Timeframe::H1.duration_ms(), 3_600_000);
        assert_eq!(Timeframe::H4.duration_ms(), 14_400_000);
    }

    #[test]
    fn candle_close_time_is_open_plus_duration_minus_one() {
        let c = Candle {
            open_time_ms: 1_700_000_000_000,
            open: Default::default(),
            high: Default::default(),
            low: Default::default(),
            close: Default::default(),
            volume: Default::default(),
            turnover: Default::default(),
        };
        assert_eq!(
            c.close_time_ms(Timeframe::H1),
            1_700_000_000_000 + 3_600_000 - 1
        );
    }
}
