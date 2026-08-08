//! ICT-style structural setup: liquidity sweep, market structure shift, then
//! an entry in the fair value gap the shift left behind.
//!
//! Pre-registered in `docs/superpowers/specs/2026-08-08-ict-study-design.md`
//! before any run. Every definition below is written out rather than assumed —
//! practitioners formalise these differently, and a result validates THESE
//! definitions, not "ICT" as a body of ideas.
//!
//! The detection rules are pure functions taking plain candle slices, so each
//! can be tested on a hand-built sequence. That is the lesson from the fill
//! model: a rule exercised only through the engine caught its own defect just
//! four runs in six.

use std::collections::{HashMap, VecDeque};

use botcore::{Candle, Side, Symbol, Timeframe};
use indicators::{Atr, Ema};
use rust_decimal::Decimal;

use crate::signal::{MarketContext, Signal};
use crate::traits::Strategy;

/// Which way a setup points. Named for the DIRECTION TRADED, not for what was
/// swept: a bullish setup sweeps lows and then buys.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Bullish,
    Bearish,
}

impl Direction {
    pub fn side(self) -> Side {
        match self {
            Direction::Bullish => Side::Buy,
            Direction::Bearish => Side::Sell,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IctParams {
    /// Period of the D1 and H4 bias EMA.
    pub bias_ema: usize,
    /// Candles either side of a swing point.
    pub swing_lookback: usize,
    /// H1 candles a sweep has to produce a structure shift within.
    pub mss_window: usize,
    /// Depth into the gap the entry sits, measured from the edge price reaches
    /// first. 0.5 is the midpoint.
    pub fvg_entry_fraction: Decimal,
    /// Stop distance beyond the sweep extreme, in M15 ATRs.
    pub stop_buffer_atr: Decimal,
    pub atr_period: usize,
    /// New York session, in ms since UTC midnight.
    ///
    /// A FIXED UTC window approximating 09:30-16:00 ET. US daylight saving
    /// shifts ET against UTC, so this is one hour off for part of each year.
    /// Declared as an approximation rather than silently treated as exact;
    /// correcting it needs a timezone database and its own edge cases.
    pub ny_open_ms: i64,
    pub ny_close_ms: i64,
    /// Whether the session window gates entries at all.
    ///
    /// EXPLORATORY. The pre-registered study fixes this true; running it false
    /// makes the result a diagnostic, not that study's outcome.
    pub session_filter: bool,
    /// Target as a multiple of risk.
    ///
    /// EXPLORATORY above 2. The owner's standing rule is 1:2, and the
    /// pre-registered study fixes it there. A higher multiple lowers the
    /// breakeven win rate (1:3 needs 25% rather than 33.3%) but the target is
    /// further away, so fewer trades reach it — the two effects pull against
    /// each other and only measurement settles it.
    pub reward_multiple: Decimal,
}

impl IctParams {
    /// Variant A — the study's PRIMARY, named before any result.
    pub fn variant_a() -> Self {
        IctParams {
            bias_ema: 50,
            swing_lookback: 5,
            mss_window: 12,
            fvg_entry_fraction: Decimal::new(5, 1),
            stop_buffer_atr: Decimal::new(25, 2),
            atr_period: 14,
            ny_open_ms: 13 * 3_600_000 + 30 * 60_000, // 13:30 UTC
            ny_close_ms: 20 * 3_600_000,              // 20:00 UTC
            session_filter: true,
            reward_multiple: Decimal::TWO,
        }
    }

    /// The six pre-declared variants. Each changes exactly ONE field from A,
    /// so a difference in results is attributable to that field.
    pub fn declared_variants() -> Vec<(&'static str, Self)> {
        let a = Self::variant_a();
        vec![
            ("A", a.clone()),
            (
                "B",
                IctParams {
                    fvg_entry_fraction: Decimal::new(75, 2),
                    ..a.clone()
                },
            ),
            (
                "C",
                IctParams {
                    swing_lookback: 3,
                    ..a.clone()
                },
            ),
            (
                "D",
                IctParams {
                    swing_lookback: 8,
                    ..a.clone()
                },
            ),
            (
                "E",
                IctParams {
                    mss_window: 24,
                    ..a.clone()
                },
            ),
            (
                "F",
                IctParams {
                    stop_buffer_atr: Decimal::new(50, 2),
                    ..a
                },
            ),
        ]
    }
}

// ----------------------------------------------------------- pure rules ----

/// A swing high needs `lookback` candles either side, all with lower highs.
///
/// Requires both sides, so the most recent `lookback` candles can never be
/// swing points yet — that is the cost of a confirmed structure rather than a
/// guessed one.
pub fn is_swing_high(candles: &[Candle], idx: usize, lookback: usize) -> bool {
    if lookback == 0 || idx < lookback || idx + lookback >= candles.len() {
        return false;
    }
    let h = candles[idx].high;
    (idx - lookback..=idx + lookback).all(|i| i == idx || candles[i].high < h)
}

pub fn is_swing_low(candles: &[Candle], idx: usize, lookback: usize) -> bool {
    if lookback == 0 || idx < lookback || idx + lookback >= candles.len() {
        return false;
    }
    let l = candles[idx].low;
    (idx - lookback..=idx + lookback).all(|i| i == idx || candles[i].low > l)
}

/// A sweep takes out a prior swing and CLOSES BACK THROUGH it.
///
/// The close is what separates a sweep from a genuine break: price reached for
/// the liquidity resting beyond the level and was rejected. A candle that
/// closes past the level has broken it, which is the opposite event.
pub fn detect_sweep(
    candle: &Candle,
    prior_swing_high: Option<Decimal>,
    prior_swing_low: Option<Decimal>,
) -> Option<Direction> {
    if let Some(low) = prior_swing_low
        && candle.low < low
        && candle.close > low
    {
        // Sold into the lows and recovered — trade UP.
        return Some(Direction::Bullish);
    }
    if let Some(high) = prior_swing_high
        && candle.high > high
        && candle.close < high
    {
        return Some(Direction::Bearish);
    }
    None
}

/// After a sweep, a close through the opposing swing confirms the turn.
///
/// A bullish setup needs a close ABOVE the most recent swing high: price
/// rejected the lows and then took out the level above, which is structure
/// shifting rather than merely pausing.
pub fn is_mss(
    direction: Direction,
    close: Decimal,
    recent_swing_high: Option<Decimal>,
    recent_swing_low: Option<Decimal>,
) -> bool {
    match direction {
        Direction::Bullish => recent_swing_high.is_some_and(|h| close > h),
        Direction::Bearish => recent_swing_low.is_some_and(|l| close < l),
    }
}

/// The untraded range left by a three-candle imbalance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fvg {
    pub low: Decimal,
    pub high: Decimal,
}

/// Three consecutive candles leaving a gap price never traded through.
///
/// Bullish: candle 1's high sits below candle 3's low, so the range between
/// them went unfilled on the way up. Bearish is the mirror. Only gaps facing
/// the setup's direction count — a bullish setup enters a gap BELOW price, on
/// a retracement into it.
pub fn find_fvg(c1: &Candle, c3: &Candle, direction: Direction) -> Option<Fvg> {
    match direction {
        Direction::Bullish if c1.high < c3.low => Some(Fvg {
            low: c1.high,
            high: c3.low,
        }),
        Direction::Bearish if c1.low > c3.high => Some(Fvg {
            low: c3.high,
            high: c1.low,
        }),
        _ => None,
    }
}

/// Where the limit sits inside the gap.
///
/// Measured from the edge price reaches FIRST as it retraces: a bullish setup
/// falls into the gap from above, so depth is measured down from the top.
pub fn fvg_entry_price(fvg: &Fvg, direction: Direction, fraction: Decimal) -> Decimal {
    let span = fvg.high - fvg.low;
    match direction {
        Direction::Bullish => fvg.high - span * fraction,
        Direction::Bearish => fvg.low + span * fraction,
    }
}

/// Whether a candle opens inside the New York window.
pub fn in_ny_session(open_time_ms: i64, open_ms: i64, close_ms: i64) -> bool {
    let ms_into_day = open_time_ms.rem_euclid(86_400_000);
    ms_into_day >= open_ms && ms_into_day < close_ms
}

/// How many candidates survive each stage of the setup.
///
/// Counters only — they never gate anything, so instrumenting cannot change
/// what the strategy does. Counting inside the real state machine rather than
/// a reimplementation matters: a parallel copy would measure a subtly
/// different thing, which is the whole failure mode being investigated.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Funnel {
    pub h1_candles: usize,
    pub sweeps: usize,
    pub mss_armed: usize,
    pub m15_candles: usize,
    pub in_session: usize,
    pub setup_active: usize,
    pub bias_aligned: usize,
    pub not_expired: usize,
    pub fvg_found: usize,
    pub signals: usize,
}

// ---------------------------------------------------------- the machine ----

/// A confirmed sweep-and-shift waiting for a gap to enter.
#[derive(Debug, Clone)]
struct ArmedSetup {
    direction: Direction,
    /// The sweep's extreme — structural invalidation. Price returning here
    /// means the sweep was not a sweep.
    sweep_extreme: Decimal,
    /// H1 candle index the setup was armed at, for expiry.
    armed_at: usize,
}

struct SymbolState {
    ema_d1: Ema,
    ema_h4: Ema,
    bias_d1: Option<Direction>,
    bias_h4: Option<Direction>,
    /// H1 candles, newest last, bounded so swings can be confirmed.
    h1: VecDeque<Candle>,
    /// Count of H1 candles ever seen, so expiry survives the window sliding.
    h1_seen: usize,
    /// The most recent sweep still inside its window, if any.
    pending_sweep: Option<(Direction, Decimal, usize)>,
    armed: Option<ArmedSetup>,
    /// Last three M15 candles, for gap detection.
    m15: VecDeque<Candle>,
    atr_m15: Atr,
}

impl SymbolState {
    fn new(p: &IctParams) -> Self {
        SymbolState {
            ema_d1: Ema::new(p.bias_ema),
            ema_h4: Ema::new(p.bias_ema),
            bias_d1: None,
            bias_h4: None,
            h1: VecDeque::new(),
            h1_seen: 0,
            pending_sweep: None,
            armed: None,
            m15: VecDeque::new(),
            atr_m15: Atr::new(p.atr_period),
        }
    }

