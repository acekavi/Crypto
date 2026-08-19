//! Trade the pullback continuation after a scheduled high-impact US news
//! event — FOMC only; see `docs/superpowers/specs/2026-08-19-fomc-news-reaction-design.md`
//! for why CPI/NFP are excluded and where the FOMC calendar came from.
//!
//! Not a straddle: neither the simulator nor the real Bybit client can
//! correctly represent a conditional/trigger entry (see the level-reaction
//! optimization doc's "momentum-confirmation entry" section). Instead:
//!
//! ```text
//!   Waiting ──event time arrives──▶ InWindow ──window closes──▶ Armed
//!                                  (no trades,                    │
//!                                   range recorded)          range breaks
//!                                                                  │
//!                                                                  ▼
//!                                                               Broken ──price
//!                                                              retests──▶ Retested
//!                                                                            │
//!                                                          reaction candle decides
//! ```
//!
//! Break, retest and reaction are the identical construction `level_reaction`
//! uses — same fields, same entry/stop/target shape — just armed by a
//! scheduled timestamp instead of a swing-fractal break.

use std::collections::HashMap;

use botcore::{Side, Symbol, Timeframe};
use indicators::Atr;
use rust_decimal::Decimal;

use crate::level_reaction::{ReactMode, StopSource};
use crate::signal::{MarketContext, Signal};
use crate::traits::Strategy;

