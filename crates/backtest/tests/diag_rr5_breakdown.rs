// DIAGNOSTIC — is the 1:5 + breakeven@2R gain broad, or one symbol / one year?
use std::collections::BTreeMap;

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
async fn diag_rr5_breakdown() {
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
                min_notional: dec!(5),
                launch_time_ms: 0,
            })
            .collect(),
        costs: CostModel {
            maker_fee_rate: dec!(0.0002),
        },
        warmup_candles: 250,
        entry_expiry_candles: 12,
    };

    for (label, rr, be) in [
        ("1:3 no BE (current)", 3i64, None),
        ("1:5 BE@2R", 5, Some(dec!(2))),
    ] {
        let p = IctParams {
            reward_multiple: Decimal::from(rr),
            breakeven_at_r: be,
            ..IctParams::liquidity_sweep_v1()
        };
        let r = run_backtest(
            &db,
            &base,
            Box::new(IctStrategy::new(p)),
            RiskManager::new(RiskParams::defaults(), dec!(0.3)),
        )
        .await
        .expect("run");

        let mut by_sym: BTreeMap<String, (i32, i32, Decimal)> = BTreeMap::new();
        let mut by_qtr: BTreeMap<String, (i32, i32, Decimal)> = BTreeMap::new();
        for t in &r.trades {
            let secs = t.entry_ms / 1000;
            let days = secs / 86_400;
            // Civil-from-days, good enough for a quarter label.
            let (mut y, mut rem) = (1970i64, days);
            loop {
                let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
                let len = if leap { 366 } else { 365 };
                if rem < len {
                    break;
                }
                rem -= len;
                y += 1;
            }
            let q = (rem / 91).min(3) + 1;
            let e = by_qtr
                .entry(format!("{y}Q{q}"))
                .or_insert((0, 0, Decimal::ZERO));
            e.0 += 1;
            e.1 += i32::from(t.net_pnl > Decimal::ZERO);
            e.2 += t.net_pnl;
            let e = by_sym
                .entry(t.symbol.as_str().to_string())
                .or_insert((0, 0, Decimal::ZERO));
            e.0 += 1;
            e.1 += i32::from(t.net_pnl > Decimal::ZERO);
            e.2 += t.net_pnl;
        }
        let m = compute(&r.trades, dec!(10000));
        println!(
            "\nDIAG ===== {label} — n={} PF={} net={}",
            m.trade_count,
            m.profit_factor
                .map(|v| v.round_dp(3).to_string())
                .unwrap_or_else(|| "undef".into()),
            m.net_pnl.round_dp(0)
        );
        println!("DIAG   by symbol:");
        for (k, (n, w, pnl)) in &by_sym {
            println!(
                "DIAG     {k:<10} n={n:<4} win={:<5.1} net={}",
                f64::from(*w) / f64::from(*n) * 100.0,
                pnl.round_dp(0)
            );
        }
        println!("DIAG   by quarter:");
        for (k, (n, w, pnl)) in &by_qtr {
            println!(
                "DIAG     {k:<8} n={n:<4} win={:<5.1} net={}",
                f64::from(*w) / f64::from(*n) * 100.0,
                pnl.round_dp(0)
            );
        }
    }
}