    /// Both higher timeframes must agree. Disagreement means no trade — there
    /// is deliberately no partial-alignment tier, which would be another knob.
    fn aligned_bias(&self) -> Option<Direction> {
        match (self.bias_d1, self.bias_h4) {
            (Some(a), Some(b)) if a == b => Some(a),
            _ => None,
        }
    }
}

pub struct IctStrategy {
    params: IctParams,
    funnel: Funnel,
    per_symbol: HashMap<Symbol, SymbolState>,
    timeframes: Vec<Timeframe>,
}

impl IctStrategy {
    /// Stage-by-stage survival counts, for diagnosing WHERE setups die.
    pub fn funnel(&self) -> Funnel {
        self.funnel
    }

    pub fn new(params: IctParams) -> Self {
        IctStrategy {
            params,
            funnel: Funnel::default(),
            per_symbol: HashMap::new(),
            timeframes: vec![Timeframe::M15, Timeframe::H1, Timeframe::H4, Timeframe::D1],
        }
    }
}

fn bias_from(close: Decimal, ema: Option<Decimal>) -> Option<Direction> {
    let e = ema?;
    Some(if close > e {
        Direction::Bullish
    } else {
        Direction::Bearish
    })
}

impl Strategy for IctStrategy {
    fn timeframes(&self) -> &[Timeframe] {
        &self.timeframes
    }

