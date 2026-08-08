// DIAGNOSTIC — do volume or funding carry information about trade outcome?
//
// Both are already in the database and neither has ever been used as a signal:
// volume is ignored entirely, funding is charged as a cost. This asks whether
// either relates to whether a trade won, BEFORE any study is built on them.
use backtest::{BacktestConfig, ClosedTrade, CostModel, ExitReason, run_backtest};
use botcore::{Instrument, Symbol, Timeframe};
use history::HistoryDb;
use risk::{RiskManager, RiskParams};
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal_macros::dec;
use strategy::ict::{IctParams, IctStrategy};

const DB: &str = "/var/home/acekavi/Projects/Crypto/data/history.db";

/// Split a set of trades on a per-trade value and report the win rate of each
/// half. A signal that carries information separates them.
fn split_report(label: &str, mut scored: Vec<(f64, bool)>) {
    if scored.len() < 20 {
        println!(
            "DIAG   {label}: only {} trades, too few to split",
            scored.len()
        );
        return;
    }
    scored.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());
    let mid = scored.len() / 2;
    let wr =
        |s: &[(f64, bool)]| s.iter().filter(|(_, w)| *w).count() as f64 / s.len() as f64 * 100.0;
    let (lo, hi) = (&scored[..mid], &scored[mid..]);
    println!(
        "DIAG   {label:<28} low half {:>5.1}% (n={})   high half {:>5.1}% (n={})   gap {:+5.1}pp",
        wr(lo),
        lo.len(),
        wr(hi),
        hi.len(),
        wr(hi) - wr(lo)
    );
}

#[tokio::test]
#[ignore = "slow diagnostic; run explicitly"]
async fn diag_volume_and_funding_vs_outcome() {
    if !std::path::Path::new(DB).exists() {
        println!("DIAG skipped");
        return;
    }
    let db = HistoryDb::open_local(DB).await.expect("db");
    // The ORIGINAL eight symbols, so this reads the trades already studied.
    let symbols: Vec<Symbol> = [
        "ADAUSDT", "BNBUSDT", "BTCUSDT", "DOGEUSDT", "ETHUSDT", "LINKUSDT", "SOLUSDT", "XRPUSDT",
    ]
    .iter()
    .map(|s| Symbol::new(*s))
    .collect();

    let series = db.stored_series().await.expect("series");
    let start = series
        .iter()
        .filter(|(s, _, _, _)| symbols.contains(s))
        .map(|(_, _, e, _)| *e)
        .max()
        .unwrap();
    let end = series
        .iter()
        .filter(|(s, _, _, _)| symbols.contains(s))
        .map(|(_, _, _, l)| *l)
        .min()
        .unwrap();

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

    let r = run_backtest(
        &db,
        &cfg,
        Box::new(IctStrategy::new(IctParams::liquidity_sweep_v1())),
        RiskManager::new(RiskParams::defaults(), dec!(0.3)),
    )
    .await
    .expect("run");
    let trades: Vec<ClosedTrade> = r.trades;
    let wins = trades
        .iter()
        .filter(|t| t.exit_reason == ExitReason::Target)
        .count();
    println!(
        "DIAG {} trades over the FULL range, {} winners ({:.1}%)\n",
        trades.len(),
        wins,
        wins as f64 / trades.len() as f64 * 100.0
    );

    // --- volume at entry, relative to the trailing 20-candle average ---
    let mut vol_scored = Vec::new();
    let mut fund_scored = Vec::new();
    for t in &trades {
        let win = t.exit_reason == ExitReason::Target;

        let h4 = db
            .candles_in_range(
                &t.symbol,
                Timeframe::H4,
                t.entry_ms - 21 * 4 * 3_600_000,
                t.entry_ms,
            )
            .await
            .expect("h4");
        if h4.len() >= 5 {
            let n = h4.len();
            let recent = &h4[..n - 1];
            let avg: Decimal =
                recent.iter().map(|c| c.volume).sum::<Decimal>() / Decimal::from(recent.len());
            if avg > Decimal::ZERO {
                let rel = (h4[n - 1].volume / avg).to_f64().unwrap_or(1.0);
                vol_scored.push((rel, win));
            }
        }

        let f = db
            .funding_in_range(&t.symbol, t.entry_ms - 24 * 3_600_000, t.entry_ms)
            .await
            .expect("funding");
        if let Some(last) = f.last() {
            // Signed by trade direction: positive means the position was on
            // the side that PAYS, i.e. the crowded side.
            let signed = match t.side {
                botcore::Side::Buy => last.rate,
                botcore::Side::Sell => -last.rate,
            };
            fund_scored.push((signed.to_f64().unwrap_or(0.0), win));
        }
    }

    println!("DIAG --- does either separate winners from losers? ---");
    split_report("volume vs 20-candle avg", vol_scored);
    split_report("funding, signed by side", fund_scored);
    println!("\nDIAG A gap near zero means no information. A large, consistent gap");
    println!("DIAG would be the first independent signal found in this project.");
}