/// FOMC announcement timestamps (UTC ms), 2:00pm ET converted via
/// `America/New_York` (correctly split across EST/EDT).
///
/// Source: federalreserve.gov/monetarypolicy/fomccalendars.htm, fetched
/// 2026-08-19. Sanity-checked: 27/28 fetched dates fall on a Wednesday (the
/// one exception, 2024-11-07, is real — that meeting moved to Thursday
/// because the Tuesday was Election Day); gaps run 41-56 days, consistent
/// with the FOMC's ~6-8-week cadence. These 24 are the ones falling inside
/// this project's price-data window (2023-08-04 .. 2026-08-08); four earlier
/// fetched dates predate it and are omitted.
pub const FOMC_ANNOUNCEMENTS_UTC_MS: [i64; 24] = [
    1_695_232_800_000, // 2023-09-20 18:00 UTC
    1_698_861_600_000, // 2023-11-01 18:00 UTC
    1_702_494_000_000, // 2023-12-13 19:00 UTC
    1_706_727_600_000, // 2024-01-31 19:00 UTC
    1_710_957_600_000, // 2024-03-20 18:00 UTC
    1_714_586_400_000, // 2024-05-01 18:00 UTC
    1_718_215_200_000, // 2024-06-12 18:00 UTC
    1_722_448_800_000, // 2024-07-31 18:00 UTC
    1_726_682_400_000, // 2024-09-18 18:00 UTC
    1_731_006_000_000, // 2024-11-07 19:00 UTC
    1_734_548_400_000, // 2024-12-18 19:00 UTC
    1_738_177_200_000, // 2025-01-29 19:00 UTC
    1_742_407_200_000, // 2025-03-19 18:00 UTC
    1_746_640_800_000, // 2025-05-07 18:00 UTC
    1_750_269_600_000, // 2025-06-18 18:00 UTC
    1_753_898_400_000, // 2025-07-30 18:00 UTC
    1_758_132_000_000, // 2025-09-17 18:00 UTC
    1_761_760_800_000, // 2025-10-29 18:00 UTC
    1_765_393_200_000, // 2025-12-10 19:00 UTC
    1_769_626_800_000, // 2026-01-28 19:00 UTC
    1_773_856_800_000, // 2026-03-18 18:00 UTC
    1_777_485_600_000, // 2026-04-29 18:00 UTC
    1_781_719_200_000, // 2026-06-17 18:00 UTC
    1_785_348_000_000, // 2026-07-29 18:00 UTC
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dir {
    Up,
    Down,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewsReactionParams {
    /// Announcement timestamps to react to. A field, not hard-coded to
    /// `FOMC_ANNOUNCEMENTS_UTC_MS`, so a test can supply a synthetic
    /// calendar without touching the real one.
    pub event_times_ms: Vec<i64>,
    pub execution_tf: Timeframe,
    pub atr_period: usize,
    /// Execution candles after the announcement during which only the range
    /// is recorded — no entries.
    pub window_candles: usize,
    /// Execution candles after the window closes within which a break still
    /// counts as reacting to this event.
    pub active_window_candles: usize,
    pub min_break_atr: Decimal,
    pub retest_window: usize,
    pub react_mode: ReactMode,
    pub entry_fraction: Decimal,
    pub stop_source: StopSource,
    pub stop_buffer_atr: Decimal,
    pub reward_multiple: Decimal,
    pub breakeven_at_r: Option<Decimal>,
    pub allow_long: bool,
    pub allow_short: bool,
}

impl NewsReactionParams {
    pub fn fomc_default() -> Self {
        NewsReactionParams {
            event_times_ms: FOMC_ANNOUNCEMENTS_UTC_MS.to_vec(),
            execution_tf: Timeframe::M5,
            atr_period: 14,
            window_candles: 12,        // 1 hour on M5
            active_window_candles: 48, // 4 hours to break, once the window closes
            min_break_atr: Decimal::new(25, 2),
            retest_window: 24,
            react_mode: ReactMode::Reversal,
            entry_fraction: Decimal::new(75, 2),
            stop_source: StopSource::TouchAndReaction,
            stop_buffer_atr: Decimal::new(5, 1),
            reward_multiple: Decimal::from(5),
            breakeven_at_r: Some(Decimal::TWO),
            allow_long: true,
            allow_short: true,
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Phase {
    /// Not currently inside any event's window or active period.
    Idle,
    /// Inside an event's news window: recording the range, taking no trades.
    InWindow {
        high: Decimal,
        low: Decimal,
        bars_left: usize,
    },
    /// Window closed; watching for a break within `active_window_candles`.
    Armed {
        high: Decimal,
        low: Decimal,
        bars_left: usize,
    },
    Broken {
        level: Decimal,
        dir: Dir,
        bars: usize,
    },
    Retested {
        level: Decimal,
        dir: Dir,
        touch_high: Decimal,
        touch_low: Decimal,
    },
}

struct SymbolState {
    atr: Atr,
    phase: Phase,
    /// Index into `event_times_ms` of the next event not yet reached.
    next_event: usize,
}

impl SymbolState {
    fn new(p: &NewsReactionParams) -> Self {
        SymbolState {
            atr: Atr::new(p.atr_period),
            phase: Phase::Idle,
            next_event: 0,
        }
    }
}

pub struct NewsReactionStrategy {
    params: NewsReactionParams,
    per_symbol: HashMap<Symbol, SymbolState>,
    timeframes: Vec<Timeframe>,
}

impl NewsReactionStrategy {
    pub fn new(params: NewsReactionParams) -> Self {
        let timeframes = vec![params.execution_tf];
        NewsReactionStrategy {
            params,
            per_symbol: HashMap::new(),
            timeframes,
        }
    }
}

impl Strategy for NewsReactionStrategy {
    fn timeframes(&self) -> &[Timeframe] {
        &self.timeframes
    }

    fn warmup_candles(&self) -> usize {
        self.params.atr_period + 20
    }

    fn on_candle_close(&mut self, ctx: &MarketContext) -> Option<Signal> {
        let p = self.params.clone();
        if ctx.timeframe != p.execution_tf {
            return None;
        }
        let state = self
            .per_symbol
            .entry(ctx.symbol.clone())
            .or_insert_with(|| SymbolState::new(&p));
        let c = ctx.candle;
        let atr = state.atr.update(c)?;
        if atr <= Decimal::ZERO {
            return None;
        }

        // An idle stream arms the moment a scheduled event's timestamp has
        // passed. `next_event` only moves forward, so an event already
        // reacted to (or missed) never re-arms.
        // Arms on the OLDEST unreacted event whose time has passed, one per
        // Idle candle — if a data gap skipped several events at once, this
        // catches up one at a time rather than silently dropping the rest.
        if matches!(state.phase, Phase::Idle) {
            if state.next_event < p.event_times_ms.len()
                && p.event_times_ms[state.next_event] <= c.open_time_ms
            {
                state.next_event += 1;
                state.phase = Phase::InWindow {
                    high: c.high,
                    low: c.low,
                    bars_left: p.window_candles,
                };
            } else {
                return None;
            }
        }

        match state.phase {
            Phase::Idle => None,
            Phase::InWindow {
                high,
                low,
                bars_left,
            } => {
                let high = high.max(c.high);
                let low = low.min(c.low);
                state.phase = if bars_left <= 1 {
                    Phase::Armed {
                        high,
                        low,
                        bars_left: p.active_window_candles,
                    }
                } else {
                    Phase::InWindow {
                        high,
                        low,
                        bars_left: bars_left - 1,
                    }
                };
                None
            }
            Phase::Armed {
                high,
                low,
                bars_left,
            } => {
                let margin = atr * p.min_break_atr;
                let broke_up = c.close > high + margin;
                let broke_down = c.close < low - margin;
                if broke_up {
                    state.phase = Phase::Broken {
                        level: high,
                        dir: Dir::Up,
                        bars: 0,
                    };
                } else if broke_down {
                    state.phase = Phase::Broken {
                        level: low,
                        dir: Dir::Down,
                        bars: 0,
                    };
                } else if bars_left <= 1 {
                    state.phase = Phase::Idle;
                } else {
                    state.phase = Phase::Armed {
                        high,
                        low,
                        bars_left: bars_left - 1,
                    };
                }
                None
            }
            Phase::Broken { level, dir, bars } => {
                let touched = match dir {
                    Dir::Up => c.low <= level,
                    Dir::Down => c.high >= level,
                };
                if touched {
                    state.phase = Phase::Retested {
                        level,
                        dir,
                        touch_high: c.high,
                        touch_low: c.low,
                    };
                } else if bars + 1 > p.retest_window {
                    state.phase = Phase::Idle;
                } else {
                    state.phase = Phase::Broken {
                        level,
                        dir,
                        bars: bars + 1,
                    };
                }
                None
            }
            Phase::Retested {
                level,
                dir,
                touch_high,
                touch_low,
            } => {
                state.phase = Phase::Idle;
                let held = match dir {
                    Dir::Up => c.close > level,
                    Dir::Down => c.close < level,
                };
                let trade_dir = match (held, p.react_mode) {
                    (true, ReactMode::Continuation | ReactMode::Both) => dir,
                    (false, ReactMode::Reversal | ReactMode::Both) => match dir {
                        Dir::Up => Dir::Down,
                        Dir::Down => Dir::Up,
                    },
                    _ => return None,
                };
                let allowed = match trade_dir {
                    Dir::Up => p.allow_long,
                    Dir::Down => p.allow_short,
                };
                if !allowed {
                    return None;
                }
                let side = match trade_dir {
                    Dir::Up => Side::Buy,
                    Dir::Down => Side::Sell,
                };
                let (low, high) = match p.stop_source {
                    StopSource::TouchOnly => (touch_low, touch_high),
                    StopSource::TouchAndReaction => (touch_low.min(c.low), touch_high.max(c.high)),
                };
                let buffer = atr * p.stop_buffer_atr;
                let entry = level + (c.close - level) * p.entry_fraction;
                let stop = match side {
                    Side::Buy => low - buffer,
                    Side::Sell => high + buffer,
                };
                let valid = match side {
                    Side::Buy => stop < entry,
                    Side::Sell => stop > entry,
                };
                if !valid || entry <= Decimal::ZERO || stop <= Decimal::ZERO {
                    return None;
                }
                let risk = (entry - stop).abs();
                if risk <= Decimal::ZERO {
                    return None;
                }
                let target = match side {
                    Side::Buy => entry + risk * p.reward_multiple,
                    Side::Sell => entry - risk * p.reward_multiple,
                };
                if target <= Decimal::ZERO {
                    return None;
                }
                Some(Signal {
                    symbol: ctx.symbol.clone(),
                    side,
                    entry_price: entry,
                    stop_price: stop,
                    target_price: target,
                    atr,
                    signal_candle_open_ms: c.open_time_ms,
                    breakeven_at_r: p.breakeven_at_r,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_fomc_date_falls_inside_the_price_data_window() {
        let data_start = 1_691_121_600_000; // 2023-08-04 00:00 UTC
        let data_end = 1_786_147_200_000; // 2026-08-08 00:00 UTC
        for &t in &FOMC_ANNOUNCEMENTS_UTC_MS {
            assert!(
                t >= data_start && t <= data_end,
                "{t} falls outside the price-data window"
            );
        }
    }

    #[test]
    fn the_calendar_is_strictly_increasing() {
        for w in FOMC_ANNOUNCEMENTS_UTC_MS.windows(2) {
            assert!(w[0] < w[1], "calendar must be sorted with no duplicates");
        }
    }

    #[test]
    fn no_two_announcements_are_implausibly_close() {
        // FOMC meets roughly every 6-8 weeks; anything under 30 days would be
        // a transcription error, not a real second meeting.
        const THIRTY_DAYS_MS: i64 = 30 * 86_400_000;
        for w in FOMC_ANNOUNCEMENTS_UTC_MS.windows(2) {
            assert!(
                w[1] - w[0] >= THIRTY_DAYS_MS,
                "gap between {} and {} is implausibly short",
                w[0],
                w[1]
            );
        }
    }
}
