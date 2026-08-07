use std::sync::{Arc, Mutex};

use botcore::{Candle, Symbol, Timeframe};
use history::download::download_symbol_with;
use history::{Gap, HistoryDb, HistoryError};
use rust_decimal_macros::dec;

const H1: i64 = 3_600_000;

async fn temp_db() -> (HistoryDb, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("h.db");
    let db = HistoryDb::open_local(path.to_str().unwrap())
        .await
        .expect("opens");
    (db, dir)
}

/// Only `open_time_ms` matters here; the price fields are filler.
fn candle(open_time_ms: i64) -> Candle {
    Candle {
        open_time_ms,
        open: dec!(100),
        high: dec!(101),
        low: dec!(99),
        close: dec!(100),
        volume: dec!(10),
        turnover: dec!(1000),
    }
}

fn candles_between(start_ms: i64, end_ms: i64) -> Vec<Candle> {
    (start_ms..=end_ms)
        .step_by(H1 as usize)
        .map(candle)
        .collect()
}

#[tokio::test]
async fn a_resumed_download_fetches_only_the_missing_portion() {
    // THE WHOLE POINT OF `download_ranges`: a symbol already downloaded up to
    // some point must not have that portion re-requested on a resumed run.
    let (db, _dir) = temp_db().await;
    let sym = Symbol::new("BTCUSDT");

    db.insert_candles(&sym, Timeframe::H1, &candles_between(0, 10 * H1))
        .await
        .expect("seed");

    let calls: Arc<Mutex<Vec<(i64, i64)>>> = Arc::new(Mutex::new(Vec::new()));
    let calls_seen = Arc::clone(&calls);

    let report = download_symbol_with(
        &db,
        &sym,
        Timeframe::H1,
        0,
        20 * H1,
        move |seg_start, seg_end| {
            calls_seen.lock().unwrap().push((seg_start, seg_end));
            async move { Ok(candles_between(seg_start, seg_end)) }
        },
    )
    .await
    .expect("download succeeds");

    assert_eq!(
        *calls.lock().unwrap(),
        vec![(11 * H1, 20 * H1)],
        "the fetcher must be asked only for the missing portion, not the whole range"
    );
    assert_eq!(
        report.candles_written, 10,
        "only the 10 newly fetched candles, not the 11 already stored"
    );

    let stored = db
        .candles_in_range(&sym, Timeframe::H1, 0, 20 * H1)
        .await
        .expect("query");
    assert_eq!(stored.len(), 21, "the full requested range is now covered");
}

#[tokio::test]
async fn a_gap_in_the_fetched_data_is_reported_never_filled_by_interpolation() {
    let (db, _dir) = temp_db().await;
    let sym = Symbol::new("BTCUSDT");

    // The fetcher's response skips the candle at 3*H1 — a genuine hole in
    // what the exchange returned, e.g. an outage on Bybit's side.
    let report = download_symbol_with(&db, &sym, Timeframe::H1, 0, 5 * H1, |seg_start, seg_end| {
        let fetched: Vec<Candle> = candles_between(seg_start, seg_end)
            .into_iter()
            .filter(|c| c.open_time_ms != 3 * H1)
            .collect();
        async move { Ok(fetched) }
    })
    .await
    .expect("download succeeds despite the gap");

    assert_eq!(
        report.gaps,
        vec![Gap {
            from_ms: 2 * H1,
            to_ms: 4 * H1
        }],
        "the hole must be reported in DownloadReport.gaps"
    );

    let at_gap = db
        .candles_in_range(&sym, Timeframe::H1, 3 * H1, 3 * H1)
        .await
        .expect("query");
    assert!(
        at_gap.is_empty(),
        "no candle must ever be invented at a reported gap"
    );
}

#[tokio::test]
async fn a_failure_partway_leaves_recorded_range_reflecting_only_what_was_written() {
    // A crash partway through a download must not corrupt the bookkeeping
    // into claiming a range it does not actually hold.
    let (db, _dir) = temp_db().await;
    let sym = Symbol::new("BTCUSDT");

    // Pre-seed the middle of the requested range, so it splits into a
    // segment BEFORE the seeded data and one AFTER it — two fetches, run in
    // that order.
    db.insert_candles(&sym, Timeframe::H1, &candles_between(10 * H1, 20 * H1))
        .await
        .expect("seed");

    let err = download_symbol_with(
        &db,
        &sym,
        Timeframe::H1,
        0,
        30 * H1,
        |seg_start, seg_end| async move {
            if seg_start == 0 {
                // The "before" segment succeeds and gets written...
                Ok(candles_between(seg_start, seg_end))
            } else {
                // ...but the "after" segment fails, simulating the process
                // dying partway through the download.
                Err(HistoryError::Exchange("simulated network failure".into()))
            }
        },
    )
    .await
    .expect_err("the failed segment's error must propagate");
    assert!(matches!(err, HistoryError::Exchange(_)));

    let recorded = db.recorded_range(&sym, Timeframe::H1).await.expect("query");
    assert_eq!(
        recorded,
        Some((0, 20 * H1)),
        "earliest_ms extends to cover the segment that DID write; latest_ms must \
         NOT advance past what was actually written"
    );

    let after_failure = db
        .candles_in_range(&sym, Timeframe::H1, 21 * H1, 30 * H1)
        .await
        .expect("query");
    assert!(
        after_failure.is_empty(),
        "a crash must not leave phantom candles behind for the segment that failed"
    );
}
