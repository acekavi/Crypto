// DIAGNOSTIC — the frozen `liquidity_sweep_v2` result, reproduced end to end.
//
// The reference figures this must print are n=298, win 24.2%, PF 1.457,
// maxDD 17.1%, net 18008 over the research window on the eight validated
// symbols. They were measured while the breakeven threshold lived on
// `BacktestConfig`; it now rides on the `Signal`, and moving it must have
// changed nothing. A different number here means the refactor changed
// behaviour, not that the number needs updating.
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
async fn diag_v2_baseline() {
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
    let rp = RiskParams {
        max_concurrent_positions: 8,
        max_daily_entries: 5,
        total_drawdown_halt_pct: dec!(0.20),
        ..RiskParams::defaults()
    };

    let r = run_backtest(
        &db,
        &cfg,
        // No breakeven argument anywhere: the strategy carries it.
        Box::new(IctStrategy::new(IctParams::liquidity_sweep_v2())),
        RiskManager::new(rp, dec!(0.3)),
    )
    .await
    .expect("run");
    let m = compute(&r.trades, dec!(10000));

    println!("DIAG expect  n=298  win=24.2  PF=1.457  maxDD=17.1  net=18008");
    println!(
        "DIAG actual  n={}  win={}  PF={}  maxDD={}  net={}",
        m.trade_count,
        (m.win_rate * Decimal::ONE_HUNDRED).round_dp(1),
        m.profit_factor
            .map(|v| v.round_dp(3).to_string())
            .unwrap_or_else(|| "undef".into()),
        m.max_drawdown_pct.round_dp(1),
        m.net_pnl.round_dp(0)
    );
}
