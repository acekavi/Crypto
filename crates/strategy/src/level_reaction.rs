//! Break a key level, wait for the retest, then read the reaction candle.
//!
//! The state machine is the strategy:
//!
//! ```text
//!   Idle ──break beyond a swing level──▶ Broken ──price returns to it──▶ Retested
//!     ▲                                    │                                │
//!     └───────── retest window expires ────┘                                │
//!     └──────────── the NEXT candle's reaction decides the trade ───────────┘
//! ```
//!
//! The reaction candle is what separates this from a plain breakout-retest: a
//! passive limit at the level takes every retest, whereas this commits only
//! after seeing whether the level HELD (close back on the breakout side) or
//! FAILED (close back through). Both readings are tradeable and which one is
//! taken is a parameter, not an assumption.
//!
//! Entries rest at the level itself, so a fill pays the maker fee — the reaction
//! candle decides *whether* to place the order, never *where*.

use std::collections::{HashMap, VecDeque};

use botcore::{Candle, Side, Symbol, Timeframe};
use indicators::Atr;
use rust_decimal::Decimal;

use crate::ict::{in_ny_session, is_swing_high, is_swing_low};
use crate::signal::{MarketContext, Signal};
use crate::traits::Strategy;

/// Which reading of the reaction candle is traded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReactMode {
    /// The level held: close back on the breakout side. Trade the breakout way.
    Continuation,
    /// The level failed: close back through it. Trade the opposite way.
    Reversal,
    /// Take whichever the reaction candle shows.
    Both,
}

