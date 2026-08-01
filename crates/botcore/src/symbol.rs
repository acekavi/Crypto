use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

/// An exchange symbol such as `BTCUSDT`. A newtype so a symbol can never be
/// confused with an arbitrary string in a function signature.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct Symbol(String);

impl Symbol {
    pub fn new(s: impl Into<String>) -> Self {
        Symbol(s.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for Symbol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Exchange-imposed trading constraints for one instrument. Every order must
/// be validated against these before it is sent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Instrument {
    pub symbol: Symbol,
    pub tick_size: Decimal,
    pub qty_step: Decimal,
    pub min_order_qty: Decimal,
    pub launch_time_ms: i64,
}

impl Instrument {
    pub fn age_days(&self, now_ms: i64) -> i64 {
        (now_ms - self.launch_time_ms) / 86_400_000
    }

    pub fn qty_is_valid(&self, qty: Decimal) -> bool {
        qty >= self.min_order_qty
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn instrument() -> Instrument {
        Instrument {
            symbol: Symbol::new("BTCUSDT"),
            tick_size: dec!(0.1),
            qty_step: dec!(0.001),
            min_order_qty: dec!(0.001),
            launch_time_ms: 1_600_000_000_000,
        }
    }

    #[test]
    fn age_days_computes_from_launch_time() {
        let i = instrument();
        let now = 1_600_000_000_000 + 30 * 86_400_000;
        assert_eq!(i.age_days(now), 30);
    }

    #[test]
    fn qty_below_minimum_is_rejected() {
        let i = instrument();
        assert!(!i.qty_is_valid(dec!(0.0005)));
        assert!(i.qty_is_valid(dec!(0.001)));
    }
}
