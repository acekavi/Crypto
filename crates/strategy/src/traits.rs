use botcore::Timeframe;

use crate::signal::{MarketContext, Signal};

/// A pluggable trading strategy.
///
/// Implementations hold their own per-symbol indicator state and are fed one
/// closed candle at a time. The engine calls `on_candle_close` only for
/// timeframes the strategy declared, and only for candles Bybit marked
/// confirmed.
pub trait Strategy: Send {
    /// Which timeframes this strategy needs fed to it.
    fn timeframes(&self) -> &[Timeframe];

    /// How many candles of history are needed before signals are trustworthy.
    /// The engine warms indicators with this much history after a restart
    /// before accepting any signal.
    fn warmup_candles(&self) -> usize;

    /// Consume one closed candle. Returns a signal only on the candle that
    /// completes a setup; `None` on every other candle, including those that
    /// merely advance indicator state.
    fn on_candle_close(&mut self, ctx: &MarketContext) -> Option<Signal>;
}
