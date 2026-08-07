//! Rolling walk-forward evaluation.
//!
//! Parameters are chosen on an in-sample window and then measured on the
//! window that follows, with the choice frozen. Only the concatenated
//! out-of-sample results are evidence — in-sample figures are kept per fold
//! for diagnosis and must never reach the gate. A single in-sample number is
//! how a curve-fit strategy gets deployed.

use rust_decimal::Decimal;
use strategy::Strategy;

use history::HistoryDb;
use risk::{RiskManager, RiskParams};

use crate::metrics::{Metrics, compute};
use crate::replay::{BacktestConfig, BacktestError, run_backtest};
use crate::sim_exchange::ClosedTrade;

const DAY_MS: i64 = 86_400_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalkForwardConfig {
    pub in_sample_ms: i64,
    pub out_of_sample_ms: i64,
}

impl WalkForwardConfig {
    /// The approved spec's windows: six months to tune, the following two
    /// months to measure. Months are 30 days — calendar months would make
    /// fold lengths vary, and a fold that is 3% longer is not comparable to
    /// its neighbours.
    pub fn defaults() -> Self {
        WalkForwardConfig {
            in_sample_ms: 180 * DAY_MS,
            out_of_sample_ms: 60 * DAY_MS,
        }
    }
}

/// One in-sample window and the out-of-sample window immediately after it.
///
/// Both bounds are half-open (`start` inclusive, `end` exclusive) so
/// consecutive folds tile the range without a candle landing in two windows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fold {
    pub index: usize,
    pub is_start_ms: i64,
    pub is_end_ms: i64,
    pub oos_start_ms: i64,
    pub oos_end_ms: i64,
}

/// Generic over the parameter type so a study can supply its own, rather than
/// every strategy being forced through one crate's params struct.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoldResult<P> {
    pub fold: Fold,
    pub chosen_params: P,
    pub is_metrics: Metrics,
    pub oos_metrics: Metrics,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalkForwardResult<P> {
    pub folds: Vec<FoldResult<P>>,
    /// Every fold's out-of-sample trades, concatenated. The only evidence.
    pub oos_trades: Vec<ClosedTrade>,
    pub oos_metrics: Metrics,
}

/// Slice `[start_ms, end_ms)` into rolling folds.
///
/// Windows advance by the OUT-OF-SAMPLE length, so out-of-sample periods are
/// contiguous and never overlap. Overlapping them would count the same trades
/// more than once and inflate the very trade count the gate checks.
///
/// A fold is emitted only when its full in-sample AND out-of-sample windows
/// fit inside the range: a truncated final fold is a shorter, noisier sample
/// masquerading as a complete one.
pub fn folds(start_ms: i64, end_ms: i64, wf: &WalkForwardConfig) -> Vec<Fold> {
    let mut out = Vec::new();
    if wf.in_sample_ms <= 0 || wf.out_of_sample_ms <= 0 {
        return out;
    }

    let mut is_start = start_ms;
    let mut index = 0;
    loop {
        let is_end = is_start + wf.in_sample_ms;
        let oos_end = is_end + wf.out_of_sample_ms;
        if oos_end > end_ms {
            break;
        }
        out.push(Fold {
            index,
            is_start_ms: is_start,
            is_end_ms: is_end,
            oos_start_ms: is_end,
            oos_end_ms: oos_end,
        });
        index += 1;
        is_start += wf.out_of_sample_ms;
    }
    out
}

/// Run every fold, tuning on in-sample and measuring on out-of-sample.
///
/// `make_strategy` builds a fresh strategy per run — strategies carry
/// per-symbol indicator state, so reusing one across windows would leak the
/// in-sample warm-up into the out-of-sample measurement.
///
/// `grid` is a small, explicitly declared parameter list. There is
/// deliberately no search algorithm: the spec puts optimisation beyond a
/// declared grid out of scope, because that is how overfitting gets
/// industrialised.
pub async fn run_walk_forward<P: Clone>(
    db: &HistoryDb,
    cfg: &BacktestConfig,
    wf: &WalkForwardConfig,
    grid: &[P],
    risk_params: &RiskParams,
    stop_limit_offset_atr: Decimal,
    make_strategy: &dyn Fn(&P) -> Box<dyn Strategy>,
) -> Result<WalkForwardResult<P>, BacktestError> {
    let mut fold_results = Vec::new();
    let mut oos_trades: Vec<ClosedTrade> = Vec::new();

    for fold in folds(cfg.start_ms, cfg.end_ms, wf) {
        // --- Tune on in-sample only ---
        let mut best: Option<(P, Metrics)> = None;
        for params in grid {
            let is_cfg = BacktestConfig {
                start_ms: fold.is_start_ms,
                end_ms: fold.is_end_ms,
                ..cfg.clone()
            };
            let run = run_backtest(
                db,
                &is_cfg,
                make_strategy(params),
                RiskManager::new(risk_params.clone(), stop_limit_offset_atr),
            )
            .await?;
            let m = compute(&run.trades, cfg.starting_equity);

            // Strictly greater, so a tie keeps the EARLIER grid entry and the
            // choice depends only on the grid's declared order rather than on
            // iteration incidentals.
            let better = match &best {
                None => true,
                Some((_, best_m)) => m.expectancy > best_m.expectancy,
            };
            if better {
                best = Some((params.clone(), m));
            }
        }

        let Some((chosen_params, is_metrics)) = best else {
            // An empty grid has nothing to choose; skip rather than invent a
            // default that was never declared.
            continue;
        };

        // --- Measure on out-of-sample with that choice frozen ---
        let oos_cfg = BacktestConfig {
            start_ms: fold.oos_start_ms,
            end_ms: fold.oos_end_ms,
            ..cfg.clone()
        };
        let oos_run = run_backtest(
            db,
            &oos_cfg,
            make_strategy(&chosen_params),
            RiskManager::new(risk_params.clone(), stop_limit_offset_atr),
        )
        .await?;
        let oos_metrics = compute(&oos_run.trades, cfg.starting_equity);

        oos_trades.extend(oos_run.trades);
        fold_results.push(FoldResult {
            fold,
            chosen_params,
            is_metrics,
            oos_metrics,
        });
    }

    // Computed over the concatenated out-of-sample trades — never over any
    // in-sample run, and never over the two mixed together.
    let mut oos_metrics = compute(&oos_trades, cfg.starting_equity);

    // Drawdown is the one metric that CANNOT be read off the concatenation.
    // `compute` walks every fold's trades onto a single starting equity, but
    // each fold actually began at that equity independently, so a losing run
    // drives the synthetic curve below zero and reports impossible figures —
    // 137% and 218% were both produced this way before this fix.
    //
    // The honest question the 15% limit asks is "did the account ever fall
    // more than 15% from its peak", and each fold IS a real account
    // trajectory. So the worst single fold is the answer; the concatenated
    // figure is replaced rather than reported alongside, because a number
    // that cannot be true has no business in the output at all.
    oos_metrics.max_drawdown_pct = fold_results
        .iter()
        .map(|f| f.oos_metrics.max_drawdown_pct)
        .max()
        .unwrap_or(Decimal::ZERO);

    Ok(WalkForwardResult {
        folds: fold_results,
        oos_trades,
        oos_metrics,
    })
}
