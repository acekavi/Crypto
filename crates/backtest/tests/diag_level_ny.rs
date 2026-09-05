// DIAGNOSTIC — the requested config: H4 swing levels, M5 execution, 1:2 R:R,
// breakeven at 1R, 1% risk, New York session only, all 8 pairs.
use std::collections::BTreeMap;

use backtest::metrics::compute;
use backtest::{BacktestConfig, CostModel, run_backtest};
use botcore::{Instrument, Symbol, Timeframe};
use history::HistoryDb;
use risk::{RiskManager, RiskParams};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use strategy::{LevelParams, LevelReactionStrategy, ReactMode};

const DB: &str = "/var/home/acekavi/Projects/Crypto/data/history.db";

#[tokio::test]
#[ignore = "slow diagnostic; run explicitly"]
async fn diag_level_ny() {
    let db = HistoryDb::open_local(DB).await.expect("db");
    let series = db.stored_series().await.expect("series");
    let syms: Vec<Symbol> = [
        "ADAUSDT", "BNBUSDT", "BTCUSDT", "DOGEUSDT", "ETHUSDT", "LINKUSDT", "SOLUSDT", "XRPUSDT",
    ]
    .iter()
    .map(|s| Symbol::new(*s))
    .collect();
    let m5 = |t: &Timeframe| *t == Timeframe::M5;
    let start = series
        .iter()
        .filter(|(s, t, _, _)| syms.contains(s) && m5(t))
        .map(|(_, _, e, _)| *e)
        .max()
        .expect("start");
    let end = series
        .iter()
        .filter(|(s, t, _, _)| syms.contains(s) && m5(t))
        .map(|(_, _, _, l)| *l)
        .min()
        .expect("end");
    let base = BacktestConfig {
        start_ms: start,
        end_ms: end,
        starting_equity: dec!(10000),
        symbols: syms.clone(),
        instruments: syms
            .iter()
            .map(|s| Instrument {
                symbol: s.clone(),
                tick_size: dec!(0.0001),
                qty_step: dec!(0.000001),
                min_order_qty: dec!(0.000001),
                min_notional: dec!(5),
                launch_time_ms: 0,
            })
            .collect(),
        costs: CostModel {
            maker_fee_rate: dec!(0.0002),
        },
        warmup_candles: 120,
        entry_expiry_candles: 12,
    };

    let requested = |mode| LevelParams {
        react_mode: mode,
        level_tf: Timeframe::H4,
        execution_tf: Timeframe::M5,
        min_break_atr: dec!(0.25),
        reward_multiple: Decimal::TWO,
        breakeven_at_r: Some(Decimal::ONE),
        session_filter: true,
        ..LevelParams::m5_default()
    };

    println!("DIAG requested config: H4 levels, M5 exec, 1:2 R:R, BE@1R, NY only, 8 pairs");
    println!("DIAG breakeven win rate at 1:2 is 33.33% before fees\n");
    println!(
        "DIAG {:<14}{:<8}{:>7}{:>8}{:>10}{:>9}{:>8}{:>11}",
        "mode", "risk%", "n", "win%", "exp", "PF", "maxDD", "net"
    );
    for (label, mode) in [
        ("continuation", ReactMode::Continuation),
        ("reversal", ReactMode::Reversal),
        ("both", ReactMode::Both),
    ] {
        for risk in ["0.01", "0.0025"] {
            let rp = RiskParams {
                risk_pct: risk.parse().unwrap(),
                max_concurrent_positions: 8,
                max_daily_entries: 5,
                total_drawdown_halt_pct: dec!(0.20),
                ..RiskParams::defaults()
            };
            let r = run_backtest(
                &db,
                &base,
                Box::new(LevelReactionStrategy::new(requested(mode))),
                RiskManager::new(rp, dec!(0.3)),
            )
            .await
            .expect("run");
            let m = compute(&r.trades, dec!(10000));
            println!(
                "DIAG {:<14}{:<8}{:>7}{:>8}{:>10}{:>9}{:>7.1}%{:>11}",
                label,
                risk,
                m.trade_count,
                (m.win_rate * Decimal::ONE_HUNDRED).round_dp(1),
                m.expectancy.round_dp(2),
                m.profit_factor
                    .map(|v| v.round_dp(3).to_string())
                    .unwrap_or_else(|| "undef".into()),
                m.max_drawdown_pct.round_dp(1),
                m.net_pnl.round_dp(0)
            );
        }
    }

    // Per-symbol and per-quarter on the requested 1% cell.
    let rp = RiskParams {
        risk_pct: dec!(0.01),
        max_concurrent_positions: 8,
        max_daily_entries: 5,
        total_drawdown_halt_pct: dec!(0.20),
        ..RiskParams::defaults()
    };
    let r = run_backtest(
        &db,
        &base,
        Box::new(LevelReactionStrategy::new(requested(ReactMode::Both))),
        RiskManager::new(rp, dec!(0.3)),
    )
    .await
    .expect("run");
    let mut by_s: BTreeMap<String, (i32, i32, Decimal)> = BTreeMap::new();
    let mut by_y: BTreeMap<i64, (i32, i32, Decimal)> = BTreeMap::new();
    for t in &r.trades {
        let days = t.entry_ms / 1000 / 86_400;
        let (mut y, mut rem) = (1970i64, days);
        loop {
            let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
            let len = if leap { 366 } else { 365 };
            if rem < len {
                break;
            }
            rem -= len;
            y += 1;
        }
        let e = by_y.entry(y).or_insert((0, 0, Decimal::ZERO));
        e.0 += 1;
        e.1 += i32::from(t.net_pnl > Decimal::ZERO);
        e.2 += t.net_pnl;
        let e = by_s
            .entry(t.symbol.as_str().to_string())
            .or_insert((0, 0, Decimal::ZERO));
        e.0 += 1;
        e.1 += i32::from(t.net_pnl > Decimal::ZERO);
        e.2 += t.net_pnl;
    }
    println!("\nDIAG ---- both / 1% risk, by symbol ----");
    for (k, (n, w, p)) in &by_s {
        println!(
            "DIAG   {k:<10} n={n:<4} win={:<5.1} net={}",
            f64::from(*w) / f64::from(*n) * 100.0,
            p.round_dp(0)
        );
    }
    println!("DIAG ---- by year ----");
    for (k, (n, w, p)) in &by_y {
        println!(
            "DIAG   {k:<6} n={n:<4} win={:<5.1} net={}",
            f64::from(*w) / f64::from(*n) * 100.0,
            p.round_dp(0)
        );
    }
}
