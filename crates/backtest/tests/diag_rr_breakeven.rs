// DIAGNOSTIC — reward multiple against breakeven-stop threshold.
use backtest::metrics::compute;
use backtest::{BacktestConfig, CostModel, run_backtest};
use botcore::{Instrument, Symbol};
use history::HistoryDb;
use risk::{RiskManager, RiskParams};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use strategy::ict::{IctParams, IctStrategy};

const DB: &str = "/var/home/acekavi/Projects/Crypto/data/history.db";

#[tokio::test]
#[ignore = "slow diagnostic; run explicitly"]
async fn diag_rr_breakeven() {
    let db = HistoryDb::open_local(DB).await.expect("db");
    let series = db.stored_series().await.expect("series");
    let syms: Vec<Symbol> = [
        "ADAUSDT", "BNBUSDT", "BTCUSDT", "DOGEUSDT", "ETHUSDT", "LINKUSDT", "SOLUSDT", "XRPUSDT",
    ]
    .iter()
    .map(|s| Symbol::new(*s))
    .collect();
    let start = series
        .iter()
        .filter(|(s, _, _, _)| syms.contains(s))
        .map(|(_, _, e, _)| *e)
        .max()
        .unwrap();
    let end = series
        .iter()
        .filter(|(s, _, _, _)| syms.contains(s))
        .map(|(_, _, _, l)| *l)
        .min()
        .unwrap();
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
        warmup_candles: 250,
        entry_expiry_candles: 12,
        breakeven_at_r: None,
    };
    println!("DIAG  R:R   BE@    n     win%    exp      PF       maxDD   net");
    for rr in [3i64, 4, 5, 6] {
        for be in [None, Some(dec!(1)), Some(dec!(2))] {
            let p = IctParams {
                reward_multiple: Decimal::from(rr),
                ..IctParams::liquidity_sweep_v1()
            };
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
            println!(
                "DIAG  1:{rr}   {:<6} {:<5} {:<7} {:<8} {:<8} {:<7} {}",
                be.map(|v| v.normalize().to_string())
                    .unwrap_or_else(|| "none".into()),
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