/// Which candles the structural stop is measured across.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopSource {
    /// The touch candle's extreme only — tighter, ignores how far the
    /// reaction candle itself travelled.
    TouchOnly,
    /// Touch and reaction candles combined — the original, wider construction.
    TouchAndReaction,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Dir {
    Up,
    Down,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LevelParams {
    /// Where swing levels are found.
    pub level_tf: Timeframe,
    /// Where breaks, retests and reactions are read.
    pub execution_tf: Timeframe,
    /// Bars either side of a fractal swing on `level_tf`.
    pub swing_lookback: usize,
    /// How many recent levels stay live.
    pub max_levels: usize,
    pub atr_period: usize,
    /// How far beyond the level the breaking candle must CLOSE, in ATRs.
    pub min_break_atr: Decimal,
    /// Execution candles allowed between the break and the retest touch.
    pub retest_window: usize,
    pub react_mode: ReactMode,
    pub reward_multiple: Decimal,
    pub breakeven_at_r: Option<Decimal>,
    /// Where the entry rests between the level and the reaction candle's
    /// close. 0.0 = at the level (deepest retracement required, current
    /// default). 1.0 = at the reaction candle's own close (shallowest, best
    /// fill rate, worst average price).
    pub entry_fraction: Decimal,
    /// Which candles the structural stop is measured across.
    pub stop_source: StopSource,
    /// ATR margin added beyond the structural stop, in either direction.
    pub stop_buffer_atr: Decimal,
    /// Minimum range of the REACTION candle, in ATRs, for the setup to
    /// qualify. Filters weak, doji-like reactions that show no conviction
    /// either way, distinct from every entry/stop lever already tested —
    /// this changes WHETHER a setup is taken, not where the entry or stop
    /// sits within one that already qualified.
    pub min_reaction_atr: Decimal,
    /// If set, a signal only fires once a LATER candle closes beyond the
    /// reaction candle's own extreme (in the trade direction) by this many
    /// ATRs — genuine momentum confirmation using already-closed data, not a
    /// conditional order. `None` keeps the original behaviour: the reaction
    /// candle alone decides the trade.
    pub confirm_margin_atr: Option<Decimal>,
    /// Execution candles allowed to wait for that confirmation before the
    /// setup is abandoned.
    pub confirm_window: usize,
    /// Gate entries to the New York session.
    ///
    /// A FIXED UTC window approximating 09:30-16:00 ET, so it is one hour off
    /// for part of each year across US daylight saving. Declared as an
    /// approximation rather than silently treated as exact — same window the
    /// ICT strategy uses, so the two cannot drift.
    pub session_filter: bool,
    pub ny_open_ms: i64,
    pub ny_close_ms: i64,
}

impl LevelParams {
    pub fn m5_default() -> Self {
        LevelParams {
            level_tf: Timeframe::H1,
            execution_tf: Timeframe::M5,
            swing_lookback: 5,
            max_levels: 6,
            atr_period: 14,
            min_break_atr: Decimal::new(25, 2),
            retest_window: 24,
            react_mode: ReactMode::Continuation,
            reward_multiple: Decimal::from(5),
            breakeven_at_r: Some(Decimal::TWO),
            entry_fraction: Decimal::ZERO,
            stop_source: StopSource::TouchAndReaction,
            stop_buffer_atr: Decimal::ZERO,
            min_reaction_atr: Decimal::ZERO,
            confirm_margin_atr: None,
            confirm_window: 12,
            session_filter: false,
            ny_open_ms: 13 * 3_600_000 + 30 * 60_000, // 13:30 UTC
            ny_close_ms: 20 * 3_600_000,              // 20:00 UTC
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum Phase {
    Idle,
    /// Broken and waiting for price to come back to the level.
    Broken {
        level: Decimal,
        dir: Dir,
        bars: usize,
    },
    /// Price touched the level; the NEXT candle is the reaction candle.
    Retested {
        level: Decimal,
        dir: Dir,
        touch_high: Decimal,
        touch_low: Decimal,
    },
    /// The reaction candle confirmed a direction; waiting for a LATER candle
    /// to close beyond the reaction candle's own extreme before committing.
    AwaitingConfirmation {
        level: Decimal,
        trade_dir: Dir,
        touch_low: Decimal,
        touch_high: Decimal,
        reaction_low: Decimal,
        reaction_high: Decimal,
        /// Reaction candle's high (Up) / low (Down) — what a later candle
        /// must close beyond.
        confirm_extreme: Decimal,
        atr_at_reaction: Decimal,
        bars: usize,
    },
}

struct SymbolState {
    /// Recent level-timeframe candles, for fractal confirmation.
    htf: VecDeque<Candle>,
    resistances: VecDeque<Decimal>,
    supports: VecDeque<Decimal>,
    atr: Atr,
    phase: Phase,
}

impl SymbolState {
    fn new(p: &LevelParams) -> Self {
        SymbolState {
            htf: VecDeque::new(),
            resistances: VecDeque::new(),
            supports: VecDeque::new(),
            atr: Atr::new(p.atr_period),
            phase: Phase::Idle,
        }
    }
}

pub struct LevelReactionStrategy {
    params: LevelParams,
    per_symbol: HashMap<Symbol, SymbolState>,
    timeframes: Vec<Timeframe>,
}

impl LevelReactionStrategy {
    pub fn new(params: LevelParams) -> Self {
        let timeframes = vec![params.level_tf, params.execution_tf];
        LevelReactionStrategy {
            params,
            per_symbol: HashMap::new(),
            timeframes,
        }
    }
}

/// Track swing highs and lows on the level timeframe.
fn track_levels(state: &mut SymbolState, p: &LevelParams, candle: &Candle) {
    state.htf.push_back(candle.clone());
    let span = 2 * p.swing_lookback + 1;
    while state.htf.len() > span {
        state.htf.pop_front();
    }
    if state.htf.len() < span {
        return;
    }
    let slice: Vec<Candle> = state.htf.iter().cloned().collect();
    let mid = p.swing_lookback;
    if is_swing_high(&slice, mid, p.swing_lookback) {
        state.resistances.push_back(slice[mid].high);
        while state.resistances.len() > p.max_levels {
            state.resistances.pop_front();
        }
    }
    if is_swing_low(&slice, mid, p.swing_lookback) {
        state.supports.push_back(slice[mid].low);
        while state.supports.len() > p.max_levels {
            state.supports.pop_front();
        }
    }
}

impl Strategy for LevelReactionStrategy {
    fn timeframes(&self) -> &[Timeframe] {
        &self.timeframes
    }

    fn warmup_candles(&self) -> usize {
        (2 * self.params.swing_lookback + 1)
            .max(self.params.atr_period)
            .max(self.params.retest_window)
            + 60
    }

    fn on_candle_close(&mut self, ctx: &MarketContext) -> Option<Signal> {
        let p = self.params.clone();
        let state = self
            .per_symbol
            .entry(ctx.symbol.clone())
            .or_insert_with(|| SymbolState::new(&p));
        let c = ctx.candle;

        if ctx.timeframe == p.level_tf {
            track_levels(state, &p, c);
            return None;
        }
        if ctx.timeframe != p.execution_tf {
            return None;
        }

        let atr = state.atr.update(c)?;
        if atr <= Decimal::ZERO {
            return None;
        }
        let margin = atr * p.min_break_atr;

        match state.phase {
            Phase::Idle => {
                // A CLOSE beyond a level by the margin, not a wick through it.
                let broke_up = state
                    .resistances
                    .iter()
                    .filter(|l| c.close > **l + margin)
                    .copied()
                    .max();
                let broke_dn = state
                    .supports
                    .iter()
                    .filter(|l| c.close < **l - margin)
                    .copied()
                    .min();
                if let Some(level) = broke_up {
                    state.phase = Phase::Broken {
                        level,
                        dir: Dir::Up,
                        bars: 0,
                    };
                } else if let Some(level) = broke_dn {
                    state.phase = Phase::Broken {
                        level,
                        dir: Dir::Down,
                        bars: 0,
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

                // Gate on the reaction candle: it is the one that decides the
                // entry, so that is the moment the session has to be open.
                if p.session_filter && !in_ny_session(c.open_time_ms, p.ny_open_ms, p.ny_close_ms) {
                    return None;
                }

                // A weak, doji-like reaction shows no conviction either way
                // and is filtered before the held/failed read even matters.
                if (c.high - c.low) < atr * p.min_reaction_atr {
                    return None;
                }

                // The reaction: did the level hold, or fail?
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

                if p.confirm_margin_atr.is_some() {
                    // Do not commit yet: wait for a later candle to prove the
                    // reaction had follow-through, using already-closed data
                    // rather than an order that would fill at a fabricated
                    // price if it rested beyond current market.
                    state.phase = Phase::AwaitingConfirmation {
                        level,
                        trade_dir,
                        touch_low,
                        touch_high,
                        reaction_low: c.low,
                        reaction_high: c.high,
                        confirm_extreme: match trade_dir {
                            Dir::Up => c.high,
                            Dir::Down => c.low,
                        },
                        atr_at_reaction: atr,
                        bars: 0,
                    };
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
                build_signal(
                    ctx.symbol,
                    side,
                    level,
                    c.close,
                    low,
                    high,
                    atr,
                    c.open_time_ms,
                    &p,
                )
            }
            Phase::AwaitingConfirmation {
                level,
                trade_dir,
                touch_low,
                touch_high,
                reaction_low,
                reaction_high,
                confirm_extreme,
                atr_at_reaction,
                bars,
            } => {
                let margin = p
                    .confirm_margin_atr
                    .expect("only entered while confirm_margin_atr is set");
                let confirmed = match trade_dir {
                    Dir::Up => c.close > confirm_extreme + atr_at_reaction * margin,
                    Dir::Down => c.close < confirm_extreme - atr_at_reaction * margin,
                };
                if confirmed {
                    state.phase = Phase::Idle;
                    let side = match trade_dir {
                        Dir::Up => Side::Buy,
                        Dir::Down => Side::Sell,
                    };
                    // TouchOnly stays touch-only regardless of confirmation;
                    // TouchAndReaction now folds in every candle the setup
                    // travelled through, including this confirming one.
                    let (low, high) = match p.stop_source {
                        StopSource::TouchOnly => (touch_low, touch_high),
                        StopSource::TouchAndReaction => (
                            touch_low.min(reaction_low).min(c.low),
                            touch_high.max(reaction_high).max(c.high),
                        ),
                    };
                    build_signal(
                        ctx.symbol,
                        side,
                        level,
                        c.close,
                        low,
                        high,
                        atr,
                        c.open_time_ms,
                        &p,
                    )
                } else if bars + 1 > p.confirm_window {
                    state.phase = Phase::Idle;
                    None
                } else {
                    state.phase = Phase::AwaitingConfirmation {
                        level,
                        trade_dir,
                        touch_low,
                        touch_high,
                        reaction_low,
                        reaction_high,
                        confirm_extreme,
                        atr_at_reaction,
                        bars: bars + 1,
                    };
                    None
                }
            }
        }
    }
}

/// Shared entry/stop/target construction for both the immediate path (no
/// confirmation required) and the confirmed path (a later candle proved
/// follow-through). `anchor_close` is whichever candle's close the entry
/// fraction is interpolated toward — the reaction candle's, or the
/// confirming candle's.
#[allow(clippy::too_many_arguments)]
fn build_signal(
    symbol: &Symbol,
    side: Side,
    level: Decimal,
    anchor_close: Decimal,
    low: Decimal,
    high: Decimal,
    atr: Decimal,
    signal_candle_open_ms: i64,
    p: &LevelParams,
) -> Option<Signal> {
    let buffer = atr * p.stop_buffer_atr;
    // Entry rests between the level (0.0) and the anchor candle's own close
    // (1.0) — the shallower the fraction, the deeper the retracement
    // required and the fewer signals fill.
    let entry = level + (anchor_close - level) * p.entry_fraction;
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
        symbol: symbol.clone(),
        side,
        entry_price: entry,
        stop_price: stop,
        target_price: target,
        atr,
        signal_candle_open_ms,
        breakeven_at_r: p.breakeven_at_r,
    })
}
