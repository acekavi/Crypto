// DIAGNOSTIC — exploratory, NOT the pre-registered study. Measures how the
// session filter and the reward multiple change setup frequency.
use botcore::{Instrument, Symbol, Timeframe};
use history::HistoryDb;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use strategy::ict::{IctParams, IctStrategy};
use strategy::{MarketContext, Strategy};

const DB: &str = "/var/home/acekavi/Projects/Crypto/data/history.db";
const HOLDOUT_DAYS: i64 = 330;
const DAY: i64 = 86_400_000;

/// One symbol's candles across every timeframe, pre-merged and pre-sorted so
/// the configuration sweep below can replay them many times without redoing
/// the load.
type SymbolTicks = (Symbol, Instrument, Vec<(Timeframe, botcore::Candle)>);

#[tokio::test]
// Diagnostic, not a guarantee: it replays the whole research window and takes
// minutes in a debug build. Run explicitly with
//   cargo test --release -p <crate> --test <name> -- --ignored --nocapture
#[ignore = "slow diagnostic; run explicitly"]
async fn diag_ict_session_and_rr() {
    if !std::path::Path::new(DB).exists() {
        println!("DIAG skipped: {DB} not present");
        return;
    }
    let db = HistoryDb::open_local(DB).await.expect("db");
    let series = db.stored_series().await.expect("series");
    let start = series.iter().map(|(_, _, e, _)| *e).max().unwrap();
    // Research window only. The holdout stays sealed for exploration too —
    // especially for exploration, since this is where overfitting starts.
    let end = series.iter().map(|(_, _, _, l)| *l).min().unwrap() - HOLDOUT_DAYS * DAY;

    let mut symbols: Vec<Symbol> = series.iter().map(|(s, _, _, _)| s.clone()).collect();
    symbols.sort();
    symbols.dedup();

    // Pre-load once; the inner loop runs many configurations over it.
    let mut per_symbol: Vec<SymbolTicks> = Vec::new();
    for sym in &symbols {
        let inst = Instrument {
            symbol: sym.clone(),
            tick_size: dec!(0.0001),
            qty_step: dec!(0.000001),
            min_order_qty: dec!(0.000001),
            min_notional: dec!(5),
            launch_time_ms: 0,
        };
        let mut ticks = Vec::new();
        for tf in [Timeframe::M15, Timeframe::H1, Timeframe::H4, Timeframe::D1] {
            for c in db.candles_in_range(sym, tf, start, end).await.expect("c") {
                ticks.push((tf, c));
            }
        }
        ticks.sort_by_key(|(tf, c)| (c.open_time_ms, tf.duration_ms()));
        per_symbol.push((sym.clone(), inst, ticks));
    }

    println!(
        "DIAG window {} days, {} symbols\n",
        (end - start) / DAY,
        symbols.len()
    );

    let base = IctParams::declared_variants();
    for (label, session, rr) in [
        ("registered  (NY, 1:2)", true, Decimal::TWO),
        ("no session  (24h, 1:2)", false, Decimal::TWO),
        ("registered  (NY, 1:3)", true, Decimal::from(3)),
        ("no session  (24h, 1:3)", false, Decimal::from(3)),
    ] {
        for (name, p) in &base {
            let params = IctParams {
                session_filter: session,
                reward_multiple: rr,
                ..p.clone()
            };
            let mut strat = IctStrategy::new(params);
            for (sym, inst, ticks) in &per_symbol {
                for (tf, c) in ticks {
                    strat.on_candle_close(&MarketContext {
                        symbol: sym,
                        timeframe: *tf,
                        candle: c,
                        instrument: inst,
                    });
                }
            }
            let f = strat.funnel();
            println!(
                "DIAG {label} variant {name}: eligible={:>7} biasOK={:>5} fvg={:>4} SIGNALS={:>4}",
                f.in_session, f.bias_aligned, f.fvg_found, f.signals
            );
        }
        println!();
    }
}
