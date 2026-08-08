//! Chronological replay driver over the live `EngineLoop`.
//!
//! This is deliberately thin. Everything that decides whether a trade
//! happens — the strategy, `RiskManager`'s caps and sizing, the executor —
//! is the exact code `bot::engine_loop::EngineLoop` runs live. This module's
//! only job is to hand it candles in the order they actually occurred and to
//! settle fills against them via `SimulatedExchange`.

use std::collections::HashMap;
use std::sync::Arc;

use bot::engine_loop::EngineLoop;
use botcore::{Candle, Instrument, Symbol, Timeframe};
use exchange::ExchangeClient;
use exchange::bybit::wire::FundingRate;
use history::{HistoryDb, find_gaps};
use persistence::Journal;
use risk::{HaltPolicy, RiskManager};
use rust_decimal::Decimal;
use strategy::Strategy;

use crate::costs::CostModel;
use crate::sim_exchange::{ClosedTrade, SimulatedExchange};

/// Everything one `run_backtest` call needs beyond the strategy and risk
/// rules, which are supplied separately so the same config can be replayed
/// against different rule sets.
///
/// `instruments` and `entry_expiry_candles` are not part of the fill or cost
/// model established in Tasks 1-3, but both `SimulatedExchange::new` and
/// `EngineLoop::new` require them (tick size/qty step for sizing, and how
/// long a resting entry waits before expiring) — there is nowhere else for a
/// replay driver to source them from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BacktestConfig {
    pub start_ms: i64,
    pub end_ms: i64,
    pub starting_equity: Decimal,
    pub symbols: Vec<Symbol>,
    pub instruments: Vec<Instrument>,
    pub costs: CostModel,
    pub warmup_candles: usize,
    pub entry_expiry_candles: u32,
    /// Pull the stop to entry once price travels this many R in favour.
    /// `None` leaves stops where the strategy placed them.
    pub breakeven_at_r: Option<Decimal>,
}

#[derive(Debug, thiserror::Error)]
pub enum BacktestError {
    #[error("history error: {0}")]
    History(String),
    #[error("engine error: {0}")]
    Engine(String),
    /// A named hole rather than a bare string so the caller can report
    /// exactly which symbol and span stopped the run, instead of parsing a
    /// message to find out.
    #[error("gap in stored data for {symbol}: candles missing between {from_ms} and {to_ms}")]
    GapInData {
        symbol: Symbol,
        from_ms: i64,
        to_ms: i64,
    },
}

