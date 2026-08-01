use rust_decimal::Decimal;

/// Exponential moving average, seeded with a simple average of the first
/// `period` samples, then updated in O(1).
#[derive(Debug, Clone)]
pub struct Ema {
    period: usize,
    alpha: Decimal,
    seed_sum: Decimal,
    seed_count: usize,
    current: Option<Decimal>,
}

impl Ema {
    pub fn new(period: usize) -> Self {
        assert!(period > 0, "EMA period must be positive");
        let alpha = Decimal::from(2) / Decimal::from(period as u64 + 1);
        Ema {
            period,
            alpha,
            seed_sum: Decimal::ZERO,
            seed_count: 0,
            current: None,
        }
    }

    /// Feed one sample. Returns the new EMA once warm, `None` before that.
    pub fn update(&mut self, price: Decimal) -> Option<Decimal> {
        match self.current {
            Some(prev) => {
                let next = self.alpha * price + (Decimal::ONE - self.alpha) * prev;
                self.current = Some(next);
            }
            None => {
                self.seed_sum += price;
                self.seed_count += 1;
                if self.seed_count == self.period {
                    self.current = Some(self.seed_sum / Decimal::from(self.period as u64));
                }
            }
        }
        self.current
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

    #[test]
    fn ema_returns_none_until_warm() {
        let mut ema = Ema::new(3);
        assert_eq!(ema.update(dec!(1)), None);
        assert_eq!(ema.update(dec!(2)), None);
        // Third value completes the seed SMA.
        assert_eq!(ema.update(dec!(3)), Some(dec!(2)));
    }

    #[test]
    fn ema_seeds_with_sma_then_applies_smoothing() {
        // period 3 -> alpha = 2/(3+1) = 0.5
        // seed SMA of [1,2,3] = 2; next price 10 -> 0.5*10 + 0.5*2 = 6
        let mut ema = Ema::new(3);
        ema.update(dec!(1));
        ema.update(dec!(2));
        ema.update(dec!(3));
        assert_eq!(ema.update(dec!(10)), Some(dec!(6)));
    }

    #[test]
    fn ema_value_matches_last_update() {
        let mut ema = Ema::new(2);
        ema.update(dec!(4));
        let last = ema.update(dec!(6));
        assert_eq!(ema.value(), last);
    }
}
