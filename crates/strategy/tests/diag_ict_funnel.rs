// DIAGNOSTIC — where do ICT setups die? Not part of the suite's guarantees.
// Reads the real research-window data and drives the real strategy.
use botcore::{Instrument, Symbol, Timeframe};
use history::HistoryDb;
use rust_decimal_macros::dec;
use strategy::ict::{IctParams, IctStrategy};
use strategy::{MarketContext, Strategy};

const DB: &str = "/var/home/acekavi/Projects/Crypto/data/history.db";
const HOLDOUT_DAYS: i64 = 330;
const DAY: i64 = 86_400_000;

#[tokio::test]
async fn diag_ict_funnel() {
    // Skips rather than fails when the history database is absent, so a fresh
    // checkout is not broken by a diagnostic that needs downloaded data.
    if !std::path::Path::new(DB).exists() {
        println!("DIAG skipped: {DB} not present");
        return;
    }
    let db = HistoryDb::open_local(DB).await.expect("db");
    let series = db.stored_series().await.expect("series");
    let full_start = series.iter().map(|(_, _, e, _)| *e).max().unwrap();
    let full_end = series.iter().map(|(_, _, _, l)| *l).min().unwrap();
    // Research window only — the holdout stays sealed even for diagnostics.
    let end = full_end - HOLDOUT_DAYS * DAY;

    let mut symbols: Vec<Symbol> = series.iter().map(|(s, _, _, _)| s.clone()).collect();
    symbols.sort();
    symbols.dedup();
    println!(
        "DIAG window {} .. {} ({} days), {} symbols",
        full_start,
        end,
        (end - full_start) / DAY,
        symbols.len()
    );

    for (name, params) in IctParams::declared_variants() {
        let mut strat = IctStrategy::new(params.clone());

        for sym in &symbols {
            let inst = Instrument {
                symbol: sym.clone(),
                tick_size: dec!(0.0001),
                qty_step: dec!(0.000001),
                min_order_qty: dec!(0.000001),
                launch_time_ms: 0,
            };
            // Merge all four timeframes chronologically, finest first on ties,
            // exactly as the replay driver orders them.
            let mut ticks: Vec<(Timeframe, botcore::Candle)> = Vec::new();
            for tf in [Timeframe::M15, Timeframe::H1, Timeframe::H4, Timeframe::D1] {
                for c in db
                    .candles_in_range(sym, tf, full_start, end)
                    .await
                    .expect("candles")
                {
                    ticks.push((tf, c));
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

        // REGRESSION GUARD, not just a print. The expiry check once sat AFTER
        // the bias comparison, so a setup whose bias never aligned was never
        // cleared: it stayed armed indefinitely and could fire months after
        // the sweep that justified it. That showed up here as 54 armed setups
        // accounting for 16,116 armed-and-in-session candles.
        //
        // The bound is logical, not an arbitrary multiple: a setup lives at
        // most `mss_window` H1 candles, which is 4x that many M15 candles, so
        // it can never be observed in MORE in-session M15 candles than that.
        // Observed correct value is ~12 against a bound of 48; the buggy
        // ordering gave ~298.
        if f.mss_armed > 0 {
            let per_setup = f.setup_active as f64 / f.mss_armed as f64;
            let ceiling = (params.mss_window * 4) as f64;
            assert!(
                per_setup < ceiling,
                "variant {name}: {per_setup:.0} armed-and-in-session candles per setup \
                 exceeds {ceiling:.0}; expiry is not clearing stale setups"
            );
        }

        println!(
            "DIAG {name}: h1={} sweeps={} armed={} | m15={} inSession={} setupActive={} biasOK={} notExpired={} fvg={} SIGNALS={}",
            f.h1_candles,
            f.sweeps,
            f.mss_armed,
            f.m15_candles,
            f.in_session,
            f.setup_active,
            f.bias_aligned,
            f.not_expired,
            f.fvg_found,
            f.signals
        );
    }
}
