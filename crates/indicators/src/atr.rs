use botcore::Candle;
use rust_decimal::Decimal;

/// Wilder's Average True Range, seeded with a simple average of the first
/// `period` true ranges then smoothed in O(1).
#[derive(Debug, Clone)]
pub struct Atr {
    period: usize,
    period_dec: Decimal,
    prev_close: Option<Decimal>,
    seed_sum: Decimal,
    seed_count: usize,
    current: Option<Decimal>,
}

impl Atr {
    pub fn new(period: usize) -> Self {
        assert!(period > 0, "ATR period must be positive");
        Atr {
            period,
            period_dec: Decimal::from(period as u64),
            prev_close: None,
            seed_sum: Decimal::ZERO,
            seed_count: 0,
            current: None,
        }
    }

    pub fn update(&mut self, candle: &Candle) -> Option<Decimal> {
        let tr = self.true_range(candle);
        self.prev_close = Some(candle.close);

        match self.current {
            Some(prev) => {
                let n1 = self.period_dec - Decimal::ONE;
                self.current = Some((prev * n1 + tr) / self.period_dec);
            }
            None => {
                self.seed_sum += tr;
                self.seed_count += 1;
                if self.seed_count == self.period {
                    self.current = Some(self.seed_sum / self.period_dec);
                }
            }
        }
        self.current
    }

    /// True range: the widest of the intrabar range and the two gap measures
    /// against the previous close.
    fn true_range(&self, candle: &Candle) -> Decimal {
        let range = candle.high - candle.low;
        match self.prev_close {
            None => range,
            Some(pc) => range.max((candle.high - pc).abs()).max((candle.low - pc).abs()),
        }
    }

    pub fn value(&self) -> Option<Decimal> {
        self.current
    }

    pub fn is_warm(&self) -> bool {
        self.current.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn candle(high: Decimal, low: Decimal, close: Decimal) -> Candle {
        Candle {
            open_time_ms: 0,
            open: close,
            high,
            low,
            close,
            volume: Decimal::ZERO,
            turnover: Decimal::ZERO,
        }
    }

    #[test]
    fn first_true_range_is_high_minus_low() {
        let mut atr = Atr::new(1);
        // With period 1 the seed completes on the first candle.
        assert_eq!(atr.update(&candle(dec!(10), dec!(8), dec!(9))), Some(dec!(2)));
    }

    #[test]
    fn true_range_accounts_for_gaps_against_previous_close() {
        let mut atr = Atr::new(1);
        atr.update(&candle(dec!(10), dec!(8), dec!(9)));
        // Gap up: high 20, low 19, prev close 9. TR = max(1, 11, 10) = 11.
        // period 1 -> ATR tracks TR exactly.
        assert_eq!(atr.update(&candle(dec!(20), dec!(19), dec!(19))), Some(dec!(11)));
    }

    #[test]
    fn atr_returns_none_until_warm() {
        let mut atr = Atr::new(3);
        assert_eq!(atr.update(&candle(dec!(10), dec!(9), dec!(9))), None);
        assert_eq!(atr.update(&candle(dec!(11), dec!(10), dec!(10))), None);
        assert!(atr.update(&candle(dec!(12), dec!(11), dec!(11))).is_some());
    }

    #[test]
    fn atr_averages_constant_ranges_to_that_range() {
        let mut atr = Atr::new(3);
        for _ in 0..6 {
            atr.update(&candle(dec!(10), dec!(9), dec!(9)));
        }
        // Every TR is exactly 1, so the average must be 1.
        assert_eq!(atr.value(), Some(dec!(1)));
    }
}
