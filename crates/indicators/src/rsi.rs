use rust_decimal::Decimal;

/// Wilder's RSI. Seeded with a simple average of the first `period` deltas,
/// then smoothed with Wilder's recurrence in O(1).
#[derive(Debug, Clone)]
pub struct Rsi {
    period: usize,
    period_dec: Decimal,
    prev_price: Option<Decimal>,
    seed_gain: Decimal,
    seed_loss: Decimal,
    seed_count: usize,
    avg_gain: Option<Decimal>,
    avg_loss: Option<Decimal>,
    current: Option<Decimal>,
}

impl Rsi {
    pub fn new(period: usize) -> Self {
        assert!(period > 0, "RSI period must be positive");
        Rsi {
            period,
            period_dec: Decimal::from(period as u64),
            prev_price: None,
            seed_gain: Decimal::ZERO,
            seed_loss: Decimal::ZERO,
            seed_count: 0,
            avg_gain: None,
            avg_loss: None,
            current: None,
        }
    }

    pub fn update(&mut self, price: Decimal) -> Option<Decimal> {
        let prev = self.prev_price.replace(price)?;

        let delta = price - prev;
        let gain = if delta > Decimal::ZERO {
            delta
        } else {
            Decimal::ZERO
        };
        let loss = if delta < Decimal::ZERO {
            -delta
        } else {
            Decimal::ZERO
        };

        match (self.avg_gain, self.avg_loss) {
            (Some(ag), Some(al)) => {
                let n1 = self.period_dec - Decimal::ONE;
                self.avg_gain = Some((ag * n1 + gain) / self.period_dec);
                self.avg_loss = Some((al * n1 + loss) / self.period_dec);
            }
            _ => {
                self.seed_gain += gain;
                self.seed_loss += loss;
                self.seed_count += 1;
                if self.seed_count == self.period {
                    self.avg_gain = Some(self.seed_gain / self.period_dec);
                    self.avg_loss = Some(self.seed_loss / self.period_dec);
                } else {
                    return None;
                }
            }
        }

        self.current = Some(self.compute());
        self.current
    }

    fn compute(&self) -> Decimal {
        let (ag, al) = (self.avg_gain.unwrap(), self.avg_loss.unwrap());
        if al.is_zero() {
            // No losses in the window: RSI is defined as 100.
            return Decimal::from(100);
        }
        if ag.is_zero() {
            return Decimal::ZERO;
        }
        let rs = ag / al;
        Decimal::from(100) - (Decimal::from(100) / (Decimal::ONE + rs))
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
    fn rsi_is_none_until_period_plus_one_samples() {
        let mut rsi = Rsi::new(3);
        assert_eq!(rsi.update(dec!(10)), None); // no delta yet
        assert_eq!(rsi.update(dec!(11)), None); // 1 delta
        assert_eq!(rsi.update(dec!(12)), None); // 2 deltas
        assert!(rsi.update(dec!(13)).is_some()); // 3 deltas -> warm
    }

    #[test]
    fn rsi_is_one_hundred_when_every_move_is_up() {
        let mut rsi = Rsi::new(3);
        for p in [10, 11, 12, 13] {
            rsi.update(Decimal::from(p));
        }
        assert_eq!(rsi.value(), Some(dec!(100)));
    }

    #[test]
    fn rsi_is_zero_when_every_move_is_down() {
        let mut rsi = Rsi::new(3);
        for p in [13, 12, 11, 10] {
            rsi.update(Decimal::from(p));
        }
        assert_eq!(rsi.value(), Some(dec!(0)));
    }

    #[test]
    fn rsi_is_fifty_when_gains_equal_losses() {
        // Seed with alternating +1/-1 moves of equal magnitude.
        let mut rsi = Rsi::new(2);
        rsi.update(dec!(10));
        rsi.update(dec!(11)); // +1
        rsi.update(dec!(10)); // -1
        assert_eq!(rsi.value(), Some(dec!(50)));
    }
}
