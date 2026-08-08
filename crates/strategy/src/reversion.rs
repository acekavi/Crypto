//! Mean reversion: fade sharp displacement from a short-term mean, but only
//! while the higher timeframe is ranging.
//!
//! Pre-registered in
//! `docs/superpowers/specs/2026-08-07-mean-reversion-study-design.md` before
//! any run. The hypothesis, every variant, the selection rule and the pass bar
//! were fixed there first, so nothing here may be tuned to flatter a result.
//!
//! This is deliberately NOT a sign-flipped `PullbackStrategy`. That one enters
//! when a pullback ENDS and bets on continuation; this enters when
//! displacement is EXTREME and bets on contraction. Different trigger,
//! different stop geometry, different reason to expect an edge.

use std::collections::HashMap;

use botcore::{Candle, Side, Symbol, Timeframe};
use indicators::{Atr, Ema};
use rust_decimal::Decimal;

use crate::signal::{MarketContext, Signal};
use crate::traits::Strategy;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReversionParams {
    /// H4 trend filter, used only to decide whether the market is ranging.
    pub ema_fast: usize,
    pub ema_slow: usize,
    /// Max `|EMA50 - EMA200| / EMA200` at which entries are permitted.
    ///
    /// IN-SAMPLE DERIVED, and the spec says so plainly: it comes from the
    /// failed trend strategy's post-mortem, where the weakest-trend quartile
    /// (strength < 0.0208) had the best profit factor. Kept because
    /// reversion-works-in-ranges is a standard mechanism rather than something
    /// discovered here — but a holdout pass reads as "survived a filter chosen
    /// with hindsight", not "discovered one".
    pub range_max: Decimal,
    /// H1 mean the stretch is measured against.
    pub ema_entry: usize,
    pub atr_period: usize,
    /// How far from the mean, in ATRs, before a setup exists.
    pub stretch_atr: Decimal,
    /// Limit placed this many ATRs FURTHER into the stretch, so the entry
    /// improves rather than chasing.
    pub entry_offset_atr: Decimal,
    /// Stop distance beyond the entry, in ATRs.
    pub stop_atr: Decimal,
}

impl ReversionParams {
    /// Variant A — the study's PRIMARY, named before any result so that
    /// "the primary passed" cannot later be redefined.
    pub fn variant_a() -> Self {
        ReversionParams {
            ema_fast: 50,
            ema_slow: 200,
            range_max: Decimal::new(2, 2), // 0.02
            ema_entry: 20,
            atr_period: 14,
            stretch_atr: Decimal::new(30, 1),      // 3.0
            entry_offset_atr: Decimal::new(25, 2), // 0.25
            stop_atr: Decimal::ONE,
        }
    }

    /// The six pre-declared variants, in the spec's order. No variant may be
    /// added, removed or altered after the first run.
    pub fn declared_variants() -> Vec<(&'static str, Self)> {
        let v = |stretch: Decimal, stop: Decimal| ReversionParams {
            stretch_atr: stretch,
            stop_atr: stop,
            ..Self::variant_a()
        };
        vec![
            ("A", v(Decimal::new(30, 1), Decimal::new(10, 1))),
            ("B", v(Decimal::new(25, 1), Decimal::new(8, 1))),
            ("C", v(Decimal::new(35, 1), Decimal::new(12, 1))),
            ("D", v(Decimal::new(30, 1), Decimal::new(8, 1))),
            ("E", v(Decimal::new(40, 1), Decimal::new(12, 1))),
            ("F", v(Decimal::new(25, 1), Decimal::new(6, 1))),
        ]
    }

    /// Whether a 2R target lands BEFORE the mean.
    ///
    /// A reversion target sitting past the mean would need price to overshoot
    /// the very level it is reverting to, which is not the hypothesis being
    /// tested. `stop_limit_offset_atr` is the live 0.3 ATR offset the risk
    /// manager adds beyond the stop trigger, and it counts toward risk because
    /// that is where a stop actually fills.
    pub fn target_lands_before_mean(&self, stop_limit_offset_atr: Decimal) -> bool {
        let risk = self.stop_atr + stop_limit_offset_atr;
        risk * Decimal::TWO <= self.stretch_atr
    }
}

/// Incremental state for one symbol.
struct SymbolState {
    ema_fast_h4: Ema,
    ema_slow_h4: Ema,
    ema_entry_h1: Ema,
    atr_h1: Atr,
}

impl SymbolState {
    fn new(p: &ReversionParams) -> Self {
        SymbolState {
            ema_fast_h4: Ema::new(p.ema_fast),
            ema_slow_h4: Ema::new(p.ema_slow),
            ema_entry_h1: Ema::new(p.ema_entry),
            atr_h1: Atr::new(p.atr_period),
        }
    }

