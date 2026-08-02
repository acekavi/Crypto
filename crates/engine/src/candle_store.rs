use std::collections::HashMap;
use std::collections::HashSet;
use std::collections::VecDeque;

use botcore::{Candle, Symbol, Timeframe};

/// What the store did with an offered candle.
///
/// Every variant except `Accepted` means the stream did NOT advance — the
/// caller must not feed the candle to a strategy, and in the `Gap` case must
/// backfill before resuming.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Acceptance {
    Accepted,
    /// Already seen. Bybit redelivers confirmed candles after a reconnect.
    Duplicate,
    /// Older than the last candle seen. Never rewind the stream.
    OutOfOrder,
    /// `missing` candles are absent between the last one and this one.
    Gap {
        missing: i64,
    },
}

#[derive(Debug, Default)]
struct Stream {
    /// Bounded to the warmup length, newest last.
    window: VecDeque<Candle>,
    last_open_ms: Option<i64>,
}

/// Per-(symbol, timeframe) candle bookkeeping.
///
/// Answers three questions the engine cannot trade without: have we seen
/// enough history to trust an indicator, is this candle the next one, and has
/// this stream gone quiet.
#[derive(Debug)]
pub struct CandleStore {
    warmup_candles: usize,
    streams: HashMap<(Symbol, Timeframe), Stream>,
}

impl CandleStore {
    pub fn new(warmup_candles: usize) -> Self {
        CandleStore {
            warmup_candles,
            streams: HashMap::new(),
        }
    }

    /// Seed a stream with historical candles after a restart or a gap backfill.
    ///
    /// Candles are sorted by open time before insertion, so a caller handing
    /// over a newest-first REST response cannot silently reverse the series.
    pub fn warm(&mut self, symbol: &Symbol, tf: Timeframe, mut candles: Vec<Candle>) {
        candles.sort_by_key(|c| c.open_time_ms);
        let warmup = self.warmup_candles;
        let stream = self.streams.entry((symbol.clone(), tf)).or_default();
        stream.window.clear();
        stream.last_open_ms = candles.last().map(|c| c.open_time_ms);
        for c in candles {
            stream.window.push_back(c);
        }
        while stream.window.len() > warmup {
            stream.window.pop_front();
        }
    }

    /// Offer the next candle. The stream advances only on `Accepted`.
    pub fn accept(&mut self, symbol: &Symbol, tf: Timeframe, candle: &Candle) -> Acceptance {
        let warmup = self.warmup_candles;
        let stream = self.streams.entry((symbol.clone(), tf)).or_default();

        if let Some(last) = stream.last_open_ms {
            let step = tf.duration_ms();
            let delta = candle.open_time_ms - last;
            if delta == 0 {
                return Acceptance::Duplicate;
            }
            if delta < 0 {
                return Acceptance::OutOfOrder;
            }
            if delta > step {
                return Acceptance::Gap {
                    missing: delta / step - 1,
                };
            }
        }

        stream.last_open_ms = Some(candle.open_time_ms);
        stream.window.push_back(candle.clone());
        while stream.window.len() > warmup {
            stream.window.pop_front();
        }
        Acceptance::Accepted
    }

    pub fn last_open_ms(&self, symbol: &Symbol, tf: Timeframe) -> Option<i64> {
        self.streams
            .get(&(symbol.clone(), tf))
            .and_then(|s| s.last_open_ms)
    }

    /// How many candles the bounded window currently holds.
    pub fn window_len(&self, symbol: &Symbol, tf: Timeframe) -> usize {
        self.streams
            .get(&(symbol.clone(), tf))
            .map(|s| s.window.len())
            .unwrap_or(0)
    }

    /// Whether enough history has been seen to trust an indicator built from it.
    pub fn is_warm(&self, symbol: &Symbol, tf: Timeframe) -> bool {
        self.window_len(symbol, tf) >= self.warmup_candles
    }

    /// Whether the stream has gone quiet.
    ///
    /// A stream that has never produced a candle counts as stale: never having
    /// heard from a subscribed symbol is not a healthy state, and trading it
    /// would mean acting with no market data at all.
    pub fn is_stale(&self, symbol: &Symbol, tf: Timeframe, now_ms: i64) -> bool {
        match self.last_open_ms(symbol, tf) {
            None => true,
            Some(last) => now_ms - last >= 2 * tf.duration_ms(),
        }
    }