    fn warmup_candles(&self) -> usize {
        // The daily bias EMA is the binding constraint, with margin so the
        // smoothed value is meaningful rather than merely defined.
        self.params.bias_ema + 50
    }

    fn on_candle_close(&mut self, ctx: &MarketContext) -> Option<Signal> {
        let p = self.params.clone();
        let state = self
            .per_symbol
            .entry(ctx.symbol.clone())
            .or_insert_with(|| SymbolState::new(&p));
        let candle = ctx.candle;

        match ctx.timeframe {
            Timeframe::D1 => {
                let e = state.ema_d1.update(candle.close);
                state.bias_d1 = bias_from(candle.close, e);
                None
            }
            Timeframe::H4 => {
                let e = state.ema_h4.update(candle.close);
                state.bias_h4 = bias_from(candle.close, e);
                None
            }
            Timeframe::H1 => {
                track_structure(&p, state, candle, &mut self.funnel);
                None
            }
            Timeframe::M15 => evaluate_m15(&p, state, ctx, &mut self.funnel),
        }
    }
}

/// Advance H1 structure: find swings, detect a sweep, then a shift.
fn track_structure(p: &IctParams, state: &mut SymbolState, candle: &Candle, f: &mut Funnel) {
    f.h1_candles += 1;
    // Confirmed swings from the window BEFORE this candle, so the levels a
    // sweep is measured against are ones that already existed.
    let (prior_high, prior_low) = confirmed_swings(&state.h1, p.swing_lookback);

    state.h1.push_back(candle.clone());
    state.h1_seen += 1;
    // Two lookbacks plus the candle itself is the most any swing test reads.
    while state.h1.len() > p.swing_lookback * 2 + 2 {
        state.h1.pop_front();
    }

    // A sweep that never produced a shift goes stale rather than lingering.
    if let Some((_, _, at)) = state.pending_sweep
        && state.h1_seen.saturating_sub(at) > p.mss_window
    {
        state.pending_sweep = None;
    }

    if let Some((direction, extreme, _)) = state.pending_sweep
        && is_mss(direction, candle.close, prior_high, prior_low)
    {
        f.mss_armed += 1;
        state.armed = Some(ArmedSetup {
            direction,
            sweep_extreme: extreme,
            armed_at: state.h1_seen,
        });
        state.pending_sweep = None;
        return;
    }

    if let Some(direction) = detect_sweep(candle, prior_high, prior_low) {
        let extreme = match direction {
            Direction::Bullish => candle.low,
            Direction::Bearish => candle.high,
        };
        f.sweeps += 1;
        state.pending_sweep = Some((direction, extreme, state.h1_seen));
    }
}

/// The most recent confirmed swing high and low in the window.
fn confirmed_swings(h1: &VecDeque<Candle>, lookback: usize) -> (Option<Decimal>, Option<Decimal>) {
    let v: Vec<Candle> = h1.iter().cloned().collect();
    let mut high = None;
    let mut low = None;
    for i in 0..v.len() {
        if high.is_none() && is_swing_high(&v, v.len() - 1 - i, lookback) {
            high = Some(v[v.len() - 1 - i].high);
        }
        if low.is_none() && is_swing_low(&v, v.len() - 1 - i, lookback) {
            low = Some(v[v.len() - 1 - i].low);
        }
        if high.is_some() && low.is_some() {
            break;
        }
    }
    (high, low)
}

fn evaluate_m15(
    p: &IctParams,
    state: &mut SymbolState,
    ctx: &MarketContext,
    f: &mut Funnel,
) -> Option<Signal> {
    let candle = ctx.candle;
    f.m15_candles += 1;
    let atr = state.atr_m15.update(candle);

    state.m15.push_back(candle.clone());
    while state.m15.len() > 3 {
        state.m15.pop_front();
    }

    let atr = atr?;
    if atr <= Decimal::ZERO {
        return None;
    }
    if p.session_filter && !in_ny_session(candle.open_time_ms, p.ny_open_ms, p.ny_close_ms) {
        return None;
    }
    f.in_session += 1;

    let setup = state.armed.clone()?;
    f.setup_active += 1;

    // EXPIRY FIRST, before any other gate can return early.
    //
    // This check used to sit after the bias comparison, which meant a setup
    // whose bias never aligned never reached it — and so was never cleared.
    // It stayed armed indefinitely and could fire months later, long after
    // the sweep and structure shift that justified it had stopped being
    // relevant. The funnel exposed it: 54 armed setups accounted for 16,116
    // armed-and-in-session candles, roughly 300 each against a window worth
    // 48.
    if state.h1_seen.saturating_sub(setup.armed_at) > p.mss_window {
        state.armed = None;
        return None;
    }
    f.not_expired += 1;

    let bias = state.aligned_bias()?;
    // The structural setup and both higher timeframes must point the same way.
    if bias != setup.direction {
        return None;
    }
    f.bias_aligned += 1;

    if state.m15.len() < 3 {
        return None;
    }
    let c1 = state.m15.front()?;
    let c3 = state.m15.back()?;
    let fvg = find_fvg(c1, c3, setup.direction)?;
    f.fvg_found += 1;

    let entry_price = fvg_entry_price(&fvg, setup.direction, p.fvg_entry_fraction);
    let buffer = atr * p.stop_buffer_atr;
    let stop_price = match setup.direction {
        Direction::Bullish => setup.sweep_extreme - buffer,
        Direction::Bearish => setup.sweep_extreme + buffer,
    };

    // A stop on the wrong side of the entry cannot be sized, and would mean
    // price had already invalidated the setup before the gap formed.
    let valid = match setup.direction {
        Direction::Bullish => stop_price < entry_price,
        Direction::Bearish => stop_price > entry_price,
    };
    if !valid || entry_price <= Decimal::ZERO || stop_price <= Decimal::ZERO {
        return None;
    }

    // Consumed: one setup produces at most one entry, so a lingering gap
    // cannot fire repeatedly and flood the daily cap.
    state.armed = None;

    let risk = (entry_price - stop_price).abs();
    let target_price = match setup.direction {
        Direction::Bullish => entry_price + risk * p.reward_multiple,
        Direction::Bearish => entry_price - risk * p.reward_multiple,
    };
    if target_price <= Decimal::ZERO {
        return None;
    }

    f.signals += 1;
    Some(Signal {
        symbol: ctx.symbol.clone(),
        side: setup.direction.side(),
        entry_price,
        stop_price,
        target_price,
        atr,
        signal_candle_open_ms: candle.open_time_ms,
    })
}
