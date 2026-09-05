// DIAGNOSTIC — export ICT trades as JSON for the chart viewer.
use backtest::{BacktestConfig, CostModel, ExitReason, run_backtest};
use botcore::{Instrument, Side, Symbol};
use history::HistoryDb;
use risk::{RiskManager, RiskParams};
use rust_decimal_macros::dec;
use strategy::ict::{IctParams, IctStrategy};

const DB: &str = "/var/home/acekavi/Projects/Crypto/data/history.db";

#[tokio::test]
#[ignore = "slow diagnostic; run explicitly"]
async fn diag_export_trades() {
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

    let cfg = BacktestConfig {
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
    let r = run_backtest(
        &db,
        &cfg,
        Box::new(IctStrategy::new(IctParams::liquidity_sweep_v1())),
        RiskManager::new(RiskParams::defaults(), dec!(0.3)),
    )
    .await
    .expect("run");

    let mut out = String::from("[\n");
    for (i, t) in r.trades.iter().enumerate() {
        if i > 0 {
            out.push_str(",\n");
        }
        let won = t.exit_reason == ExitReason::Target;
        // ClosedTrade carries no stop/target, but both are recoverable: a
        // stopped trade exits AT the stop, a winner exits at 3R, so one exit
        // price plus the reward multiple determines both levels.
        let move_abs = (t.exit_price - t.entry_price).abs();
        let risk = if won { move_abs / dec!(3) } else { move_abs };
        let (stop, target) = match t.side {
            Side::Buy => (t.entry_price - risk, t.entry_price + risk * dec!(3)),
            Side::Sell => (t.entry_price + risk, t.entry_price - risk * dec!(3)),
        };
        out.push_str(&format!(
            r#"{{"sym":"{}","side":"{}","entry_ms":{},"exit_ms":{},"entry":{},"exit":{},"stop":{},"target":{},"net":{},"won":{}}}"#,
            t.symbol.as_str(),
            if t.side == Side::Buy { "long" } else { "short" },
            t.entry_ms, t.exit_ms,
            t.entry_price, t.exit_price, stop, target,
            t.net_pnl.round_dp(2), won
        ));
    }
    out.push_str("\n]\n");
    std::fs::write("/tmp/claude-1000/-var-home-acekavi-Projects-Crypto/6bc6455c-d9f4-4d9a-b0d7-ee930cafa55b/scratchpad/trades.json", out).expect("write");
    println!("DIAG exported {} trades", r.trades.len());
}
