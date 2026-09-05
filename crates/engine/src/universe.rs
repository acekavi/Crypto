use std::collections::{HashMap, HashSet};

use botcore::{Instrument, Symbol};
use exchange::bybit::wire::Ticker;
use rust_decimal::Decimal;

/// Which symbols the bot is willing to trade.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UniverseFilter {
    pub size: usize,
    pub min_turnover_24h: Decimal,
    pub min_listing_age_days: i64,
}

impl UniverseFilter {
    /// The spec's defaults: top 20 by 24h turnover, at least 50M USDT turnover,
    /// listed at least 30 days.
    pub fn defaults() -> Self {
        UniverseFilter {
            size: 20,
            min_turnover_24h: Decimal::from(50_000_000),
            min_listing_age_days: 30,
        }
    }
}

/// Rank and filter the tradable universe.
///
/// `protected` holds symbols with an open position or a resting entry order.
/// Those are ALWAYS retained regardless of ranking or filters: dropping a
/// symbol the bot is still holding would stop its candles arriving, leaving the
/// engine unable to manage the exit. They are added on top of the ranked set,
/// so a protected symbol never consumes one of the `size` slots.
///
/// A ticker with no matching instrument is excluded — without tick size and
/// quantity step no valid order could be formed for it anyway.
pub fn select_universe(
    tickers: &[Ticker],
    instruments: &[Instrument],
    filter: &UniverseFilter,
    now_ms: i64,
    protected: &HashSet<Symbol>,
) -> Vec<Symbol> {
    let by_symbol: HashMap<&str, &Instrument> =
        instruments.iter().map(|i| (i.symbol.as_str(), i)).collect();

    let mut qualified: Vec<&Ticker> = tickers
        .iter()
        .filter(|t| {
            let Some(inst) = by_symbol.get(t.symbol.as_str()) else {
                return false;
            };
            t.turnover_24h >= filter.min_turnover_24h
                && inst.age_days(now_ms) >= filter.min_listing_age_days
        })
        .collect();

    // Descending turnover. Ties break on symbol name so the ordering is
    // deterministic across runs rather than dependent on the exchange's
    // response order.
    qualified.sort_by(|a, b| {
        b.turnover_24h
            .cmp(&a.turnover_24h)
            .then_with(|| a.symbol.as_str().cmp(b.symbol.as_str()))
    });

    let mut out: Vec<Symbol> = qualified
        .into_iter()
        .take(filter.size)
        .map(|t| t.symbol.clone())
        .collect();

    let already: HashSet<&str> = out.iter().map(|s| s.as_str()).collect();
    let mut extras: Vec<Symbol> = protected
        .iter()
        .filter(|s| !already.contains(s.as_str()))
        .cloned()
        .collect();
    // Deterministic order for the appended protected symbols too.
    extras.sort_by(|a, b| a.as_str().cmp(b.as_str()));
    out.extend(extras);

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use botcore::{Instrument, Symbol};
    use exchange::bybit::wire::Ticker;
    use rust_decimal_macros::dec;
    use std::collections::HashSet;

    const DAY: i64 = 86_400_000;
    const NOW: i64 = 1_700_000_000_000;

    fn ticker(sym: &str, turnover: Decimal) -> Ticker {
        Ticker {
            symbol: Symbol::new(sym),
            turnover_24h: turnover,
            last_price: dec!(100),
        }
    }

    fn instrument(sym: &str, age_days: i64) -> Instrument {
        Instrument {
            symbol: Symbol::new(sym),
            tick_size: dec!(0.1),
            qty_step: dec!(0.001),
            min_order_qty: dec!(0.001),
            min_notional: dec!(5),
            launch_time_ms: NOW - age_days * DAY,
        }
    }

    #[test]
    fn symbols_are_ranked_by_turnover_descending() {
        let tickers = vec![
            ticker("AAAUSDT", dec!(5_000_000)),
            ticker("BBBUSDT", dec!(9_000_000)),
            ticker("CCCUSDT", dec!(7_000_000)),
        ];
        let instruments = vec![
            instrument("AAAUSDT", 100),
            instrument("BBBUSDT", 100),
            instrument("CCCUSDT", 100),
        ];
        let f = UniverseFilter {
            size: 3,
            min_turnover_24h: dec!(1_000_000),
            min_listing_age_days: 30,
        };
        let out = select_universe(&tickers, &instruments, &f, NOW, &HashSet::new());
        assert_eq!(
            out.iter().map(|s| s.as_str()).collect::<Vec<_>>(),
            vec!["BBBUSDT", "CCCUSDT", "AAAUSDT"]
        );
    }

    #[test]
    fn the_result_is_truncated_to_the_configured_size() {
        let tickers = vec![
            ticker("AAAUSDT", dec!(5_000_000)),
            ticker("BBBUSDT", dec!(9_000_000)),
            ticker("CCCUSDT", dec!(7_000_000)),
        ];
        let instruments = vec![
            instrument("AAAUSDT", 100),
            instrument("BBBUSDT", 100),
            instrument("CCCUSDT", 100),
        ];
        let f = UniverseFilter {
            size: 2,
            min_turnover_24h: dec!(1_000_000),
            min_listing_age_days: 30,
        };
        let out = select_universe(&tickers, &instruments, &f, NOW, &HashSet::new());
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].as_str(), "BBBUSDT");
    }

    #[test]
    fn symbols_below_the_turnover_floor_are_excluded() {
        let tickers = vec![
            ticker("THINUSDT", dec!(100)),
            ticker("DEEPUSDT", dec!(9_000_000)),
        ];
        let instruments = vec![instrument("THINUSDT", 100), instrument("DEEPUSDT", 100)];
        let f = UniverseFilter {
            size: 10,
            min_turnover_24h: dec!(1_000_000),
            min_listing_age_days: 30,
        };
        let out = select_universe(&tickers, &instruments, &f, NOW, &HashSet::new());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].as_str(), "DEEPUSDT");
    }

    #[test]
    fn freshly_listed_symbols_are_excluded() {
        // A new listing has no price history for indicators and unstable
        // liquidity; a backtest over it would also be meaningless.
        let tickers = vec![
            ticker("NEWUSDT", dec!(9_000_000)),
            ticker("OLDUSDT", dec!(9_000_000)),
        ];
        let instruments = vec![instrument("NEWUSDT", 5), instrument("OLDUSDT", 100)];
        let f = UniverseFilter {
            size: 10,
            min_turnover_24h: dec!(1_000_000),
            min_listing_age_days: 30,
        };
        let out = select_universe(&tickers, &instruments, &f, NOW, &HashSet::new());
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].as_str(), "OLDUSDT");
    }

    #[test]
    fn a_ticker_with_no_matching_instrument_is_excluded() {
        // Without instrument metadata there is no tick size or qty step, so no
        // order could be correctly formed anyway.
        let tickers = vec![ticker("GHOSTUSDT", dec!(9_000_000))];
        let f = UniverseFilter {
            size: 10,
            min_turnover_24h: dec!(1_000_000),
            min_listing_age_days: 30,
        };
        let out = select_universe(&tickers, &[], &f, NOW, &HashSet::new());
        assert!(out.is_empty());
    }

    #[test]
    fn a_protected_symbol_is_retained_even_when_it_fails_every_filter() {
        // Holding a position in a symbol that just fell out of the ranking must
        // not orphan it — the engine still needs its candles to manage the exit.
        let tickers = vec![
            ticker("HELDUSDT", dec!(1)),
            ticker("DEEPUSDT", dec!(9_000_000)),
        ];
        let instruments = vec![instrument("HELDUSDT", 1), instrument("DEEPUSDT", 100)];
        let f = UniverseFilter {
            size: 1,
            min_turnover_24h: dec!(1_000_000),
            min_listing_age_days: 30,
        };
        let mut protected = HashSet::new();
        protected.insert(Symbol::new("HELDUSDT"));

        let out = select_universe(&tickers, &instruments, &f, NOW, &protected);
        let names: Vec<_> = out.iter().map(|s| s.as_str()).collect();
        assert!(
            names.contains(&"HELDUSDT"),
            "protected symbol was dropped: {names:?}"
        );
        assert!(names.contains(&"DEEPUSDT"));
    }

    #[test]
    fn a_protected_symbol_is_not_duplicated_when_it_also_qualifies() {
        let tickers = vec![ticker("DEEPUSDT", dec!(9_000_000))];
        let instruments = vec![instrument("DEEPUSDT", 100)];
        let f = UniverseFilter {
            size: 10,
            min_turnover_24h: dec!(1_000_000),
            min_listing_age_days: 30,
        };
        let mut protected = HashSet::new();
        protected.insert(Symbol::new("DEEPUSDT"));

        let out = select_universe(&tickers, &instruments, &f, NOW, &protected);
        assert_eq!(out.len(), 1, "symbol appeared twice: {out:?}");
    }

    #[test]
    fn defaults_match_the_spec() {
        let f = UniverseFilter::defaults();
        assert_eq!(f.size, 20);
        assert_eq!(f.min_turnover_24h, dec!(50_000_000));
        assert_eq!(f.min_listing_age_days, 30);
    }
}
