// DIAGNOSTIC — incremental effect of each proposed improvement.
// Each step is applied on top of the previous, so the marginal contribution of
// every change is visible rather than only the combined result.
use backtest::metrics::compute;
use backtest::{BacktestConfig, CostModel, run_backtest};
use botcore::{Instrument, Symbol, Timeframe};
use history::HistoryDb;
use risk::{RiskManager, RiskParams};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use strategy::ict::{IctParams, IctStrategy};

const DB: &str = "/var/home/acekavi/Projects/Crypto/data/history.db";
const DAY: i64 = 86_400_000;

async fn measure(db: &HistoryDb, cfg: &BacktestConfig, p: IctParams, label: &str) {
    let r = run_backtest(
        db,
        cfg,
        Box::new(IctStrategy::new(p)),
        RiskManager::new(RiskParams::defaults(), dec!(0.3)),
    )
    .await
    .expect("run");
    let m = compute(&r.trades, cfg.starting_equity);
    println!(
        "DIAG {label:<34} n={:<5} win%={:<6} exp={:<10} PF={:<8} maxDD={:<6} net={}",
        m.trade_count,
        (m.win_rate * Decimal::ONE_HUNDRED).round_dp(1),
        m.expectancy.round_dp(2),
        m.profit_factor
            .map(|v| v.round_dp(3).to_string())
            .unwrap_or_else(|| "undef".into()),
        m.max_drawdown_pct.round_dp(1),
        m.net_pnl.round_dp(2)
    );
}

#[tokio::test]
#[ignore = "slow diagnostic; run explicitly"]
async fn diag_improvement_steps() {
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
    let base_cfg = BacktestConfig {
        start_ms: start,
        end_ms: end,
        starting_equity: dec!(10000),
        symbols,
        instruments,
        costs: CostModel {
            maker_fee_rate: dec!(0.0002),
        },
        warmup_candles: 250,
        entry_expiry_candles: 3,
        breakeven_at_r: None,
    };
    let h4m15 = IctParams {
        execution_tf: Timeframe::M15,
        ..IctParams::h4_sweep_m5_entry()
    };

    println!(
        "DIAG {} days, 8 symbols, research window only\n",
        (end - start) / DAY
    );

    println!("DIAG === BASELINE ===");
    measure(&db, &base_cfg, h4m15.clone(), "H4 sweep, expiry 3").await;

    println!("\nDIAG === STEP 1: entry expiry ===");
    for exp in [3u32, 6, 12, 24, 48] {
        let cfg = BacktestConfig {
            entry_expiry_candles: exp,
            ..base_cfg.clone()
        };
        measure(&db, &cfg, h4m15.clone(), &format!("H4 sweep, expiry {exp}")).await;
    }

    println!("\nDIAG === STEP 2: extra liquidity pools (H4 sweep, expiry 12) ===");
    let cfg12 = BacktestConfig {
        entry_expiry_candles: 12,
        ..base_cfg.clone()
    };
    measure(&db, &cfg12, h4m15.clone(), "swings only (baseline)").await;
    measure(
        &db,
        &cfg12,
        IctParams {
            use_pdh_pdl: true,
            ..h4m15.clone()
        },
        "+ PDH/PDL",
    )
    .await;
    measure(
        &db,
        &cfg12,
        IctParams {
            use_session_levels: true,
            ..h4m15.clone()
        },
        "+ session levels",
    )
    .await;
    measure(
        &db,
        &cfg12,
        IctParams {
            use_pdh_pdl: true,
            use_session_levels: true,
            ..h4m15.clone()
        },
        "+ both",
    )
    .await;

    println!("\nDIAG === STEP 4: sweep on H1 instead of H4 (expiry held at 12) ===");
    measure(&db, &cfg12, h4m15.clone(), "H4 sweep, expiry 12").await;
    measure(
        &db,
        &cfg12,
        IctParams {
            structure_tf: Timeframe::H1,
            ..h4m15.clone()
        },
        "H1 sweep, expiry 12",
    )
    .await;
}
