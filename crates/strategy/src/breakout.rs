//! Range breakout with a pullback entry.
//!
//! Every rule is written out rather than assumed — "breakout pullback" names a
//! family of ideas, and a result validates THESE definitions, not the family.
//!
//! 1. A range is the highest high and lowest low of the previous `lookback`
//!    candles, excluding the current one.
//! 2. A breakout is a CLOSE beyond that range by at least `min_break_atr`
//!    ATRs. Requiring displacement rather than a bare touch is what separates
//!    a breakout from a wick poking one tick through a level.
//! 3. The entry rests at the broken level itself — the retest. This is a LIMIT
//!    order by construction, so a filled entry pays the maker fee.
//! 4. The stop is the breakout candle's opposite extreme: the level the move
//!    came from. A breakout candle entirely clear of the range leaves the stop
//!    on the wrong side of the entry, and is refused rather than resized.
//! 5. One breakout produces at most one entry.

use std::collections::{HashMap, VecDeque};

use botcore::{Candle, Side, Symbol, Timeframe};
use indicators::Atr;
use rust_decimal::Decimal;

use crate::signal::{MarketContext, Signal};
use crate::traits::Strategy;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BreakoutParams {
    /// Candles forming the range the breakout must clear.
    pub lookback: usize,
    pub atr_period: usize,
    /// How far beyond the range the close must sit, in ATRs.
    pub min_break_atr: Decimal,
    /// Target as a multiple of risk.
    pub reward_multiple: Decimal,
    /// Pull the stop to entry once the trade travels this many R in favour.
    pub breakeven_at_r: Option<Decimal>,
    pub timeframe: Timeframe,
    pub allow_long: bool,
    pub allow_short: bool,
}

impl BreakoutParams {
    /// The requested configuration: 5-minute chart, retest entry.
    pub fn m5_default() -> Self {
        BreakoutParams {
            lookback: 20,
            atr_period: 14,
            min_break_atr: Decimal::new(25, 2), // 0.25
            reward_multiple: Decimal::from(5),
            breakeven_at_r: Some(Decimal::TWO),
            timeframe: Timeframe::M5,
            allow_long: true,
            allow_short: true,
        }
    }
}

struct SymbolState {
    highs: VecDeque<Decimal>,
    lows: VecDeque<Decimal>,
    atr: Atr,
}

impl SymbolState {
    fn new(p: &BreakoutParams) -> Self {
        SymbolState {
            highs: VecDeque::with_capacity(p.lookback),
            lows: VecDeque::with_capacity(p.lookback),
            atr: Atr::new(p.atr_period),
        }
    }
}

pub struct BreakoutStrategy {
    params: BreakoutParams,
    per_symbol: HashMap<Symbol, SymbolState>,
    timeframes: Vec<Timeframe>,
}

impl BreakoutStrategy {
    pub fn new(params: BreakoutParams) -> Self {
        let timeframes = vec![params.timeframe];
        BreakoutStrategy {
            params,
            per_symbol: HashMap::new(),
            timeframes,
        }
    }
}

impl Strategy for BreakoutStrategy {
    fn timeframes(&self) -> &[Timeframe] {
        &self.timeframes
    }

    fn warmup_candles(&self) -> usize {
        self.params.lookback + self.params.atr_period + 50
    }

    fn on_candle_close(&mut self, ctx: &MarketContext) -> Option<Signal> {
        let p = self.params.clone();
        let state = self
            .per_symbol
            .entry(ctx.symbol.clone())
            .or_insert_with(|| SymbolState::new(&p));
        let c = ctx.candle;

        // The range excludes the current candle, so read it BEFORE pushing.
        let ready = state.highs.len() == p.lookback;
        let range_high = state.highs.iter().copied().max();
        let range_low = state.lows.iter().copied().min();
        let atr = state.atr.update(c);

        state.highs.push_back(c.high);
        state.lows.push_back(c.low);
        if state.highs.len() > p.lookback {
            state.highs.pop_front();
            state.lows.pop_front();
        }

        if !ready {
            return None;
        }
        let (rh, rl, atr) = (range_high?, range_low?, atr?);
        if atr <= Decimal::ZERO {
            return None;
        }
        let margin = atr * p.min_break_atr;

        // Direction, entry at the broken level, stop at the breakout candle's
        // opposite extreme.
        let (side, entry, stop) = if c.close > rh + margin && p.allow_long {
            (Side::Buy, rh, c.low)
        } else if c.close < rl - margin && p.allow_short {
            (Side::Sell, rl, c.high)
        } else {
            return None;
        };

        // A stop on the wrong side of the entry means the breakout candle
        // never traded back into the range — there is nothing to retest.
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
