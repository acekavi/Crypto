//! A seeded random-entry benchmark.
//!
//! Two very different claims look identical in a single equity curve:
//!   - "my entry signal predicts price" — a real edge, and
//!   - "1:2 R:R with a 1% risk cap makes money on any entry" — risk
//!     management flattering noise.
//!
//! This isolates the first by holding everything else constant. Sizing,
//! exits, universe, position caps, daily caps, fees and funding are identical
//! to the strategy under test; the ONLY differences are when an entry fires
//! and which way it points.

use std::collections::HashMap;

use botcore::{Side, Timeframe};
use history::HistoryDb;
use indicators::Atr;
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use risk::{RiskManager, RiskParams};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use strategy::pullback::StrategyParams;
use strategy::{MarketContext, Signal, Strategy};

use crate::metrics::compute;
use crate::replay::{BacktestConfig, BacktestError, run_backtest};

/// Probabilities are compared as scaled integers rather than floats, so no
/// `f64` enters a crate whose results must be exactly reproducible.
const PROBABILITY_SCALE: u64 = 1_000_000;

pub struct RandomEntryStrategy {
    rng: StdRng,
    timeframes: Vec<Timeframe>,
    warmup: usize,
    /// `signal_probability * PROBABILITY_SCALE`, precomputed.
    threshold: u64,
    atr_stop_multiple: Decimal,
    reward_multiple: Decimal,
    atr_period: usize,
    atr_by_symbol: HashMap<String, Atr>,
}

impl RandomEntryStrategy {
    /// `params` supplies the ATR period, stop multiple and reward multiple, so
    /// the benchmark sizes and exits its trades exactly as the strategy under
    /// test does. A benchmark with a different stop distance would be
    /// measuring position sizing, not entry quality.
    pub fn new(
        seed: u64,
        signal_probability: Decimal,
        timeframes: Vec<Timeframe>,
        warmup: usize,
        params: &StrategyParams,
    ) -> Self {
        let scaled = signal_probability * Decimal::from(PROBABILITY_SCALE);
        RandomEntryStrategy {
            rng: StdRng::seed_from_u64(seed),
            timeframes,
            warmup,
            threshold: scaled.to_u64().unwrap_or(0).min(PROBABILITY_SCALE),
            atr_stop_multiple: params.atr_stop_multiple,
            reward_multiple: params.reward_multiple,
            atr_period: params.atr_period,
            atr_by_symbol: HashMap::new(),
        }
    }
}

impl Strategy for RandomEntryStrategy {
    fn timeframes(&self) -> &[Timeframe] {
        &self.timeframes
    }

    fn warmup_candles(&self) -> usize {
        self.warmup
    }

    fn on_candle_close(&mut self, ctx: &MarketContext) -> Option<Signal> {
        // Entries are timed on 1h exactly as `PullbackStrategy` times them.
        // Letting the benchmark also fire on H4 would give it more chances to
        // enter than the strategy it is being compared against.
        if ctx.timeframe != Timeframe::H1 {
            return None;
        }

        let period = self.atr_period;
        let atr = self
            .atr_by_symbol
            .entry(ctx.symbol.as_str().to_string())
            .or_insert_with(|| Atr::new(period))
            .update(ctx.candle)?;
        if atr.is_zero() {
            // A zero-width stop would divide by zero in sizing.
            return None;
        }

        // Draw the entry decision BEFORE the side, and draw unconditionally,
        // so the RNG advances the same way whatever the outcome. A draw made
        // only on some branches would make the sequence depend on the data
        // and break seed reproducibility.
        let roll = self.rng.random_range(0..PROBABILITY_SCALE);
        let side = if self.rng.random_range(0..2u8) == 0 {
            Side::Buy
        } else {
            Side::Sell
        };
        if roll >= self.threshold {
            return None;
        }

        // Both sides are drawn: a long-only benchmark would measure a
        // market-direction bias rather than an entry edge.
        let entry_price = ctx.candle.close;
        let risk = atr * self.atr_stop_multiple;
        let (stop_price, target_price) = match side {
            Side::Buy => (
                entry_price - risk,
                entry_price + risk * self.reward_multiple,
            ),
            Side::Sell => (
                entry_price + risk,
                entry_price - risk * self.reward_multiple,
            ),
        };
        if stop_price <= Decimal::ZERO || target_price <= Decimal::ZERO {
            return None;
        }

        Some(Signal {
            symbol: ctx.symbol.clone(),
            side,
            entry_price,
            stop_price,
            target_price,
            atr,
            signal_candle_open_ms: ctx.candle.open_time_ms,
            breakeven_at_r: None,
        })
    }
}

/// One expectancy per seed, in seed order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BenchmarkDistribution {
    /// Recorded so a reported percentile can be reproduced exactly. An
    /// unreproducible benchmark is not evidence.
    pub seeds: Vec<u64>,
    pub expectancies: Vec<Decimal>,
}

impl BenchmarkDistribution {
    /// Expectancies sorted ascending — the input `percentile` expects.
    pub fn sorted_expectancies(&self) -> Vec<Decimal> {
        let mut v = self.expectancies.clone();
        v.sort();
        v
    }
}

/// Nearest-rank percentile: the value at rank `ceil(p/100 * n)`, 1-indexed.
///
/// Nearest-rank rather than an interpolating variant because interpolation
/// would invent a value that no run actually produced, and because the
/// arithmetic stays in `Decimal` with no float rounding to reason about. With
/// 100 seeds the 95th percentile is simply the 95th smallest run.
///
/// `sorted` must be ascending; an empty slice yields zero.
pub fn percentile(sorted: &[Decimal], p: Decimal) -> Decimal {
    if sorted.is_empty() {
        return Decimal::ZERO;
    }
    let n = Decimal::from(sorted.len());
    let rank = (p / Decimal::ONE_HUNDRED * n).ceil();
    let rank = rank.to_usize().unwrap_or(1).max(1).min(sorted.len());
    sorted[rank - 1]
}

/// Run one backtest per seed and collect the expectancies.
///
/// Every run uses the same config, the same risk envelope and the same cost
/// model as the strategy under test — only the entries differ.
#[allow(clippy::too_many_arguments)]
pub async fn run_benchmark(
    db: &HistoryDb,
    cfg: &BacktestConfig,
    risk_params: &RiskParams,
    stop_limit_offset_atr: Decimal,
    seeds: &[u64],
    signal_probability: Decimal,
    params: &StrategyParams,
    timeframes: Vec<Timeframe>,
    warmup: usize,
) -> Result<BenchmarkDistribution, BacktestError> {
    let mut expectancies = Vec::with_capacity(seeds.len());
    for seed in seeds {
        let run = run_backtest(
            db,
            cfg,
            Box::new(RandomEntryStrategy::new(
                *seed,
                signal_probability,
                timeframes.clone(),
                warmup,
                params,
            )),
            RiskManager::new(risk_params.clone(), stop_limit_offset_atr),
        )
        .await?;
        expectancies.push(compute(&run.trades, cfg.starting_equity).expectancy);
    }
    Ok(BenchmarkDistribution {
        seeds: seeds.to_vec(),
        expectancies,
    })
}