impl From<history::HistoryError> for BacktestError {
    fn from(e: history::HistoryError) -> Self {
        BacktestError::History(e.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BacktestResult {
    /// Drawdown breaches observed. Under the backtest's `RecordOnly` policy
    /// these did NOT stop trading — they are reported so a run that spends
    /// time past its risk limit cannot look identical to one that never did.
    pub halt_events: usize,
    pub trades: Vec<ClosedTrade>,
    pub final_equity: Decimal,
    pub ambiguous_exits: usize,
    pub candles_replayed: usize,
}

/// One unit of the merged chronological stream: one symbol's candle on one
/// of the strategy's declared timeframes.
struct ReplayTick {
    symbol: Symbol,
    tf: Timeframe,
    candle: Candle,
}

/// Ranks timeframes finest-first, purely as a deterministic tie-break for two
/// ticks of the SAME symbol that land on the same `open_time_ms` (an H1
/// candle and an H4 candle can both open at midnight). Ordering by symbol
/// name alone, as the plan requires for cross-symbol ties, does not resolve
/// that case.
fn timeframe_rank(tf: Timeframe) -> u8 {
    match tf {
        Timeframe::H1 => 2,
        Timeframe::M5 => 0,
        Timeframe::M15 => 1,
        Timeframe::H4 => 3,
        Timeframe::D1 => 4,
    }
}

/// Replay `[cfg.start_ms, cfg.end_ms]` through `EngineLoop` exactly as the
/// live bot would see it, and settle every fill against `SimulatedExchange`.
///
/// # Chronological ordering
///
/// Every symbol's candles, on every timeframe the strategy declares, are
/// merged into ONE stream ordered by `open_time_ms` (ties broken by symbol
/// name, then by `timeframe_rank`). Replaying symbol-by-symbol would let the
/// engine see one symbol's entire future before another's first candle, and
/// the position cap and daily-entry cap — both global — would then be
/// enforced against a timeline that never existed.
///
/// # Settlement only on the finest timeframe
///
/// `SimulatedExchange::advance` is called only for ticks on the strategy's
/// finest declared timeframe (H1, when H1 is declared). An H4 candle's
/// high/low span the same price action four H1 candles already covered;
/// settling fills against it a second time would double-count that action,
/// and because the H4 candle's `open_time_ms` coincides with its first H1
/// constituent's, settling it would price a move (the rest of that 4h
/// window) that, at this point in the global stream, has not happened yet —
/// exactly the look-ahead bias the fill model exists to prevent. The engine
/// still receives every declared timeframe via `on_candle_closed`, matching
/// what the live strategy sees.
///
/// # Gaps
///
/// A hole in the stored candle series for any (symbol, timeframe) refuses
/// the whole run via `BacktestError::GapInData`: continuing would price a
/// move through the missing span as if it happened continuously.
pub async fn run_backtest(
    db: &HistoryDb,
    cfg: &BacktestConfig,
    strategy: Box<dyn Strategy>,
    risk: RiskManager,
) -> Result<BacktestResult, BacktestError> {
    let timeframes = strategy.timeframes().to_vec();
    let strategy_warmup = strategy.warmup_candles();
    let finest = timeframes.iter().copied().min_by_key(|tf| tf.duration_ms());

    let sim = Arc::new(SimulatedExchange::with_breakeven(
        cfg.starting_equity,
        cfg.instruments.clone(),
        cfg.costs,
        cfg.breakeven_at_r,
    ));
    let client: Arc<dyn ExchangeClient> = sim.clone();

    // Never the live trading journal: a throwaway in-memory database means a
    // backtest run can leave no trace in the real audit trail the live bot's
    // daily-fill cap reads from, however this run ends.
    let journal = Arc::new(
        Journal::open_local(":memory:")
            .await
            .map_err(|e| BacktestError::Engine(format!("opening the backtest journal: {e}")))?,
    );

    let mut engine = EngineLoop::new(
        strategy,
        // Measurement, not operation: a latched halt would truncate the very
        // sample the trade-count threshold demands. Halts are counted and
        // reported instead. Live trading is unaffected — `HaltPolicy::Enforce`
        // remains the default everywhere else.
        risk.with_halt_policy(HaltPolicy::RecordOnly),
        client,
        journal,
        cfg.instruments.clone(),
        // Attributes journal rows to a ruleset in live trading; a backtest's
        // temporary journal is discarded at the end of this call, so the
        // exact value carries no meaning here.
        "backtest".to_string(),
        cfg.entry_expiry_candles,
        cfg.warmup_candles,
    );

    // Enough to satisfy BOTH the candle store's window and the strategy's own
    // indicators, whichever is longer.
    let warmup_candles_needed = cfg.warmup_candles.max(strategy_warmup);

    let mut ticks: Vec<ReplayTick> = Vec::new();
    let mut funding_by_symbol: HashMap<Symbol, Vec<FundingRate>> = HashMap::new();

    for symbol in &cfg.symbols {
        for &tf in &timeframes {
            let candles = db
                .candles_in_range(symbol, tf, cfg.start_ms, cfg.end_ms)
                .await?;

            if let Some(gap) = find_gaps(&candles, tf).into_iter().next() {
                return Err(BacktestError::GapInData {
                    symbol: symbol.clone(),
                    from_ms: gap.from_ms,
                    to_ms: gap.to_ms,
                });
            }

            // Warm from history BEFORE the measured window, exactly as
            // main.rs warms from `klines()` before it starts streaming.
            //
            // This previously seeded an empty stream and let the window warm
            // itself, which quietly made short windows unmeasurable: with a
            // 200-period EMA on 4h the strategy produced nothing for its first
            // ~75 days, so a 60-day out-of-sample window could never contain a
            // single trade and reported zero as though that were a result.
            // Warm-up candles are drawn from OUTSIDE `[start_ms, end_ms)`, so
            // they inform indicators without becoming tradeable bars.
            let warm_span = warmup_candles_needed as i64 * tf.duration_ms();
            let warm = db
                .candles_in_range(symbol, tf, cfg.start_ms - warm_span, cfg.start_ms - 1)
                .await?;
            engine.warm(symbol, tf, warm);

            for candle in candles {
                ticks.push(ReplayTick {
                    symbol: symbol.clone(),
                    tf,
                    candle,
                });
            }
        }

        let funding = db
            .funding_in_range(symbol, cfg.start_ms, cfg.end_ms)
            .await?;
        funding_by_symbol.insert(symbol.clone(), funding);
    }

    ticks.sort_by(|a, b| {
        a.candle
            .open_time_ms
            .cmp(&b.candle.open_time_ms)
            .then_with(|| a.symbol.cmp(&b.symbol))
            .then_with(|| timeframe_rank(a.tf).cmp(&timeframe_rank(b.tf)))
    });

    engine
        .load_baselines(cfg.start_ms)
        .await
        .map_err(|e| BacktestError::Engine(e.to_string()))?;

    let candles_replayed = ticks.len();
    let no_funding: Vec<FundingRate> = Vec::new();

    for tick in &ticks {
        if Some(tick.tf) == finest {
            let funding = funding_by_symbol.get(&tick.symbol).unwrap_or(&no_funding);
            sim.advance(&tick.symbol, &tick.candle, funding);
        }

        engine
            .on_candle_closed(&tick.symbol, tick.tf, &tick.candle)
            .await
            .map_err(|e| BacktestError::Engine(e.to_string()))?;

        // A no-op in practice: `SimulatedExchange::tickers` always returns
        // empty (see its doc comment), so the ladder never finds a last
        // price and never acts. Called anyway to match the live per-candle
        // sequence exactly — see the module doc for why the trigger case it
        // guards cannot actually arise against this fill model.
        engine
            .drive_stop_escalation(tick.candle.close_time_ms(tick.tf))
            .await
            .map_err(|e| BacktestError::Engine(e.to_string()))?;
    }

    let balance = client_balance(&sim).await?;
    let trades = sim.closed_trades();
    let ambiguous_exits = trades.iter().filter(|t| t.was_ambiguous).count();

    Ok(BacktestResult {
        trades,
        final_equity: balance,
        ambiguous_exits,
        candles_replayed,
        halt_events: engine.halt_events(),
    })
}

/// `SimulatedExchange::balance` is only reachable through the `ExchangeClient`
/// trait (it is the trait's method, not an inherent one), so this needs the
/// trait in scope; factored out purely to keep that import call site small.
async fn client_balance(sim: &Arc<SimulatedExchange>) -> Result<Decimal, BacktestError> {
    let bal = sim
        .balance()
        .await
        .map_err(|e| BacktestError::Engine(e.to_string()))?;
    Ok(bal.equity)
}
