// DIAGNOSTIC — do the position caps cost anything, and what does 2% risk do?
// Walk-forward, so the numbers are comparable to the PASS run.
use backtest::metrics::compute;
use backtest::walk_forward::{WalkForwardConfig, run_walk_forward};
use backtest::{BacktestConfig, CostModel};
use botcore::{Instrument, Symbol};
use history::HistoryDb;
use risk::RiskParams;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use strategy::ict::{IctParams, IctStrategy};

const DB: &str = "/var/home/acekavi/Projects/Crypto/data/history.db";
const DAY: i64 = 86_400_000;

#[tokio::test]
#[ignore = "slow diagnostic; run explicitly"]
async fn diag_position_caps_and_risk() {
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
    let cfg = BacktestConfig {
        start_ms: start,
        end_ms: end,
        starting_equity: dec!(10000),
        symbols: symbols.clone(),
        instruments: symbols
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
        warmup_candles: 250,
        entry_expiry_candles: 12,
    };

    let p = IctParams::liquidity_sweep_v1();
    let wf = WalkForwardConfig::defaults();

    // One position per symbol is NOT removed: Bybit's one-way mode allows a
    // single net position per symbol, so lifting it would model an account
    // that cannot exist.
    let configs = [
        ("1% risk, caps 4/5 (the PASS)", dec!(0.01), 4usize, 5u32),
        ("2% risk, caps 4/5", dec!(0.02), 4, 5),
        ("1% risk, NO caps", dec!(0.01), 8, 1000),
        ("2% risk, NO caps", dec!(0.02), 8, 1000),
        ("2% risk, concurrency only lifted", dec!(0.02), 8, 5),
        ("2% risk, daily cap only lifted", dec!(0.02), 4, 1000),
    ];

    println!("DIAG walk-forward, {} days, 9 folds\n", (end - start) / DAY);
    for (label, risk, conc, daily) in configs {
        let rp = RiskParams {
            risk_pct: risk,
            max_concurrent_positions: conc,
            max_daily_entries: daily,
            ..RiskParams::defaults()
        };
        let r = run_walk_forward(
            &db,
            &cfg,
            &wf,
            std::slice::from_ref(&p),
            &rp,
            dec!(0.3),
            &|q| Box::new(IctStrategy::new(q.clone())),
        )
        .await
        .expect("run");
        let m = &r.oos_metrics;
        let halts: usize = r.folds.iter().map(|_| 0).sum::<usize>();
        let _ = halts;
        println!(
            "DIAG {label:<34} n={:<5} win%={:<6} exp={:<10} PF={:<8} worstFoldDD={:<7} net={}",
            m.trade_count,
            (m.win_rate * Decimal::ONE_HUNDRED).round_dp(1),
            m.expectancy.round_dp(2),
            m.profit_factor
                .map(|v| v.round_dp(3).to_string())
                .unwrap_or_else(|| "undef".into()),
            m.max_drawdown_pct.round_dp(2),
            compute(&r.oos_trades, dec!(10000)).net_pnl.round_dp(2)
        );
    }
}
