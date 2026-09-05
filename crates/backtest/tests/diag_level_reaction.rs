// DIAGNOSTIC — break a swing level, retest it, trade the reaction candle.
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
async fn diag_level_reaction() {
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
    println!(
        "DIAG {:<14}{:<7}{:<8}{:>7}{:>8}{:>10}{:>9}{:>8}{:>12}",
        "mode", "levels", "brk_atr", "n", "win%", "exp", "PF", "maxDD", "net"
    );
    for (label, mode) in [
        ("continuation", ReactMode::Continuation),
        ("reversal", ReactMode::Reversal),
        ("both", ReactMode::Both),
    ] {
        for (tf_label, tf) in [("H1", Timeframe::H1), ("H4", Timeframe::H4)] {
            for brk in ["0.25", "0.5"] {
                let p = LevelParams {
                    react_mode: mode,
                    level_tf: tf,
                    min_break_atr: brk.parse().unwrap(),
                    ..LevelParams::m5_default()
                };
                let rp = RiskParams {
                    max_concurrent_positions: 8,
                    max_daily_entries: 5,
                    total_drawdown_halt_pct: dec!(0.20),
                    ..RiskParams::defaults()
                };
                let r = run_backtest(
                    &db,
                    &base,
                    Box::new(LevelReactionStrategy::new(p)),
                    RiskManager::new(rp, dec!(0.3)),
                )
                .await
                .expect("run");
                let m = compute(&r.trades, dec!(10000));
                println!(
                    "DIAG {:<14}{:<7}{:<8}{:>7}{:>8}{:>10}{:>9}{:>7.1}%{:>12}",
                    label,
                    tf_label,
                    brk,
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
    }
}
