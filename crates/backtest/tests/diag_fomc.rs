// DIAGNOSTIC — FOMC news-reaction strategy. 24 events x 8 symbols is a small
// sample by this project's standards; see the pre-registration for why the
// bar here is conservative rather than a pass/fail line.
use std::collections::BTreeMap;

use backtest::metrics::compute;
use backtest::{BacktestConfig, CostModel, run_backtest};
use botcore::{Instrument, Symbol, Timeframe};
use history::HistoryDb;
use risk::{RiskManager, RiskParams};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use strategy::{NewsReactionParams, NewsReactionStrategy, ReactMode};

const DB: &str = "/var/home/acekavi/Projects/Crypto/data/history.db";

#[tokio::test]
#[ignore = "slow diagnostic; run explicitly"]
async fn diag_fomc() {
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
    // Full population, no funding-size selection bias.
    let rp = RiskParams {
        risk_pct: dec!(0.0025),
        max_concurrent_positions: 8,
        max_daily_entries: 5,
        total_drawdown_halt_pct: dec!(0.20),
        ..RiskParams::defaults()
    };

    println!("DIAG 24 FOMC events x 8 symbols, 0.25% risk (full population)\n");
    println!(
        "DIAG {:<14}{:>7}{:>8}{:>10}{:>9}{:>8}{:>12}",
        "react_mode", "n", "win%", "exp", "PF", "maxDD", "net"
    );
    let mut all_trades = Vec::new();
    for (label, mode) in [
        ("Continuation", ReactMode::Continuation),
        ("Reversal", ReactMode::Reversal),
        ("Both", ReactMode::Both),
    ] {
        let p = NewsReactionParams {
            react_mode: mode,
            ..NewsReactionParams::fomc_default()
        };
        let r = run_backtest(
            &db,
            &base,
            Box::new(NewsReactionStrategy::new(p)),
            RiskManager::new(rp.clone(), dec!(0.3)),
        )
        .await
        .expect("run");
        let m = compute(&r.trades, dec!(10000));
        println!(
            "DIAG {:<14}{:>7}{:>8}{:>10}{:>9}{:>7.1}%{:>12}",
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
        if label == "Both" {
            all_trades = r.trades;
        }
    }

    println!("\nDIAG ---- every trade from the 'Both' run, and its concentration ----");
    let mut by_s: BTreeMap<String, (i32, i32, Decimal)> = BTreeMap::new();
    for t in &all_trades {
        let e = by_s
            .entry(t.symbol.as_str().to_string())
            .or_insert((0, 0, Decimal::ZERO));
        e.0 += 1;
        e.1 += i32::from(t.net_pnl > Decimal::ZERO);
        e.2 += t.net_pnl;
        println!(
            "DIAG   {}  entry_ms={}  {}  net={}",
            t.symbol.as_str(),
            t.entry_ms,
            if t.net_pnl > Decimal::ZERO {
                "WIN "
            } else {
                "loss"
            },
            t.net_pnl.round_dp(1)
        );
    }
    println!("\nDIAG   by symbol:");
    for (k, (n, w, p)) in &by_s {
        println!(
            "DIAG     {k:<10} n={n:<4} win={:<5.1} net={}",
            f64::from(*w) / f64::from(*n) * 100.0,
            p.round_dp(0)
        );
    }
}
