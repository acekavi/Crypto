// DIAGNOSTIC — exploratory, NOT the pre-registered study. Measures how the
// session filter and reward multiple change OUTCOMES, which the signal-count
// funnel cannot show.
use backtest::metrics::compute;
use backtest::{BacktestConfig, CostModel, ExitReason, run_backtest};
use botcore::{Instrument, Symbol};
use history::HistoryDb;
use risk::{RiskManager, RiskParams};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use strategy::ict::{IctParams, IctStrategy};

const DB: &str = "/var/home/acekavi/Projects/Crypto/data/history.db";
const DAY: i64 = 86_400_000;

#[tokio::test]
// Diagnostic, not a guarantee: it replays the whole research window and takes
// minutes in a debug build. Run explicitly with
//   cargo test --release -p <crate> --test <name> -- --ignored --nocapture
#[ignore = "slow diagnostic; run explicitly"]
async fn diag_ict_outcomes() {
    if !std::path::Path::new(DB).exists() {
        println!("DIAG skipped");
        return;
    }
    let db = HistoryDb::open_local(DB).await.expect("db");
    let series = db.stored_series().await.expect("series");
    let start = series.iter().map(|(_, _, e, _)| *e).max().unwrap();
    let end = series.iter().map(|(_, _, _, l)| *l).min().unwrap() - 330 * DAY;

    let mut symbols: Vec<Symbol> = series.iter().map(|(s, _, _, _)| s.clone()).collect();
    symbols.sort();
    symbols.dedup();
    let instruments: Vec<Instrument> = symbols
        .iter()
        .map(|s| Instrument {
            symbol: s.clone(),
            tick_size: dec!(0.0001),
            qty_step: dec!(0.000001),
            min_order_qty: dec!(0.000001),
            launch_time_ms: 0,
        })
        .collect();

    let cfg = BacktestConfig {
        start_ms: start,
        end_ms: end,
        starting_equity: dec!(10000),
        symbols: symbols.clone(),
        instruments,
        costs: CostModel {
            maker_fee_rate: dec!(0.0002),
        },
        warmup_candles: 250,
        entry_expiry_candles: 3,
        breakeven_at_r: None,
    };

    println!(
        "DIAG single-pass over the {}-day research window\n",
        (end - start) / DAY
    );
    let variants = IctParams::declared_variants();

    for (label, session, rr) in [
        ("NY  1:2", true, Decimal::TWO),
        ("24h 1:2", false, Decimal::TWO),
        ("NY  1:3", true, Decimal::from(3)),
        ("24h 1:3", false, Decimal::from(3)),
    ] {
        for want in ["A", "C", "E"] {
            let base = variants.iter().find(|(n, _)| *n == want).unwrap().1.clone();
            let p = IctParams {
                session_filter: session,
                reward_multiple: rr,
                ..base
            };
            let r = run_backtest(
                &db,
                &cfg,
                Box::new(IctStrategy::new(p)),
                RiskManager::new(RiskParams::defaults(), dec!(0.3)),
            )
            .await
            .expect("run");
            let m = compute(&r.trades, dec!(10000));
            let tgt = r
                .trades
                .iter()
                .filter(|t| t.exit_reason == ExitReason::Target)
                .count();
            let pf = m
                .profit_factor
                .map(|v| v.round_dp(3).to_string())
                .unwrap_or_else(|| "undef".into());
            println!(
                "DIAG {label} {want}: n={:<4} win%={:<6} targets={:<4} exp={:<11} PF={:<8} net={:<10} fees={}",
                m.trade_count,
                (m.win_rate * Decimal::ONE_HUNDRED).round_dp(1),
                tgt,
                m.expectancy.round_dp(2),
                pf,
                m.net_pnl.round_dp(2),
                m.total_fees.round_dp(2)
            );
        }
        println!();
    }
}
