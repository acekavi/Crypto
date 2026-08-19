// DIAGNOSTIC — entry depth x stop construction, fixed at 0.25% risk so the
// full signal population is funded and no cell benefits from the same
// margin-selection bias that got the base version rejected.

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
async fn diag_level_entry_stop() {
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
    // Fixed at 0.25% for every cell — the pre-registration's whole point.
    let rp = RiskParams {
        risk_pct: dec!(0.0025),
        max_concurrent_positions: 8,
        max_daily_entries: 5,
        total_drawdown_halt_pct: dec!(0.20),
        ..RiskParams::defaults()
    };

    println!(
        "DIAG all cells at 0.25% risk (full signal population), react=Reversal, level_tf=H4\n"
    );
    println!(
        "DIAG {:<10}{:<16}{:<9}{:>7}{:>8}{:>10}{:>9}{:>8}{:>12}",
        "entry_frac", "stop_source", "stop_buf", "n", "win%", "exp", "PF", "maxDD", "net"
    );
    let mut best: Option<(Decimal, String)> = None;
    for entry_frac in ["0.0", "0.25", "0.5", "0.75", "1.0"] {
        for (src_label, src) in [
            ("TouchOnly", StopSource::TouchOnly),
            ("TouchAndReaction", StopSource::TouchAndReaction),
        ] {
            for buf in ["0.0", "0.25", "0.5"] {
                let p = LevelParams {
                    react_mode: ReactMode::Reversal,
                    level_tf: Timeframe::H4,
                    min_break_atr: dec!(0.25),
                    entry_fraction: entry_frac.parse().unwrap(),
                    stop_source: src,
                    stop_buffer_atr: buf.parse().unwrap(),
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
                let pf = m.profit_factor.unwrap_or(Decimal::ZERO);
                let label = format!("{entry_frac}/{src_label}/{buf}");
                if best.as_ref().is_none_or(|(bp, _)| pf > *bp) {
                    best = Some((pf, label.clone()));
                }
                println!(
                    "DIAG {:<10}{:<16}{:<9}{:>7}{:>8}{:>10}{:>9}{:>7.1}%{:>12}",
                    entry_frac,
                    src_label,
                    buf,
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
    if let Some((pf, label)) = best {
        println!("\nDIAG best cell: {label}  PF={}", pf.round_dp(3));
    }
}
