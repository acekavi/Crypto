use std::collections::HashMap;
use std::collections::VecDeque;

use botcore::{Candle, Side, Symbol, Timeframe};
use indicators::{Atr, Ema, Rsi};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

use crate::signal::{MarketContext, Signal};
use crate::traits::Strategy;

/// Tunable rules. Every threshold is a value here rather than a constant, so
/// Phase 2 can sweep them without touching code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StrategyParams {
    pub ema_fast: usize,
    pub ema_slow: usize,
    pub ema_entry: usize,
    pub rsi_period: usize,
    pub rsi_long_trigger: Decimal,
    pub rsi_short_trigger: Decimal,
    pub atr_period: usize,
    pub atr_band_min_pct: Decimal,
    pub atr_band_max_pct: Decimal,
    pub swing_lookback: usize,
    pub atr_stop_multiple: Decimal,
    pub reward_multiple: Decimal,
    pub pullback_lookback: usize,
    pub pullback_atr_fraction: Decimal,
}

impl StrategyParams {
    /// The spec's defaults. Conventional starting points, deliberately not
    /// tuned — tuning is Phase 2 work against out-of-sample data.
    pub fn defaults() -> Self {
        StrategyParams {
            ema_fast: 50,
            ema_slow: 200,
            ema_entry: 20,
            rsi_period: 14,
            rsi_long_trigger: dec!(40),
            rsi_short_trigger: dec!(60),
            atr_period: 14,
            atr_band_min_pct: dec!(0.003),
            atr_band_max_pct: dec!(0.05),
            swing_lookback: 10,
            atr_stop_multiple: dec!(1.5),
            reward_multiple: dec!(2),
            pullback_lookback: 5,
            pullback_atr_fraction: dec!(0.5),
        }
    }
}

/// Which direction the 4h trend permits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bias {
    Long,
    Short,
    None,
}

/// All incremental state for one symbol.
struct SymbolState {
    ema_fast_h4: Ema,
    ema_slow_h4: Ema,
    ema_entry_h1: Ema,
    rsi_h1: Rsi,
    atr_h1: Atr,
    /// Closed 1h candles, newest last, bounded to `swing_lookback`.
    recent_h1: VecDeque<Candle>,
    /// RSI on the previous 1h close, for detecting a cross rather than a level.
    prev_rsi: Option<Decimal>,
    /// EMA20 value at each of the last `pullback_lookback` 1h closes, so a
    /// pullback is measured against the EMA as it was then, not as it is now.
    recent_ema_entry: VecDeque<Decimal>,
}

impl SymbolState {
    fn new(p: &StrategyParams) -> Self {
        SymbolState {
            ema_fast_h4: Ema::new(p.ema_fast),
            ema_slow_h4: Ema::new(p.ema_slow),
            ema_entry_h1: Ema::new(p.ema_entry),
            rsi_h1: Rsi::new(p.rsi_period),
            atr_h1: Atr::new(p.atr_period),
            recent_h1: VecDeque::new(),
            prev_rsi: None,
            recent_ema_entry: VecDeque::new(),
        }
    }

    fn bias(&self) -> Bias {
        match (self.ema_fast_h4.value(), self.ema_slow_h4.value()) {
            (Some(fast), Some(slow)) if fast > slow => Bias::Long,
            (Some(fast), Some(slow)) if fast < slow => Bias::Short,
            _ => Bias::None,
        }
    }
}

/// Trend-filtered pullback with a limit entry at EMA20.
///
/// This is a starting point for exercising the engine, not a validated edge.
/// No claim is made about its profitability; Phase 2 decides whether it or
/// anything else reaches mainnet.
pub struct PullbackStrategy {
    params: StrategyParams,
    per_symbol: HashMap<Symbol, SymbolState>,
    timeframes: Vec<Timeframe>,
}

impl PullbackStrategy {
    pub fn new(params: StrategyParams) -> Self {
        PullbackStrategy {
            params,
            per_symbol: HashMap::new(),
            timeframes: vec![Timeframe::H1, Timeframe::H4],
        }
    }
}

impl Strategy for PullbackStrategy {
    fn timeframes(&self) -> &[Timeframe] {
        &self.timeframes
    }

    fn warmup_candles(&self) -> usize {
        // The slow EMA is the binding constraint; a margin on top keeps the
        // smoothed value meaningful rather than merely defined.
        self.params.ema_slow + 50
    }

    fn on_candle_close(&mut self, ctx: &MarketContext) -> Option<Signal> {
        let params = self.params.clone();
        let state = self
            .per_symbol
            .entry(ctx.symbol.clone())
            .or_insert_with(|| SymbolState::new(&params));

        match ctx.timeframe {
            // 4h only advances the bias filter. Entries are always timed on 1h.
            Timeframe::H4 => {
                state.ema_fast_h4.update(ctx.candle.close);
                state.ema_slow_h4.update(ctx.candle.close);
                None
            }
            Timeframe::H1 => evaluate_h1(&params, state, ctx),
        }
    }
}

