//! Guards the spec's central determinism rule: no file under
//! `crates/backtest/src/` may read the wall clock.
//!
//! This is a source-level test, in the style of
//! `crates/exchange/tests/no_market_orders.rs`, rather than a behavioural one
//! because the rule is about what the crate cannot express, not about what
//! one function happens to return at runtime. Every timestamp a backtest
//! reasons about must come from a candle or a funding rate, never from the
//! clock the process happens to run on — a single wall-clock read would make
//! two runs of the same replay disagree, silently, which is exactly the
//! property every comparison in Plan 2c depends on not happening.

use std::fs;
use std::path::Path;

fn rust_sources(dir: &Path, out: &mut Vec<(String, String)>) {
    for entry in fs::read_dir(dir).expect("readable directory") {
        let entry = entry.expect("readable entry");
        let path = entry.path();
        if path.is_dir() {
            if path.file_name().is_some_and(|n| n == "target") {
                continue;
            }
            rust_sources(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            let text = fs::read_to_string(&path).expect("readable source");
            out.push((path.display().to_string(), text));
        }
    }
}

/// Only `crates/backtest/src/` — not its tests, and not the rest of the
/// workspace, which live code (the live bot) legitimately reads the wall
/// clock from.
fn backtest_src_sources() -> Vec<(String, String)> {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut out = Vec::new();
    rust_sources(&root, &mut out);
    out
}

#[test]
fn no_backtest_source_file_reads_the_wall_clock() {
    let offenders: Vec<(String, &'static str)> = backtest_src_sources()
        .into_iter()
        .flat_map(|(path, text)| {
            ["SystemTime::now", "Instant::now", "Utc::now"]
                .into_iter()
                .filter(|needle| text.contains(needle))
                .map(|needle| (path.clone(), needle))
                .collect::<Vec<_>>()
        })
        .collect();

    assert!(
        offenders.is_empty(),
        "wall-clock read found in: {offenders:?}\n\
         crates/backtest/src/ must derive every timestamp from replayed data \
         (a candle's open time, a funding rate's timestamp), never from the \
         clock — a single wall-clock call would make two runs of the same \
         replay disagree silently."
    );
}
