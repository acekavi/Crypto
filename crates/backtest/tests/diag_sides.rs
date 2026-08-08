// DIAGNOSTIC — long-only vs short-only vs both, and the breakeven stop.
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
async fn diag_sides() {
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
    };
    let v1 = IctParams::liquidity_sweep_v1();
    let cases = [
        ("both sides (baseline)", v1.clone(), None),
        (
            "LONG only",
            IctParams {
                allow_short: false,
                ..v1.clone()
            },
            None,
        ),
        (
            "SHORT only",
            IctParams {
                allow_long: false,
                ..v1.clone()
            },
            None,
        ),
        ("both + breakeven @1R", v1.clone(), Some(Decimal::ONE)),
        ("both + breakeven @1.5R", v1.clone(), Some(dec!(1.5))),
        (
            "SHORT only + breakeven @1R",
            IctParams {
                allow_long: false,
                ..v1
            },
            Some(Decimal::ONE),
        ),
    ];
    for (label, p, be) in cases {
        // The threshold rides on the strategy now, not on the run config.
        let p = IctParams {
            breakeven_at_r: be,
            ..p
        };
        let r = run_backtest(
            &db,
            &base,
            Box::new(IctStrategy::new(p)),
            RiskManager::new(RiskParams::defaults(), dec!(0.3)),
        )
        .await
        .expect("run");
        let m = compute(&r.trades, dec!(10000));
        println!(
            "DIAG {label:<28} n={:<5} win%={:<6} exp={:<9} PF={:<8} maxDD={:<6} net={}",
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