/// Advance 1h state, then test the setup. Returns a signal only on the candle
/// that completes it.
fn evaluate_h1(
    params: &StrategyParams,
    state: &mut SymbolState,
    ctx: &MarketContext,
) -> Option<Signal> {
    let candle = ctx.candle;

    // Advance indicators first so every value below describes this candle.
    let ema20 = state.ema_entry_h1.update(candle.close);
    let rsi_now = state.rsi_h1.update(candle.close);
    let atr = state.atr_h1.update(candle);

    let prev_rsi = state.prev_rsi;
    state.prev_rsi = rsi_now;

    state.recent_h1.push_back(candle.clone());
    while state.recent_h1.len() > params.swing_lookback {
        state.recent_h1.pop_front();
    }

    if let Some(e) = ema20 {
        state.recent_ema_entry.push_back(e);
        while state.recent_ema_entry.len() > params.pullback_lookback {
            state.recent_ema_entry.pop_front();
        }
    }

    // Every indicator must be warm, and we need a previous RSI to detect a
    // cross rather than merely a level.
    let (ema20, rsi_now, atr, prev_rsi) = (ema20?, rsi_now?, atr?, prev_rsi?);

    let bias = state.bias();
    if bias == Bias::None {
        return None;
    }

    // Volatility gate: skip dead and berserk markets.
    if candle.close.is_zero() {
        return None;
    }
    let atr_pct = atr / candle.close;
    if atr_pct < params.atr_band_min_pct || atr_pct > params.atr_band_max_pct {
        return None;
    }

    // Need a full swing window before a stop can be located.
    if state.recent_h1.len() < params.swing_lookback {
        return None;
    }

    let side = match bias {
        Bias::Long => Side::Buy,
        Bias::Short => Side::Sell,
        Bias::None => unreachable!("bias None returned above"),
    };

    if !pullback_occurred(params, state, side, atr) {
        return None;
    }
    if !rsi_triggered(params, side, prev_rsi, rsi_now) {
        return None;
    }

    let stop_price = stop_for(params, state, side, ema20, atr);
    let risk = (ema20 - stop_price).abs();
    if risk.is_zero() {
        return None;
    }
    let target_price = match side {
        Side::Buy => ema20 + risk * params.reward_multiple,
        Side::Sell => ema20 - risk * params.reward_multiple,
    };

    Some(Signal {
        symbol: ctx.symbol.clone(),
        side,
        entry_price: ema20,
        stop_price,
        target_price,
        atr,
        signal_candle_open_ms: candle.open_time_ms,
    })
}

/// True when price came within `pullback_atr_fraction × ATR` of the EMA20
/// within the lookback window, in the direction the bias permits.
///
/// Each candle is compared against the EMA as it stood at that candle's close,
/// not today's EMA — otherwise a fast-moving EMA would retroactively invent or
/// erase pullbacks.
fn pullback_occurred(
    params: &StrategyParams,
    state: &SymbolState,
    side: Side,
    atr: Decimal,
) -> bool {
    let threshold = atr * params.pullback_atr_fraction;
    let n = params.pullback_lookback.min(state.recent_h1.len());
    let candles = state.recent_h1.iter().rev().take(n);
    let emas = state.recent_ema_entry.iter().rev().take(n);

    for (c, e) in candles.zip(emas) {
        let touched = match side {
            Side::Buy => (c.low - *e).abs() <= threshold,
            Side::Sell => (c.high - *e).abs() <= threshold,
        };
        if touched {
            return true;
        }
    }
    false
}

/// True when RSI crossed the trigger this candle — not merely sits past it.
fn rsi_triggered(params: &StrategyParams, side: Side, prev: Decimal, now: Decimal) -> bool {
    match side {
        Side::Buy => prev < params.rsi_long_trigger && now >= params.rsi_long_trigger,
        Side::Sell => prev > params.rsi_short_trigger && now <= params.rsi_short_trigger,
    }
}

/// The further from entry of the swing extreme and the ATR-based stop.
///
/// "Further" is deliberate: taking the tighter of the two would put the stop
/// inside recent noise, where it gets hit by ordinary chop rather than by the
/// setup being wrong.
fn stop_for(
    params: &StrategyParams,
    state: &SymbolState,
    side: Side,
    entry: Decimal,
    atr: Decimal,
) -> Decimal {
    let atr_stop = match side {
        Side::Buy => entry - atr * params.atr_stop_multiple,
        Side::Sell => entry + atr * params.atr_stop_multiple,
    };
    match side {
        Side::Buy => {
            let swing_low = state
                .recent_h1
                .iter()
                .map(|c| c.low)
                .min()
                .unwrap_or(atr_stop);
            swing_low.min(atr_stop)
        }
        Side::Sell => {
            let swing_high = state
                .recent_h1
                .iter()
                .map(|c| c.high)
                .max()
                .unwrap_or(atr_stop);
            swing_high.max(atr_stop)
        }
    }
}
