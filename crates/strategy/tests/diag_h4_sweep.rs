// DIAGNOSTIC — exploratory. Counts setups for the H4-sweep / 5m-FVG variant.
use botcore::{Instrument, Symbol, Timeframe};
use history::HistoryDb;
use rust_decimal_macros::dec;
use strategy::ict::{IctParams, IctStrategy};
use strategy::{MarketContext, Strategy};

const DB: &str = "/var/home/acekavi/Projects/Crypto/data/history.db";
const DAY: i64 = 86_400_000;

#[tokio::test]
#[ignore = "slow diagnostic; run explicitly"]
async fn diag_h4_sweep_counts() {
    if !std::path::Path::new(DB).exists() {
        println!("DIAG skipped");
        return;
    }
    let db = HistoryDb::open_local(DB).await.expect("db");
    let series = db.stored_series().await.expect("series");
    let start = series.iter().map(|(_, _, e, _)| *e).max().unwrap();
    // Research window only; the holdout stays sealed even for counting.
    let end = series.iter().map(|(_, _, _, l)| *l).min().unwrap() - 330 * DAY;

    let mut symbols: Vec<Symbol> = series.iter().map(|(s, _, _, _)| s.clone()).collect();
    symbols.sort();
    symbols.dedup();

    let configs = [
        (
            "registered  H1 sweep / M15 / MSS / 1:2",
            IctParams::variant_a(),
        ),
        (
            "requested   H4 sweep / M5  / noMSS / 1:3",
            IctParams::h4_sweep_m5_entry(),
        ),
        (
            "H4 sweep / M15 / noMSS / 1:3",
            IctParams {
                execution_tf: Timeframe::M15,
                ..IctParams::h4_sweep_m5_entry()
            },
        ),
        (
            "H1 sweep / M5  / noMSS / 1:3",
            IctParams {
                structure_tf: Timeframe::H1,
                ..IctParams::h4_sweep_m5_entry()
            },
        ),
    ];

    println!(
        "DIAG window {} days, {} symbols\n",
        (end - start) / DAY,
        symbols.len()
    );

    for (label, params) in configs {
        let mut strat = IctStrategy::new(params.clone());
        let tfs: Vec<Timeframe> = strat.timeframes().to_vec();
        for sym in &symbols {
            let inst = Instrument {
                symbol: sym.clone(),
                tick_size: dec!(0.0001),
                qty_step: dec!(0.000001),
                min_order_qty: dec!(0.000001),
                launch_time_ms: 0,
            };
            let mut ticks: Vec<(Timeframe, botcore::Candle)> = Vec::new();
            for tf in &tfs {
                for c in db.candles_in_range(sym, *tf, start, end).await.expect("c") {
                    ticks.push((*tf, c));
                }
            }
            ticks.sort_by_key(|(tf, c)| (c.open_time_ms, tf.duration_ms()));
            for (tf, c) in &ticks {
                strat.on_candle_close(&MarketContext {
                    symbol: sym,
                    timeframe: *tf,
                    candle: c,
                    instrument: &inst,
                });
            }
        }
        let f = strat.funnel();
        println!(
            "DIAG {label}\n      sweeps={:<6} armed={:<6} execCandles={:<8} biasOK={:<6} fvg={:<5} SIGNALS={}",
            f.sweeps, f.mss_armed, f.m15_candles, f.bias_aligned, f.fvg_found, f.signals
        );
    }
}
