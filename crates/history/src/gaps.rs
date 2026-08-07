use botcore::{Candle, Timeframe};

/// A hole in a stored candle series: candles are missing strictly between
/// `from_ms` (the last present open time) and `to_ms` (the next present one).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Gap {
    pub from_ms: i64,
    pub to_ms: i64,
}

/// Walk consecutive pairs and report every step that is not exactly one
/// candle wide.
///
/// Strict inequality rather than `>` because spacing narrower than
/// `tf.duration_ms()` is just as much evidence the series was not fetched at
/// the timeframe it claims as spacing wider than it — both mean a candle
/// that belongs in this series, at this cadence, was not stored.
///
/// Assumes `candles` is already ascending by `open_time_ms`, which is the
/// contract `HistoryDb::candles_in_range` guarantees — this does not sort.
pub fn find_gaps(candles: &[Candle], tf: Timeframe) -> Vec<Gap> {
    let step = tf.duration_ms();
    candles
        .windows(2)
        .filter_map(|pair| {
            let (prev, next) = (&pair[0], &pair[1]);
            if next.open_time_ms - prev.open_time_ms != step {
                Some(Gap {
                    from_ms: prev.open_time_ms,
                    to_ms: next.open_time_ms,
                })
            } else {
                None
            }
        })
        .collect()
}
