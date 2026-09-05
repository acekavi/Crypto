use std::collections::HashMap;
use std::cmp::Ordering;
use std::str::FromStr;

use history::{HistoryDb, HistoryError};
use pairs::{PairParams, PairSide, RollingZ, SignalEngine, per_leg_notional, unrealized_pnl_fraction};
use rust_decimal::Decimal;
use serde::Serialize;

#[derive(Debug, thiserror::Error)]
pub enum PairBacktestError {
    #[error(transparent)]
    History(#[from] HistoryError),
    #[error("backtest data error: {0}")]
    Data(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct PairTrade {
    pub entry_ms: i64,
    pub exit_ms: i64,
    pub side: String,
    pub entry_z: f64,
    pub exit_z: f64,
    pub reason: String,
    pub net: f64,
    pub per_leg_notional: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct BacktestResult {
    pub pair: String,
    pub trades: usize,
    pub wins: usize,
    pub losses: usize,
    pub win_rate: f64,
    pub profit_factor: f64,
    pub net: f64,
    pub avg_trade: f64,
    pub max_drawdown_pct: f64,
    pub trades_detail: Vec<PairTrade>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CompactBacktestResult {
    pub trades: usize,
    pub wins: usize,
    pub losses: usize,
    pub win_rate: f64,
    pub profit_factor: f64,
    pub net: f64,
    pub avg_trade: f64,
    pub max_drawdown_pct: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct SplitBacktestSummary {
    pub pair: String,
    pub train: CompactBacktestResult,
    pub holdout: CompactBacktestResult,
    pub full: CompactBacktestResult,
    pub fixed_notional_usdt: String,
    pub fixed_notional_train: CompactBacktestResult,
    pub fixed_notional_holdout: CompactBacktestResult,
    pub fixed_notional_full: CompactBacktestResult,
    pub recent_trades: Vec<PairTrade>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ParameterAuditCandidate {
    pub label: String,
    pub rolling_window: usize,
    pub entry_z: f64,
    pub stop_z: f64,
    pub target_z: f64,
    pub max_hold_bars: i64,
    pub audit_score: f64,
    pub accepted: bool,
    pub rejection_reasons: Vec<String>,
    pub summary: SplitBacktestSummary,
}

#[derive(Debug, Clone, Serialize)]
pub struct ParameterAuditReport {
    pub pair: String,
    pub current_label: String,
    pub min_holdout_trades: usize,
    pub candidate_count: usize,
    pub accepted_count: usize,
    pub best_label: Option<String>,
    pub candidates: Vec<ParameterAuditCandidate>,
}

#[derive(Debug, Clone, Serialize)]
pub struct WalkForwardFoldWindow {
    pub index: usize,
    pub is_start_ms: i64,
    pub is_end_ms: i64,
    pub oos_start_ms: i64,
    pub oos_end_ms: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct WalkForwardAuditFoldSummary {
    pub fold: WalkForwardFoldWindow,
    pub chosen_label: String,
    pub in_sample_score: f64,
    pub oos_trades: usize,
    pub oos_profit_factor: f64,
    pub oos_fixed_profit_factor: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct WalkForwardCandidateScoreboardRow {
    pub label: String,
    pub folds_selected: usize,
    pub oos_trades: usize,
    pub oos_profit_factor: f64,
    pub oos_fixed_profit_factor: f64,
    pub oos_net: f64,
    pub oos_fixed_net: f64,
}

#[derive(Debug, Clone, Serialize)]
pub struct WalkForwardAuditReport {
    pub pair: String,
    pub candidate_pool_size: usize,
    pub folds: Vec<WalkForwardAuditFoldSummary>,
    pub scoreboard: Vec<WalkForwardCandidateScoreboardRow>,
    pub decisions: Vec<PromotionDecision>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PromotionVerdict {
    Promote,
    Watchlist,
    Reject,
}

impl PromotionVerdict {
    pub fn as_str(self) -> &'static str {
        match self {
            PromotionVerdict::Promote => "promote",
            PromotionVerdict::Watchlist => "watchlist",
            PromotionVerdict::Reject => "reject",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct PromotionDecision {
    pub verdict: PromotionVerdict,
    pub candidate: WalkForwardCandidateScoreboardRow,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone)]
struct SimPosition {
    side: PairSide,
    entry_ms: i64,
    entry_z: f64,
    a0: f64,
    b0: f64,
    age: i64,
    breakeven_armed: bool,
    per_leg_notional: f64,
}

fn dec_from_f64(v: f64, field: &str) -> Result<Decimal, PairBacktestError> {
    Decimal::from_str(&v.to_string())
        .map_err(|e| PairBacktestError::Data(format!("{field}: {e}")))
}

pub async fn load_pair_series_from_db(
    db_path: &str,
    params: &PairParams,
) -> Result<(Vec<i64>, Vec<f64>, Vec<f64>), PairBacktestError> {
    let db = HistoryDb::open_local(db_path).await?;
    let a_range = db
        .recorded_range(&params.leg_a, params.timeframe)
        .await?
        .ok_or_else(|| PairBacktestError::Data(format!("no data for {}", params.leg_a)))?;
    let b_range = db
        .recorded_range(&params.leg_b, params.timeframe)
        .await?
        .ok_or_else(|| PairBacktestError::Data(format!("no data for {}", params.leg_b)))?;
    let start = a_range.0.max(b_range.0);
    let end = a_range.1.min(b_range.1);
    if start > end {
        return Err(PairBacktestError::Data(format!(
            "{} and {} have no overlapping {} history",
            params.leg_a,
            params.leg_b,
            params.timeframe.as_bybit_interval()
        )));
    }

    let a_rows = db
        .candles_in_range(&params.leg_a, params.timeframe, start, end)
        .await?;
    let b_rows = db
        .candles_in_range(&params.leg_b, params.timeframe, start, end)
        .await?;

    let by_a: HashMap<i64, f64> = a_rows
        .into_iter()
        .map(|c| {
            let close = c.close.to_string().parse::<f64>().map_err(|e| {
                PairBacktestError::Data(format!("{} close parse: {e}", params.leg_a))
            })?;
            Ok((c.open_time_ms, close))
        })
        .collect::<Result<_, PairBacktestError>>()?;
    let by_b: HashMap<i64, f64> = b_rows
        .into_iter()
        .map(|c| {
            let close = c.close.to_string().parse::<f64>().map_err(|e| {
                PairBacktestError::Data(format!("{} close parse: {e}", params.leg_b))
            })?;
            Ok((c.open_time_ms, close))
        })
        .collect::<Result<_, PairBacktestError>>()?;

    let mut common: Vec<i64> = by_a
        .keys()
        .copied()
        .filter(|ms| by_b.contains_key(ms))
        .collect();
    common.sort_unstable();
    if common.is_empty() {
        return Err(PairBacktestError::Data(format!(
            "{} and {} share no common candles",
            params.leg_a, params.leg_b
        )));
    }
    let a = common.iter().map(|ms| by_a[ms]).collect();
    let b = common.iter().map(|ms| by_b[ms]).collect();
    Ok((common, a, b))
}

pub async fn run_backtest(
    db_path: &str,
    params: &PairParams,
    start_ms: Option<i64>,
    end_ms: Option<i64>,
) -> Result<BacktestResult, PairBacktestError> {
    let (times, a_prices, b_prices) = load_pair_series_from_db(db_path, params).await?;
    let spreads: Vec<f64> = a_prices
        .iter()
        .zip(&b_prices)
        .map(|(a, b)| a.ln() - b.ln())
        .collect();
    let engine = SignalEngine::new(params.clone());
    let mut rz = RollingZ::new(params.rolling_window);

    let mut wins = 0usize;
    let mut losses = 0usize;
    let mut gross_profit = 0.0;
    let mut gross_loss = 0.0;
    let mut equity = 1.0;
    let mut position: Option<SimPosition> = None;
    let mut trades = Vec::new();

    for i in 0..times.len() {
        let ms = times[i];
        if start_ms.is_some_and(|start| ms < start) {
            let _ = rz.push(spreads[i]);
            continue;
        }
        if end_ms.is_some_and(|end| ms >= end) {
            let _ = rz.push(spreads[i]);
            continue;
        }
        let Some(stats) = rz.push(spreads[i]) else {
            continue;
        };
        let z = stats.z;
        let sigma = stats.sd;

        if position.is_none() {
            let Some(sig) = engine.entry_signal(z) else {
                continue;
            };
            let per_leg = if params.risk_pct_of_equity > Decimal::ZERO {
                per_leg_notional(params, dec_from_f64(equity, "equity")?, dec_from_f64(equity, "equity")?, sigma)
                    .map_err(|e| PairBacktestError::Data(format!("sizing: {e}")))?
                    .notional
                    .to_string()
                    .parse::<f64>()
                    .map_err(|e| PairBacktestError::Data(format!("notional parse: {e}")))?
            } else {
                params
                    .per_leg_notional_usdt
                    .to_string()
                    .parse::<f64>()
                    .map_err(|e| PairBacktestError::Data(format!("notional parse: {e}")))?
            };
            position = Some(SimPosition {
                side: sig,
                entry_ms: ms,
                entry_z: z,
                a0: a_prices[i],
                b0: b_prices[i],
                age: 0,
                breakeven_armed: false,
                per_leg_notional: per_leg,
            });
            continue;
        }

        let pos = position.as_mut().expect("position exists");
        pos.age += 1;
        if !pos.breakeven_armed && engine.should_arm_breakeven(pos.side, z) {
            pos.breakeven_armed = true;
        }
        let pnl_fraction = unrealized_pnl_fraction(
            pos.side,
            dec_from_f64(pos.a0, "a0")?,
            dec_from_f64(pos.b0, "b0")?,
            dec_from_f64(a_prices[i], "a_now")?,
            dec_from_f64(b_prices[i], "b_now")?,
            params.fee_per_leg,
        )
        .to_string()
        .parse::<f64>()
        .map_err(|e| PairBacktestError::Data(format!("pnl fraction parse: {e}")))?;
        let reason = engine.exit_reason(
            pos.side,
            z,
            pos.age,
            pos.breakeven_armed,
            Some(dec_from_f64(pnl_fraction, "pnl_fraction")?),
        );
        let Some(reason) = reason else {
            continue;
        };

        let pnl = pnl_fraction * pos.per_leg_notional;
        equity += pnl;
        if pnl > 0.0 {
            wins += 1;
            gross_profit += pnl;
        } else if pnl < 0.0 {
            losses += 1;
            gross_loss += -pnl;
        }
        trades.push(PairTrade {
            entry_ms: pos.entry_ms,
            exit_ms: ms,
            side: pos.side.as_str().to_string(),
            entry_z: pos.entry_z,
            exit_z: z,
            reason: reason.as_str().to_string(),
            net: pnl,
            per_leg_notional: pos.per_leg_notional,
        });
        position = None;
    }

    let mut equity_curve = 1.0f64;
    let mut peak = 1.0f64;
    let mut max_dd = 0.0f64;
    for trade in &trades {
        equity_curve += trade.net;
        peak = peak.max(equity_curve);
        let dd = if peak > 0.0 {
            (peak - equity_curve) / peak
        } else {
            0.0
        };
        if dd > max_dd {
            max_dd = dd;
        }
    }

    let trade_count = trades.len();
    let net = equity - 1.0;
    Ok(BacktestResult {
        pair: format!("{}/{}", params.leg_a, params.leg_b),
        trades: trade_count,
        wins,
        losses,
        win_rate: if trade_count == 0 { 0.0 } else { wins as f64 / trade_count as f64 },
        profit_factor: if gross_loss > 0.0 {
            gross_profit / gross_loss
        } else if gross_profit > 0.0 {
            f64::INFINITY
        } else {
            0.0
        },
        net,
        avg_trade: if trade_count == 0 { 0.0 } else { net / trade_count as f64 },
        max_drawdown_pct: max_dd * 100.0,
        trades_detail: trades,
    })
}

pub fn compact(result: &BacktestResult) -> CompactBacktestResult {
    CompactBacktestResult {
        trades: result.trades,
        wins: result.wins,
        losses: result.losses,
        win_rate: result.win_rate,
        profit_factor: result.profit_factor,
        net: result.net,
        avg_trade: result.avg_trade,
        max_drawdown_pct: result.max_drawdown_pct,
    }
}

pub async fn split_summary(
    db_path: &str,
    params: &PairParams,
    split_pct: f64,
) -> Result<SplitBacktestSummary, PairBacktestError> {
    let (times, _, _) = load_pair_series_from_db(db_path, params).await?;
    if times.is_empty() {
        return Err(PairBacktestError::Data("no pair candles".into()));
    }
    let split_idx = ((times.len() as f64) * split_pct).floor() as usize;
    let safe_idx = split_idx.min(times.len().saturating_sub(1));
    let split_ms = times[safe_idx];
    let train = run_backtest(db_path, params, None, Some(split_ms)).await?;
    let hold = run_backtest(db_path, params, Some(split_ms), None).await?;
    let full = run_backtest(db_path, params, None, None).await?;

    let mut fixed = params.clone();
    fixed.risk_pct_of_equity = Decimal::ZERO;
    let fixed_train = run_backtest(db_path, &fixed, None, Some(split_ms)).await?;
    let fixed_hold = run_backtest(db_path, &fixed, Some(split_ms), None).await?;
    let fixed_full = run_backtest(db_path, &fixed, None, None).await?;

    Ok(SplitBacktestSummary {
        pair: full.pair.clone(),
        train: compact(&train),
        holdout: compact(&hold),
        full: compact(&full),
        fixed_notional_usdt: params.per_leg_notional_usdt.to_string(),
        fixed_notional_train: compact(&fixed_train),
        fixed_notional_holdout: compact(&fixed_hold),
        fixed_notional_full: compact(&fixed_full),
        recent_trades: full.trades_detail.iter().rev().take(8).cloned().collect::<Vec<_>>().into_iter().rev().collect(),
    })
}

fn parameter_label(params: &PairParams) -> String {
    format!(
        "w{}-e{:.2}-s{:.2}-t{:.2}-h{}",
        params.rolling_window, params.entry_z, params.stop_z, params.target_z, params.max_hold_bars
    )
}

pub fn evaluate_parameter_candidate(
    label: &str,
    params: &PairParams,
    summary: SplitBacktestSummary,
    min_holdout_trades: usize,
) -> ParameterAuditCandidate {
    let mut rejection_reasons = Vec::new();
    if summary.holdout.trades < min_holdout_trades {
        rejection_reasons.push(format!(
            "holdout trades {} below minimum {}",
            summary.holdout.trades, min_holdout_trades
        ));
    }
    if summary.holdout.profit_factor <= 1.0 {
        rejection_reasons.push(format!(
            "holdout PF {:.3} must be > 1.0",
            summary.holdout.profit_factor
        ));
    }
    if summary.fixed_notional_holdout.profit_factor <= 1.0 {
        rejection_reasons.push(format!(
            "fixed-notional holdout PF {:.3} must be > 1.0",
            summary.fixed_notional_holdout.profit_factor
        ));
    }
    if summary.full.profit_factor <= 1.0 {
        rejection_reasons.push(format!(
            "full PF {:.3} must be > 1.0",
            summary.full.profit_factor
        ));
    }
    if summary.fixed_notional_full.profit_factor <= 1.0 {
        rejection_reasons.push(format!(
            "fixed-notional full PF {:.3} must be > 1.0",
            summary.fixed_notional_full.profit_factor
        ));
    }

    let holdout_quality = summary.holdout.profit_factor
        * summary.fixed_notional_holdout.profit_factor
        * summary.holdout.win_rate.max(0.05);
    let full_quality = summary.full.profit_factor.min(5.0)
        * summary.fixed_notional_full.profit_factor.min(5.0);
    let stability_penalty = 1.0
        + (summary.full.max_drawdown_pct / 100.0)
        + (summary.fixed_notional_full.max_drawdown_pct / 100.0);
    let evidence_bonus = 1.0 + (summary.holdout.trades as f64 / min_holdout_trades.max(1) as f64);
    let score = if rejection_reasons.is_empty() {
        (holdout_quality * full_quality * evidence_bonus) / stability_penalty
    } else {
        f64::NEG_INFINITY
    };

    ParameterAuditCandidate {
        label: label.to_string(),
        rolling_window: params.rolling_window,
        entry_z: params.entry_z,
        stop_z: params.stop_z,
        target_z: params.target_z,
        max_hold_bars: params.max_hold_bars,
        audit_score: score,
        accepted: rejection_reasons.is_empty(),
        rejection_reasons,
        summary,
    }
}

pub fn rejected_parameter_candidate(
    label: &str,
    params: &PairParams,
    reason: &str,
) -> ParameterAuditCandidate {
    let zero = CompactBacktestResult {
        trades: 0,
        wins: 0,
        losses: 0,
        win_rate: 0.0,
        profit_factor: 0.0,
        net: 0.0,
        avg_trade: 0.0,
        max_drawdown_pct: 0.0,
    };
    ParameterAuditCandidate {
        label: label.to_string(),
        rolling_window: params.rolling_window,
        entry_z: params.entry_z,
        stop_z: params.stop_z,
        target_z: params.target_z,
        max_hold_bars: params.max_hold_bars,
        audit_score: f64::NEG_INFINITY,
        accepted: false,
        rejection_reasons: vec![reason.to_string()],
        summary: SplitBacktestSummary {
            pair: params.display_pair(),
            train: zero.clone(),
            holdout: zero.clone(),
            full: zero.clone(),
            fixed_notional_usdt: params.per_leg_notional_usdt.to_string(),
            fixed_notional_train: zero.clone(),
            fixed_notional_holdout: zero.clone(),
            fixed_notional_full: zero,
            recent_trades: Vec::new(),
        },
    }
}

pub fn rank_parameter_candidates(
    mut candidates: Vec<ParameterAuditCandidate>,
) -> Vec<ParameterAuditCandidate> {
    candidates.sort_by(|a, b| match (a.accepted, b.accepted) {
        (true, false) => Ordering::Less,
        (false, true) => Ordering::Greater,
        (true, true) => b
            .audit_score
            .partial_cmp(&a.audit_score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| b.summary.holdout.profit_factor.partial_cmp(&a.summary.holdout.profit_factor).unwrap_or(Ordering::Equal))
            .then_with(|| a.label.cmp(&b.label)),
        (false, false) => a
            .rejection_reasons
            .len()
            .cmp(&b.rejection_reasons.len())
            .then_with(|| a.label.cmp(&b.label)),
    });
    candidates
}

pub fn render_parameter_audit_text(
    pair_name: &str,
    candidates: &[ParameterAuditCandidate],
    min_holdout_trades: usize,
    top_n: usize,
) -> String {
    let ranked = rank_parameter_candidates(candidates.to_vec());
    let accepted = ranked.iter().filter(|c| c.accepted).collect::<Vec<_>>();
    let rejected = ranked.iter().filter(|c| !c.accepted).collect::<Vec<_>>();
    let mut lines = vec![
        format!("Parameter audit · {}", pair_name),
        format!(
            "Candidates: {} | Accepted: {} | Min holdout trades: {}",
            ranked.len(),
            accepted.len(),
            min_holdout_trades
        ),
    ];

    lines.push(String::new());
    lines.push("Top accepted candidates".to_string());
    if accepted.is_empty() {
        lines.push("  none".to_string());
    } else {
        for candidate in accepted.into_iter().take(top_n.max(1)) {
            lines.push(format!(
                "  {} | score {:.3} | hold PF {:.3} / fixed hold PF {:.3} | full PF {:.3} / fixed full PF {:.3}",
                candidate.label,
                candidate.audit_score,
                candidate.summary.holdout.profit_factor,
                candidate.summary.fixed_notional_holdout.profit_factor,
                candidate.summary.full.profit_factor,
                candidate.summary.fixed_notional_full.profit_factor,
            ));
        }
    }

    lines.push(String::new());
    lines.push("Rejected candidates".to_string());
    if rejected.is_empty() {
        lines.push("  none".to_string());
    } else {
        for candidate in rejected.into_iter().take(top_n.max(1)) {
            lines.push(format!(
                "  {} | {}",
                candidate.label,
                candidate.rejection_reasons.join("; ")
            ));
        }
    }

    lines.join("\n")
}

fn unique_sorted_usize(values: &[usize]) -> Vec<usize> {
    let mut out = values.to_vec();
    out.sort_unstable();
    out.dedup();
    out
}

fn unique_sorted_i64(values: &[i64]) -> Vec<i64> {
    let mut out = values.to_vec();
    out.sort_unstable();
    out.dedup();
    out
}

fn unique_sorted_f64(values: &[f64]) -> Vec<f64> {
    let mut out = values
        .iter()
        .copied()
        .filter(|v| v.is_finite())
        .collect::<Vec<_>>();
    out.sort_by(|a, b| a.partial_cmp(b).unwrap_or(Ordering::Equal));
    out.dedup_by(|a, b| (*a - *b).abs() < 1e-9);
    out
}

pub async fn audit_current_neighborhood(
    db_path: &str,
    params: &PairParams,
    split_pct: f64,
    min_holdout_trades: usize,
) -> Result<ParameterAuditReport, PairBacktestError> {
    let window_step = (params.rolling_window / 6).max(30);
    let hold_step = if params.max_hold_bars >= 72 { 24 } else { 12 };
    let windows = unique_sorted_usize(&[
        params.rolling_window.saturating_sub(window_step).max(20),
        params.rolling_window,
        params.rolling_window + window_step,
    ]);
    let entries = unique_sorted_f64(&[
        (params.entry_z - 0.25).max(0.25),
        params.entry_z,
        params.entry_z + 0.25,
    ]);
    let stops = unique_sorted_f64(&[
        params.stop_z - 0.25,
        params.stop_z,
        params.stop_z + 0.25,
    ]);
    let holds = unique_sorted_i64(&[
        (params.max_hold_bars - hold_step).max(1),
        params.max_hold_bars,
        params.max_hold_bars + hold_step,
    ]);

    let mut raw_candidates = Vec::new();
    for rolling_window in windows {
        for entry_z in &entries {
            for stop_z in &stops {
                if *stop_z <= *entry_z {
                    continue;
                }
                for max_hold_bars in &holds {
                    let mut candidate = params.clone();
                    candidate.rolling_window = rolling_window;
                    candidate.entry_z = *entry_z;
                    candidate.stop_z = *stop_z;
                    candidate.max_hold_bars = *max_hold_bars;
                    let label = parameter_label(&candidate);
                    match split_summary(db_path, &candidate, split_pct).await {
                        Ok(summary) => raw_candidates.push(evaluate_parameter_candidate(
                            &label,
                            &candidate,
                            summary,
                            min_holdout_trades,
                        )),
                        Err(e) => raw_candidates.push(rejected_parameter_candidate(
                            &label,
                            &candidate,
                            &e.to_string(),
                        )),
                    }
                }
            }
        }
    }

    let candidates = rank_parameter_candidates(raw_candidates);
    let accepted_count = candidates.iter().filter(|c| c.accepted).count();
    let best_label = candidates.iter().find(|c| c.accepted).map(|c| c.label.clone());
    Ok(ParameterAuditReport {
        pair: params.display_pair(),
        current_label: parameter_label(params),
        min_holdout_trades,
        candidate_count: candidates.len(),
        accepted_count,
        best_label,
        candidates,
    })
}

pub fn walk_forward_folds_for_range(
    start_ms: i64,
    end_ms: i64,
    in_sample_ms: i64,
    out_of_sample_ms: i64,
) -> Vec<WalkForwardFoldWindow> {
    let mut out = Vec::new();
    if in_sample_ms <= 0 || out_of_sample_ms <= 0 {
        return out;
    }
    let mut is_start_ms = start_ms;
    let mut index = 0usize;
    loop {
        let is_end_ms = is_start_ms + in_sample_ms;
        let oos_end_ms = is_end_ms + out_of_sample_ms;
        if oos_end_ms > end_ms {
            break;
        }
        out.push(WalkForwardFoldWindow {
            index,
            is_start_ms,
            is_end_ms,
            oos_start_ms: is_end_ms,
            oos_end_ms,
        });
        is_start_ms += out_of_sample_ms;
        index += 1;
    }
    out
}

fn fold_selection_score(risk: &BacktestResult, fixed: &BacktestResult) -> f64 {
    if risk.trades == 0 || fixed.trades == 0 {
        return f64::NEG_INFINITY;
    }
    if risk.profit_factor <= 1.0 || fixed.profit_factor <= 1.0 {
        return f64::NEG_INFINITY;
    }
    let quality = risk.profit_factor.min(5.0)
        * fixed.profit_factor.min(5.0)
        * risk.win_rate.max(0.05)
        * fixed.win_rate.max(0.05);
    let penalty = 1.0 + (risk.max_drawdown_pct / 100.0) + (fixed.max_drawdown_pct / 100.0);
    (quality * (1.0 + risk.trades.min(fixed.trades) as f64 / 10.0)) / penalty
}

pub fn choose_walk_forward_winner(
    candidates: &[ParameterAuditCandidate],
) -> Option<ParameterAuditCandidate> {
    candidates
        .iter()
        .filter(|c| c.accepted)
        .max_by(|a, b| {
            a.audit_score
                .partial_cmp(&b.audit_score)
                .unwrap_or(Ordering::Equal)
                .then_with(|| a.label.cmp(&b.label))
        })
        .cloned()
}

fn compact_from_pair_trades(pair: &str, trades: &[PairTrade]) -> CompactBacktestResult {
    let mut wins = 0usize;
    let mut losses = 0usize;
    let mut gross_profit = 0.0f64;
    let mut gross_loss = 0.0f64;
    let mut equity = 1.0f64;
    let mut peak = 1.0f64;
    let mut max_dd = 0.0f64;

    let mut ordered = trades.to_vec();
    ordered.sort_by_key(|t| (t.exit_ms, t.entry_ms));
    for trade in &ordered {
        if trade.net > 0.0 {
            wins += 1;
            gross_profit += trade.net;
        } else if trade.net < 0.0 {
            losses += 1;
            gross_loss += -trade.net;
        }
        equity += trade.net;
        peak = peak.max(equity);
        let dd = if peak > 0.0 { (peak - equity) / peak } else { 0.0 };
        if dd > max_dd {
            max_dd = dd;
        }
    }
    let trades_n = ordered.len();
    let net = ordered.iter().map(|t| t.net).sum::<f64>();
    let _ = pair;
    CompactBacktestResult {
        trades: trades_n,
        wins,
        losses,
        win_rate: if trades_n == 0 { 0.0 } else { wins as f64 / trades_n as f64 },
        profit_factor: if gross_loss > 0.0 {
            gross_profit / gross_loss
        } else if gross_profit > 0.0 {
            f64::INFINITY
        } else {
            0.0
        },
        net,
        avg_trade: if trades_n == 0 { 0.0 } else { net / trades_n as f64 },
        max_drawdown_pct: max_dd * 100.0,
    }
}

pub fn render_walk_forward_audit_text(
    pair_name: &str,
    candidate_pool_size: usize,
    folds: &[WalkForwardAuditFoldSummary],
    scoreboard: &[(String, usize, f64, f64)],
) -> String {
    let mut lines = vec![
        format!("Walk-forward audit · {}", pair_name),
        format!("Candidate pool: {} | Folds run: {}", candidate_pool_size, folds.len()),
        String::new(),
        "Fold winners".to_string(),
    ];
    if folds.is_empty() {
        lines.push("  none".to_string());
    } else {
        for fold in folds {
            lines.push(format!(
                "  fold {} | {} | IS score {:.3} | OOS trades {} | OOS PF {:.3} | OOS fixed PF {:.3}",
                fold.fold.index,
                fold.chosen_label,
                fold.in_sample_score,
                fold.oos_trades,
                fold.oos_profit_factor,
                fold.oos_fixed_profit_factor,
            ));
        }
    }
    lines.push(String::new());
    lines.push("Candidate scoreboard".to_string());
    if scoreboard.is_empty() {
        lines.push("  none".to_string());
    } else {
        for (label, selected, pf, fixed_pf) in scoreboard {
            lines.push(format!(
                "  {} | folds selected {} | OOS PF {:.3} | OOS fixed PF {:.3}",
                label, selected, pf, fixed_pf
            ));
        }
    }
    lines.join("\n")
}

pub fn classify_walk_forward_candidate(
    row: &WalkForwardCandidateScoreboardRow,
) -> PromotionDecision {
    let mut reasons = Vec::new();
    if row.oos_profit_factor <= 1.0 {
        reasons.push(format!("OOS PF {:.3} must be > 1.0", row.oos_profit_factor));
    }
    if row.oos_fixed_profit_factor <= 1.0 {
        reasons.push(format!(
            "OOS fixed PF {:.3} must be > 1.0",
            row.oos_fixed_profit_factor
        ));
    }
    if row.oos_net <= 0.0 {
        reasons.push(format!("OOS net {:.4} must be positive", row.oos_net));
    }
    if row.oos_fixed_net <= 0.0 {
        reasons.push(format!(
            "OOS fixed net {:.4} must be positive",
            row.oos_fixed_net
        ));
    }
    if !row.oos_profit_factor.is_finite() || !row.oos_fixed_profit_factor.is_finite() {
        reasons.push("infinite PF needs more loss-side evidence before promotion".to_string());
    }

    if !reasons.is_empty() {
        return PromotionDecision {
            verdict: if row.oos_profit_factor > 1.0
                && row.oos_fixed_profit_factor > 1.0
                && row.oos_net > 0.0
                && row.oos_fixed_net > 0.0
            {
                PromotionVerdict::Watchlist
            } else {
                PromotionVerdict::Reject
            },
            candidate: row.clone(),
            reasons,
        };
    }

    let mut watchlist_reasons = Vec::new();
    if row.folds_selected < 2 {
        watchlist_reasons.push(format!(
            "folds selected {} below promote minimum 2",
            row.folds_selected
        ));
    }
    if row.oos_trades < 12 {
        watchlist_reasons.push(format!(
            "OOS trades {} below promote minimum 12",
            row.oos_trades
        ));
    }

    PromotionDecision {
        verdict: if watchlist_reasons.is_empty() {
            PromotionVerdict::Promote
        } else {
            PromotionVerdict::Watchlist
        },
        candidate: row.clone(),
        reasons: watchlist_reasons,
    }
}

pub fn render_promotion_gate_text(pair_name: &str, decisions: &[PromotionDecision]) -> String {
    let mut promote = Vec::new();
    let mut watchlist = Vec::new();
    let mut reject = Vec::new();
    for decision in decisions {
        match decision.verdict {
            PromotionVerdict::Promote => promote.push(decision),
            PromotionVerdict::Watchlist => watchlist.push(decision),
            PromotionVerdict::Reject => reject.push(decision),
        }
    }

    let mut lines = vec![format!("Promotion gate · {}", pair_name)];
    for (title, bucket) in [
        ("Promote", promote),
        ("Watchlist", watchlist),
        ("Reject", reject),
    ] {
        lines.push(String::new());
        lines.push(title.to_string());
        if bucket.is_empty() {
            lines.push("  none".to_string());
            continue;
        }
        for decision in bucket {
            let row = &decision.candidate;
            let why = if decision.reasons.is_empty() {
                "meets promote gate".to_string()
            } else {
                decision.reasons.join("; ")
            };
            lines.push(format!(
                "  {} | folds {} | OOS trades {} | PF {:.3} | fixed PF {:.3} | {}",
                row.label,
                row.folds_selected,
                row.oos_trades,
                row.oos_profit_factor,
                row.oos_fixed_profit_factor,
                why,
            ));
        }
    }
    lines.join("\n")
}

pub async fn audit_walk_forward_top_candidates(
    db_path: &str,
    params: &PairParams,
    split_pct: f64,
    min_holdout_trades: usize,
    top_n: usize,
    in_sample_ms: i64,
    out_of_sample_ms: i64,
) -> Result<WalkForwardAuditReport, PairBacktestError> {
    let current = audit_current_neighborhood(db_path, params, split_pct, min_holdout_trades).await?;
    let pool = current
        .candidates
        .iter()
        .filter(|c| c.accepted)
        .take(top_n.max(1))
        .cloned()
        .collect::<Vec<_>>();

    let (times, _, _) = load_pair_series_from_db(db_path, params).await?;
    let tf_ms = params.timeframe.duration_ms();
    let start_ms = *times.first().ok_or_else(|| PairBacktestError::Data("no pair candles".into()))?;
    let end_ms = times.last().copied().ok_or_else(|| PairBacktestError::Data("no pair candles".into()))? + tf_ms;
    let folds = walk_forward_folds_for_range(start_ms, end_ms, in_sample_ms, out_of_sample_ms);

    let mut fold_rows = Vec::new();
    let mut risk_by_label: HashMap<String, Vec<PairTrade>> = HashMap::new();
    let mut fixed_by_label: HashMap<String, Vec<PairTrade>> = HashMap::new();
    let mut chosen_count: HashMap<String, usize> = HashMap::new();

    for fold in folds {
        let mut is_evals = Vec::new();
        for candidate in &pool {
            let mut candidate_params = params.clone();
            candidate_params.rolling_window = candidate.rolling_window;
            candidate_params.entry_z = candidate.entry_z;
            candidate_params.stop_z = candidate.stop_z;
            candidate_params.target_z = candidate.target_z;
            candidate_params.max_hold_bars = candidate.max_hold_bars;

            let mut fixed_params = candidate_params.clone();
            fixed_params.risk_pct_of_equity = Decimal::ZERO;
            let is_risk = run_backtest(db_path, &candidate_params, Some(fold.is_start_ms), Some(fold.is_end_ms)).await;
            let is_fixed = run_backtest(db_path, &fixed_params, Some(fold.is_start_ms), Some(fold.is_end_ms)).await;
            if let (Ok(risk), Ok(fixed)) = (is_risk, is_fixed) {
                let score = fold_selection_score(&risk, &fixed);
                if score.is_finite() {
                    let mut accepted = candidate.clone();
                    accepted.audit_score = score;
                    is_evals.push(accepted);
                }
            }
        }

        let Some(chosen) = choose_walk_forward_winner(&is_evals) else {
            continue;
        };

        let mut chosen_params = params.clone();
        chosen_params.rolling_window = chosen.rolling_window;
        chosen_params.entry_z = chosen.entry_z;
        chosen_params.stop_z = chosen.stop_z;
        chosen_params.target_z = chosen.target_z;
        chosen_params.max_hold_bars = chosen.max_hold_bars;
        let mut chosen_fixed = chosen_params.clone();
        chosen_fixed.risk_pct_of_equity = Decimal::ZERO;

        let oos_risk = run_backtest(db_path, &chosen_params, Some(fold.oos_start_ms), Some(fold.oos_end_ms)).await?;
        let oos_fixed = run_backtest(db_path, &chosen_fixed, Some(fold.oos_start_ms), Some(fold.oos_end_ms)).await?;

        risk_by_label
            .entry(chosen.label.clone())
            .or_default()
            .extend(oos_risk.trades_detail.clone());
        fixed_by_label
            .entry(chosen.label.clone())
            .or_default()
            .extend(oos_fixed.trades_detail.clone());
        *chosen_count.entry(chosen.label.clone()).or_insert(0) += 1;

        fold_rows.push(WalkForwardAuditFoldSummary {
            fold,
            chosen_label: chosen.label.clone(),
            in_sample_score: chosen.audit_score,
            oos_trades: oos_risk.trades,
            oos_profit_factor: oos_risk.profit_factor,
            oos_fixed_profit_factor: oos_fixed.profit_factor,
        });
    }

    let mut scoreboard = Vec::new();
    for candidate in &pool {
        let risk = compact_from_pair_trades(&params.display_pair(), risk_by_label.get(&candidate.label).map(Vec::as_slice).unwrap_or(&[]));
        let fixed = compact_from_pair_trades(&params.display_pair(), fixed_by_label.get(&candidate.label).map(Vec::as_slice).unwrap_or(&[]));
        scoreboard.push(WalkForwardCandidateScoreboardRow {
            label: candidate.label.clone(),
            folds_selected: chosen_count.get(&candidate.label).copied().unwrap_or(0),
            oos_trades: risk.trades,
            oos_profit_factor: risk.profit_factor,
            oos_fixed_profit_factor: fixed.profit_factor,
            oos_net: risk.net,
            oos_fixed_net: fixed.net,
        });
    }
    scoreboard.sort_by(|a, b| {
        b.folds_selected
            .cmp(&a.folds_selected)
            .then_with(|| b.oos_profit_factor.partial_cmp(&a.oos_profit_factor).unwrap_or(Ordering::Equal))
            .then_with(|| b.oos_fixed_profit_factor.partial_cmp(&a.oos_fixed_profit_factor).unwrap_or(Ordering::Equal))
            .then_with(|| a.label.cmp(&b.label))
    });
    let decisions = scoreboard
        .iter()
        .map(classify_walk_forward_candidate)
        .collect::<Vec<_>>();

    Ok(WalkForwardAuditReport {
        pair: params.display_pair(),
        candidate_pool_size: pool.len(),
        folds: fold_rows,
        scoreboard,
        decisions,
    })
}