    /// Drop streams for symbols no longer being tracked.
    ///
    /// The universe re-ranks daily and this process runs for months, so without
    /// this the map accumulates a permanent entry for every symbol that ever
    /// entered the ranking. Callers pass the current universe; everything else
    /// is forgotten.
    ///
    /// Returns the number of streams removed, so a caller can log rotation
    /// rather than have memory quietly change size.
    pub fn retain_symbols(&mut self, keep: &HashSet<Symbol>) -> usize {
        let before = self.streams.len();
        self.streams.retain(|(symbol, _), _| keep.contains(symbol));
        before - self.streams.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use botcore::{Candle, Symbol, Timeframe};
    use rust_decimal::Decimal;

    const H1: i64 = 3_600_000;

    fn candle(open_time_ms: i64) -> Candle {
        Candle {
            open_time_ms,
            open: Decimal::from(100),
            high: Decimal::from(101),
            low: Decimal::from(99),
            close: Decimal::from(100),
            volume: Decimal::ZERO,
            turnover: Decimal::ZERO,
        }
    }

    fn btc() -> Symbol {
        Symbol::new("BTCUSDT")
    }

    #[test]
    fn a_fresh_store_is_cold_and_has_no_last_candle() {
        let s = CandleStore::new(10);
        assert!(!s.is_warm(&btc(), Timeframe::H1));
        assert_eq!(s.last_open_ms(&btc(), Timeframe::H1), None);
    }

    #[test]
    fn warming_with_enough_history_makes_the_stream_warm() {
        let mut s = CandleStore::new(10);
        let history: Vec<Candle> = (0..10).map(|i| candle(i * H1)).collect();
        s.warm(&btc(), Timeframe::H1, history);
        assert!(s.is_warm(&btc(), Timeframe::H1));
        assert_eq!(s.last_open_ms(&btc(), Timeframe::H1), Some(9 * H1));
    }

    #[test]
    fn warming_with_insufficient_history_leaves_the_stream_cold() {
        // Acting on a half-warm indicator is how a restart produces a bad
        // trade; the store must be able to say "not yet".
        let mut s = CandleStore::new(10);
        let history: Vec<Candle> = (0..9).map(|i| candle(i * H1)).collect();
        s.warm(&btc(), Timeframe::H1, history);
        assert!(!s.is_warm(&btc(), Timeframe::H1));
    }

    #[test]
    fn warming_sorts_a_newest_first_response() {
        // Bybit returns klines newest-first; handing that straight over must
        // not reverse the series.
        let mut s = CandleStore::new(3);
        s.warm(
            &btc(),
            Timeframe::H1,
            vec![candle(2 * H1), candle(0), candle(H1)],
        );
        assert_eq!(s.last_open_ms(&btc(), Timeframe::H1), Some(2 * H1));
    }

    #[test]
    fn a_consecutive_candle_is_accepted() {
        let mut s = CandleStore::new(2);
        s.warm(&btc(), Timeframe::H1, vec![candle(0), candle(H1)]);
        assert_eq!(
            s.accept(&btc(), Timeframe::H1, &candle(2 * H1)),
            Acceptance::Accepted
        );
        assert_eq!(s.last_open_ms(&btc(), Timeframe::H1), Some(2 * H1));
    }

    #[test]
    fn a_repeated_candle_is_a_duplicate_and_does_not_advance_the_stream() {
        // Bybit can redeliver a confirmed candle after a reconnect. Processing
        // it twice would feed the same bar into indicators twice.
        let mut s = CandleStore::new(1);
        s.warm(&btc(), Timeframe::H1, vec![candle(0)]);
        assert_eq!(
            s.accept(&btc(), Timeframe::H1, &candle(0)),
            Acceptance::Duplicate
        );
        assert_eq!(s.last_open_ms(&btc(), Timeframe::H1), Some(0));
    }

    #[test]
    fn an_older_candle_is_out_of_order_and_does_not_rewind_the_stream() {
        let mut s = CandleStore::new(1);
        s.warm(&btc(), Timeframe::H1, vec![candle(5 * H1)]);
        assert_eq!(
            s.accept(&btc(), Timeframe::H1, &candle(3 * H1)),
            Acceptance::OutOfOrder
        );
        assert_eq!(s.last_open_ms(&btc(), Timeframe::H1), Some(5 * H1));
    }

    #[test]
    fn a_skipped_candle_is_reported_as_a_gap_and_does_not_advance_the_stream() {
        // The engine must backfill before resuming; advancing here would make
        // the hole permanently undetectable.
        let mut s = CandleStore::new(1);
        s.warm(&btc(), Timeframe::H1, vec![candle(0)]);
        assert_eq!(
            s.accept(&btc(), Timeframe::H1, &candle(2 * H1)),
            Acceptance::Gap { missing: 1 }
        );
        assert_eq!(s.last_open_ms(&btc(), Timeframe::H1), Some(0));
    }

    #[test]
    fn the_first_candle_of_a_cold_stream_is_accepted_without_a_gap() {
        let mut s = CandleStore::new(1);
        assert_eq!(
            s.accept(&btc(), Timeframe::H1, &candle(9 * H1)),
            Acceptance::Accepted
        );
    }

    #[test]
    fn a_stream_is_stale_after_twice_its_timeframe() {
        let mut s = CandleStore::new(1);
        s.warm(&btc(), Timeframe::H1, vec![candle(0)]);
        // The candle opened at 0 and covers up to H1. Two timeframes past its
        // open is the threshold.
        assert!(!s.is_stale(&btc(), Timeframe::H1, 2 * H1 - 1));
        assert!(s.is_stale(&btc(), Timeframe::H1, 2 * H1));
    }

    #[test]
    fn a_stream_that_has_never_produced_a_candle_is_stale() {
        // Never having heard from a subscribed symbol is not a healthy state.
        let s = CandleStore::new(1);
        assert!(s.is_stale(&btc(), Timeframe::H1, 1_700_000_000_000));
    }

    #[test]
    fn streams_are_tracked_per_symbol_and_per_timeframe() {
        let mut s = CandleStore::new(1);
        s.warm(&btc(), Timeframe::H1, vec![candle(0)]);
        assert_eq!(s.last_open_ms(&btc(), Timeframe::H4), None);
        assert_eq!(s.last_open_ms(&Symbol::new("ETHUSDT"), Timeframe::H1), None);
    }

    #[test]
    fn the_window_is_bounded_to_the_warmup_length() {
        // Unbounded growth over a 24/7 run is a slow leak.
        let mut s = CandleStore::new(3);
        for i in 0..100 {
            s.accept(&btc(), Timeframe::H1, &candle(i * H1));
        }
        assert_eq!(s.window_len(&btc(), Timeframe::H1), 3);
    }

    #[test]
    fn pruning_leaves_a_kept_symbols_stream_untouched() {
        let mut s = CandleStore::new(1);
        s.warm(&btc(), Timeframe::H1, vec![candle(0)]);
        let mut keep = HashSet::new();
        keep.insert(btc());
        s.retain_symbols(&keep);
        assert_eq!(s.last_open_ms(&btc(), Timeframe::H1), Some(0));
    }

    #[test]
    fn pruning_drops_a_symbol_absent_from_keep() {
        let mut s = CandleStore::new(1);
        s.warm(&btc(), Timeframe::H1, vec![candle(0)]);
        s.retain_symbols(&HashSet::new());
        assert_eq!(s.last_open_ms(&btc(), Timeframe::H1), None);
    }

    #[test]
    fn pruning_a_symbol_drops_both_of_its_timeframes() {
        // The key is (symbol, timeframe); a naive filter keyed only on the
        // symbol half of the tuple could leave the other timeframe behind.
        let mut s = CandleStore::new(1);
        s.warm(&btc(), Timeframe::H1, vec![candle(0)]);
        s.warm(&btc(), Timeframe::H4, vec![candle(0)]);
        s.retain_symbols(&HashSet::new());
        assert_eq!(s.last_open_ms(&btc(), Timeframe::H1), None);
        assert_eq!(s.last_open_ms(&btc(), Timeframe::H4), None);
    }

    #[test]
    fn pruning_returns_the_count_of_streams_actually_removed() {
        let mut s = CandleStore::new(1);
        s.warm(&btc(), Timeframe::H1, vec![candle(0)]);
        s.warm(&btc(), Timeframe::H4, vec![candle(0)]);
        s.warm(&Symbol::new("ETHUSDT"), Timeframe::H1, vec![candle(0)]);
        let mut keep = HashSet::new();
        keep.insert(Symbol::new("ETHUSDT"));
        assert_eq!(s.retain_symbols(&keep), 2);
    }

    #[test]
    fn pruning_with_an_empty_keep_set_removes_everything() {
        let mut s = CandleStore::new(1);
        s.warm(&btc(), Timeframe::H1, vec![candle(0)]);
        s.warm(&Symbol::new("ETHUSDT"), Timeframe::H4, vec![candle(0)]);
        assert_eq!(s.retain_symbols(&HashSet::new()), 2);
    }

    #[test]
    fn pruning_a_store_with_nothing_to_remove_returns_zero_and_changes_nothing() {
        let mut s = CandleStore::new(1);
        s.warm(&btc(), Timeframe::H1, vec![candle(0)]);
        let mut keep = HashSet::new();
        keep.insert(btc());
        assert_eq!(s.retain_symbols(&keep), 0);
        assert_eq!(s.last_open_ms(&btc(), Timeframe::H1), Some(0));
    }
}
