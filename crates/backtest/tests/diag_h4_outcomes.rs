// DIAGNOSTIC — exploratory. Profitability of the no-MSS ICT configurations.
use backtest::metrics::compute;
use backtest::{BacktestConfig, CostModel, ExitReason, run_backtest};
use botcore::{Instrument, Symbol, Timeframe};
use history::HistoryDb;
use risk::{RiskManager, RiskParams};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use strategy::ict::{IctParams, IctStrategy};

const DB: &str = "/var/home/acekavi/Projects/Crypto/data/history.db";
const DAY: i64 = 86_400_000;

#[tokio::test]
#[ignore = "slow diagnostic; run explicitly"]
async fn diag_h4_outcomes() {
    if !std::path::Path::new(DB).exists() {
        println!("DIAG skipped");
        return;
    }
    let db = HistoryDb::open_local(DB).await.expect("db");
    let series = db.stored_series().await.expect("series");
    let start = series.iter().map(|(_, _, e, _)| *e).max().unwrap();
    // Research window only; the holdout stays sealed.
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

    let base = BacktestConfig {
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

    let req = IctParams::h4_sweep_m5_entry();
    let configs = [
        ("H4/M5  noMSS 1:3", req.clone(), None),
        (
            "H4/M15 noMSS 1:3",
            IctParams {
                execution_tf: Timeframe::M15,
                ..req.clone()
            },
            None,
        ),
        (
            "H1/M5  noMSS 1:3",
            IctParams {
                structure_tf: Timeframe::H1,
                ..req.clone()
            },
            None,
        ),
        (
            "H1/M15 noMSS 1:3",
            IctParams {
                structure_tf: Timeframe::H1,
                execution_tf: Timeframe::M15,
                ..req.clone()
            },
            None,
        ),
        (
            "H1/M15 noMSS 1:2",
            IctParams {
                structure_tf: Timeframe::H1,
                execution_tf: Timeframe::M15,
                reward_multiple: Decimal::TWO,
                ..req.clone()
            },
            None,
        ),
        (
            "H1/M15 noMSS 1:3 +BE",
            IctParams {
                structure_tf: Timeframe::H1,
                execution_tf: Timeframe::M15,
                ..req
            },
            Some(Decimal::ONE),
        ),
    ];

    println!("DIAG {} days, research window only\n", (end - start) / DAY);
    for (label, p, be) in configs {
        let cfg = BacktestConfig {
            breakeven_at_r: be,
            ..base.clone()
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
            "DIAG {label}: n={:<4} win%={:<6} tgt={:<4} exp={:<10} PF={:<8} maxDD={:<7} net={:<11} fees={}",
            m.trade_count,
            (m.win_rate * Decimal::ONE_HUNDRED).round_dp(1),
            tgt,
            m.expectancy.round_dp(2),
            pf,
            m.max_drawdown_pct.round_dp(1),
            m.net_pnl.round_dp(2),
            m.total_fees.round_dp(0)
        );
    }
}