    /// `|fast - slow| / slow`, or `None` until both EMAs are warm.
    ///
    /// Normalised by the slow EMA so one threshold works across BTC at $70,000
    /// and DOGE at $0.16.
    fn trend_strength(&self) -> Option<Decimal> {
        match (self.ema_fast_h4.value(), self.ema_slow_h4.value()) {
            (Some(fast), Some(slow)) if slow > Decimal::ZERO => Some((fast - slow).abs() / slow),
            _ => None,
        }
    }
}

pub struct ReversionStrategy {
    params: ReversionParams,
    per_symbol: HashMap<Symbol, SymbolState>,
    timeframes: Vec<Timeframe>,
}

impl ReversionStrategy {
    pub fn new(params: ReversionParams) -> Self {
        ReversionStrategy {
            params,
            per_symbol: HashMap::new(),
            timeframes: vec![Timeframe::H1, Timeframe::H4],
        }
    }
}

impl Strategy for ReversionStrategy {
    fn timeframes(&self) -> &[Timeframe] {
        &self.timeframes
    }

    fn warmup_candles(&self) -> usize {
        // The 200-period H4 EMA is the binding constraint, with margin so the
        // smoothed value is meaningful rather than merely defined.
        self.params.ema_slow + 50
    }

    fn on_candle_close(&mut self, ctx: &MarketContext) -> Option<Signal> {
        let params = self.params.clone();
        let state = self
            .per_symbol
            .entry(ctx.symbol.clone())
            .or_insert_with(|| SymbolState::new(&params));

        match ctx.timeframe {
            // 4h only advances the regime filter. Entries are always timed on
            // 1h, matching how the rest of the engine is wired.
            Timeframe::H4 => {
                state.ema_fast_h4.update(ctx.candle.close);
                state.ema_slow_h4.update(ctx.candle.close);
                None
            }
            Timeframe::H1 => evaluate_h1(&params, state, ctx.candle, ctx),
            // Never subscribed to by this strategy — `timeframes()` declares
            // only H1 and H4 — so nothing should route here. Named explicitly
            // rather than caught by `_` so that adding a timeframe forces this
            // decision again instead of silently defaulting to "ignore".
            Timeframe::M5 | Timeframe::M15 | Timeframe::D1 => None,
        }
    }
}

fn evaluate_h1(
    params: &ReversionParams,
    state: &mut SymbolState,
    candle: &Candle,
    ctx: &MarketContext,
) -> Option<Signal> {
    // Advance indicators first so every value below describes THIS candle.
    let ema20 = state.ema_entry_h1.update(candle.close);
    let atr = state.atr_h1.update(candle);

    let ema20 = ema20?;
    let atr = atr?;
    if atr <= Decimal::ZERO {
        // A zero-width ATR gives a zero-width stop, which cannot be sized.
        return None;
    }

    // Refuse outright in a trending market. Fading displacement while a trend
    // is working is the falling-knife loss this gate exists to avoid, and it
    // is the clause that separates this hypothesis from "buy anything that
    // fell".
    let strength = state.trend_strength()?;
    if strength >= params.range_max {
        return None;
    }

    let stretch = (candle.close - ema20) / atr;
    let side = if stretch <= -params.stretch_atr {
        Side::Buy
    } else if stretch >= params.stretch_atr {
        Side::Sell
    } else {
        return None;
    };

    // Placed FURTHER into the stretch, not at the close: a limit that improves
    // the entry rather than chasing it. Requires price to extend a little more
    // before filling, which the limit-only rule makes natural.
    let offset = params.entry_offset_atr * atr;
    let stop_distance = params.stop_atr * atr;
    let (entry_price, stop_price) = match side {
        Side::Buy => (candle.close - offset, candle.close - offset - stop_distance),
        Side::Sell => (candle.close + offset, candle.close + offset + stop_distance),
    };

    // A deep stretch on a low-priced instrument can push either price through
    // zero. Refuse rather than emit it — the risk manager would reject it
    // anyway, and a negative price must never reach an exchange.
    if entry_price <= Decimal::ZERO || stop_price <= Decimal::ZERO {
        return None;
    }

    // `target_price` is advisory only: `RiskManager` recomputes the real
    // target as 2R from the entry-to-stop-limit distance, which is the
    // corrected definition. Filled in consistently so the signal is coherent
    // on its own.
    let target_price = match side {
        Side::Buy => entry_price + stop_distance * Decimal::TWO,
        Side::Sell => entry_price - stop_distance * Decimal::TWO,
    };
    if target_price <= Decimal::ZERO {
        return None;
    }

    Some(Signal {
        symbol: ctx.symbol.clone(),
        side,
        entry_price,
        stop_price,
        target_price,
        atr,
        signal_candle_open_ms: candle.open_time_ms,
    })
}
