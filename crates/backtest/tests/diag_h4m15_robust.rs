// DIAGNOSTIC — is the H4/M15 1:3 result robust, or a single lucky cell?
//
// A real edge is a PLATEAU: nearby parameter values behave similarly, and the
// profit is spread across symbols and time. An overfit is a SPIKE: it dies as
// soon as anything moves, and one symbol or one quarter carries it.
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

fn winner() -> IctParams {
    IctParams {
        execution_tf: Timeframe::M15,
        ..IctParams::h4_sweep_m5_entry()
    }
}

async fn run(
    db: &HistoryDb,
    cfg: &BacktestConfig,
    p: IctParams,
) -> (usize, Decimal, Decimal, Option<Decimal>) {
    let r = run_backtest(
        db,
        cfg,
        Box::new(IctStrategy::new(p)),
        RiskManager::new(RiskParams::defaults(), dec!(0.3)),
    )
    .await
    .expect("run");
    let m = compute(&r.trades, cfg.starting_equity);
    (m.trade_count, m.net_pnl, m.expectancy, m.profit_factor)
}

#[tokio::test]
#[ignore = "slow diagnostic; run explicitly"]
async fn diag_h4m15_robustness() {
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
    let inst = |s: &Symbol| Instrument {
        symbol: s.clone(),
        tick_size: dec!(0.0001),
        qty_step: dec!(0.000001),
        min_order_qty: dec!(0.000001),
        launch_time_ms: 0,
    };
    let base = BacktestConfig {
        start_ms: start,
        end_ms: end,
        starting_equity: dec!(10000),
        symbols: symbols.clone(),
        instruments: symbols.iter().map(inst).collect(),
        costs: CostModel {
            maker_fee_rate: dec!(0.0002),
        },
        warmup_candles: 250,
        entry_expiry_candles: 3,
        breakeven_at_r: None,
    };

    let (n, net, exp, pf) = run(&db, &base, winner()).await;
    println!(
        "DIAG baseline H4/M15 1:3: n={n} net={} exp={} PF={:?}\n",
        net.round_dp(2),
        exp.round_dp(2),
        pf.map(|v| v.round_dp(3))
    );

    println!("DIAG --- per symbol: is one symbol carrying it? ---");
    for s in &symbols {
        let cfg = BacktestConfig {
            symbols: vec![s.clone()],
            instruments: vec![inst(s)],
            ..base.clone()
        };
        let (n, net, _, _) = run(&db, &cfg, winner()).await;
        println!(
            "DIAG   {:<9} n={:<3} net={}",
            s.as_str(),
            n,
            net.round_dp(2)
        );
    }

    println!("\nDIAG --- per period: is one stretch carrying it? ---");
    let span = (end - start) / 4;
    for q in 0..4 {
        let cfg = BacktestConfig {
            start_ms: start + q * span,
            end_ms: start + (q + 1) * span,
            ..base.clone()
        };
        let (n, net, _, _) = run(&db, &cfg, winner()).await;
        println!(
            "DIAG   quarter {} n={:<3} net={}",
            q + 1,
            n,
            net.round_dp(2)
        );
    }

    println!("\nDIAG --- parameter neighbourhood: plateau or spike? ---");
    let w = winner();
    let neighbours = [
        (
            "swing_lookback 3",
            IctParams {
                swing_lookback: 3,
                ..w.clone()
            },
        ),
        ("swing_lookback 5 (base)", w.clone()),
        (
            "swing_lookback 8",
            IctParams {
                swing_lookback: 8,
                ..w.clone()
            },
        ),
        (
            "fvg_fraction 0.25",
            IctParams {
                fvg_entry_fraction: dec!(0.25),
                ..w.clone()
            },
        ),
        ("fvg_fraction 0.50 (base)", w.clone()),
        (
            "fvg_fraction 0.75",
            IctParams {
                fvg_entry_fraction: dec!(0.75),
                ..w.clone()
            },
        ),
        ("stop_buffer 0.00 (base)", w.clone()),
        (
            "stop_buffer 0.25",
            IctParams {
                stop_buffer_atr: dec!(0.25),
                ..w.clone()
            },
        ),
        (
            "RR 2.5",
            IctParams {
                reward_multiple: dec!(2.5),
                ..w.clone()
            },
        ),
        ("RR 3.0 (base)", w.clone()),
        (
            "RR 3.5",
            IctParams {
                reward_multiple: dec!(3.5),
                ..w
            },
        ),
    ];
    for (label, p) in neighbours {
        let (n, net, exp, pf) = run(&db, &base, p).await;
        println!(
            "DIAG   {:<26} n={:<4} net={:<11} exp={:<9} PF={}",
            label,
            n,
            net.round_dp(2),
            exp.round_dp(2),
            pf.map(|v| v.round_dp(3).to_string())
                .unwrap_or_else(|| "undef".into())
        );
    }
}
