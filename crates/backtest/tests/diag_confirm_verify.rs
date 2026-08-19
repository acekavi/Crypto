// DIAGNOSTIC — does confirmation add real information, or does it just
// further concentrate an already-selected base cell? Test it against the
// PLAIN base (no reaction-strength filter) too, and break the best cell
// down by symbol/quarter.
use std::collections::BTreeMap;

use backtest::metrics::compute;
use backtest::{BacktestConfig, CostModel, run_backtest};
use botcore::{Instrument, Symbol, Timeframe};
use history::HistoryDb;
use risk::{RiskManager, RiskParams};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use strategy::{LevelParams, LevelReactionStrategy, ReactMode, StopSource};

const DB: &str = "/var/home/acekavi/Projects/Crypto/data/history.db";

#[tokio::test]
#[ignore = "slow diagnostic; run explicitly"]
async fn diag_confirm_verify() {
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
                launch_time_ms: 0,
            })
            .collect(),
        costs: CostModel {
            maker_fee_rate: dec!(0.0002),
        },
        warmup_candles: 120,
        entry_expiry_candles: 12,
    };
    let rp = RiskParams {
        risk_pct: dec!(0.0025),
        max_concurrent_positions: 8,
        max_daily_entries: 5,
        total_drawdown_halt_pct: dec!(0.20),
        ..RiskParams::defaults()
    };

    println!(
        "DIAG === Does confirmation help WITHOUT the min_reaction_atr=1.5 filter it was stacked on? ===\n"
    );
    println!(
        "DIAG {:<22}{:>7}{:>8}{:>10}{:>9}{:>8}{:>12}",
        "config", "n", "win%", "exp", "PF", "maxDD", "net"
    );
    for (label, min_react, margin) in [
        ("plain base, no confirm", dec!(0.0), None),
        ("plain base, confirm=1.0/12", dec!(0.0), Some(dec!(1.0))),
        ("min_react=1.5, no confirm", dec!(1.5), None),
        ("min_react=1.5, confirm=1.0/12", dec!(1.5), Some(dec!(1.0))),
    ] {
        let p = LevelParams {
            react_mode: ReactMode::Reversal,
            level_tf: Timeframe::H4,
            min_break_atr: dec!(0.25),
            entry_fraction: dec!(0.75),
            stop_source: StopSource::TouchAndReaction,
            stop_buffer_atr: dec!(0.5),
            min_reaction_atr: min_react,
            confirm_margin_atr: margin,
            confirm_window: 12,
            ..LevelParams::m5_default()
        };
        let r = run_backtest(
            &db,
            &base,
            Box::new(LevelReactionStrategy::new(p)),
            RiskManager::new(rp.clone(), dec!(0.3)),
        )
        .await
        .expect("run");
        let m = compute(&r.trades, dec!(10000));
        println!(
            "DIAG {:<22}{:>7}{:>8}{:>10}{:>9}{:>7.1}%{:>12}",
            label,
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

    println!("\nDIAG === Breakdown of the best cell: min_react=1.5, confirm=1.0/12 ===");
    let p = LevelParams {
        react_mode: ReactMode::Reversal,
        level_tf: Timeframe::H4,
        min_break_atr: dec!(0.25),
        entry_fraction: dec!(0.75),
        stop_source: StopSource::TouchAndReaction,
        stop_buffer_atr: dec!(0.5),
        min_reaction_atr: dec!(1.5),
        confirm_margin_atr: Some(dec!(1.0)),
        confirm_window: 12,
        ..LevelParams::m5_default()
    };
    let r = run_backtest(
        &db,
        &base,
        Box::new(LevelReactionStrategy::new(p)),
        RiskManager::new(rp, dec!(0.3)),
    )
    .await
    .expect("run");
    let mut by_s: BTreeMap<String, (i32, i32, Decimal)> = BTreeMap::new();
    let mut by_q: BTreeMap<String, (i32, i32, Decimal)> = BTreeMap::new();
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
        let q = (rem / 91).min(3) + 1;
        let e = by_q
            .entry(format!("{y}Q{q}"))
            .or_insert((0, 0, Decimal::ZERO));
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
    let m = compute(&r.trades, dec!(10000));
    let spos = by_s.values().filter(|(_, _, p)| *p > Decimal::ZERO).count();
    let qpos = by_q.values().filter(|(_, _, p)| *p > Decimal::ZERO).count();
    println!(
        "DIAG   n={} PF={} net={}  symbols {}/{}  quarters {}/{}",
        m.trade_count,
        m.profit_factor
            .map(|v| v.round_dp(3).to_string())
            .unwrap_or_default(),
        m.net_pnl.round_dp(0),
        spos,
        by_s.len(),
        qpos,
        by_q.len()
    );
    for (k, (n, w, p)) in &by_s {
        println!(
            "DIAG     {k:<10} n={n:<4} win={:<5.1} net={}",
            f64::from(*w) / f64::from(*n) * 100.0,
            p.round_dp(0)
        );
    }
    for (k, (n, w, p)) in &by_q {
        println!(
            "DIAG     {k:<8} n={n:<4} win={:<5.1} net={}",
            f64::from(*w) / f64::from(*n) * 100.0,
            p.round_dp(0)
        );
    }
}
