use std::collections::HashMap;
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
    pub recent_trades: Vec<PairTrade>,
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
    Ok(SplitBacktestSummary {
        pair: full.pair.clone(),
        train: compact(&train),
        holdout: compact(&hold),
        full: compact(&full),
        recent_trades: full.trades_detail.iter().rev().take(8).cloned().collect::<Vec<_>>().into_iter().rev().collect(),
    })
}
