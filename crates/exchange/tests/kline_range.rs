// Pure page-walking logic, independent of HTTP.
// Given a fetcher that returns pages, it must:
//  - assemble multiple pages into one ascending, deduplicated series
//  - stop when a page comes back empty
//  - stop when the oldest candle reaches start_ms
//  - never loop forever when the API returns the same page repeatedly
//  - exclude candles outside the requested range

use std::sync::atomic::{AtomicUsize, Ordering};

use botcore::Candle;
use exchange::bybit::rest::walk_kline_pages;
use exchange::bybit::transport::ExchangeError;
use rust_decimal_macros::dec;

const H1: i64 = 3_600_000;

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

/// Bybit returns klines newest-first (confirmed live in Step 1 of the plan),
/// so a fake page must do the same for the walker's cursor logic to exercise
/// realistic input.
fn page_newest_first(times: &[i64]) -> Vec<Candle> {
    let mut times = times.to_vec();
    times.sort_by(|a, b| b.cmp(a));
    times.into_iter().map(candle).collect()
}

#[tokio::test]
async fn a_range_needing_three_pages_assembles_all_candles_ascending_with_no_duplicates() {
    // 12 hourly candles, 5 per page -> 3 pages (5 + 5 + 2), with the last
    // request overlapping the one before it, which real pagination does too.
    let all_times: Vec<i64> = (0..12).map(|i| i * H1).collect();
    let calls = AtomicUsize::new(0);

    let result = walk_kline_pages(0, 11 * H1, |_start, end| {
        calls.fetch_add(1, Ordering::SeqCst);
        let page: Vec<i64> = all_times
            .iter()
            .copied()
            .filter(|&t| t <= end)
            .rev()
            .take(5)
            .collect();
        async move { Ok(page_newest_first(&page)) }
    })
    .await
    .expect("walk succeeds");

    assert_eq!(calls.load(Ordering::SeqCst), 3, "expected exactly 3 pages");
    let times: Vec<i64> = result.iter().map(|c| c.open_time_ms).collect();
    assert_eq!(times, all_times, "ascending, complete, no duplicates");
}

#[tokio::test]
async fn an_empty_page_terminates_the_walk() {
    let result = walk_kline_pages(0, 10 * H1, |_start, _end| async { Ok(Vec::new()) })
        .await
        .expect("walk succeeds");
    assert!(result.is_empty());
}

#[tokio::test]
async fn a_stuck_api_returning_the_same_page_forever_terminates() {
    // The one that matters: an API that ignores the cursor and keeps
    // returning the identical page must not spin the loop forever.
    let calls = AtomicUsize::new(0);
    let fixed_page = page_newest_first(&[5 * H1, 6 * H1, 7 * H1]);

    let result = walk_kline_pages(0, 100 * H1, |_start, _end| {
        calls.fetch_add(1, Ordering::SeqCst);
        let page = fixed_page.clone();
        async move { Ok(page) }
    })
    .await
    .expect("walk terminates instead of hanging");

    // Two calls: the first makes progress-looking output, the second detects
    // the cursor failed to move and stops. It must not be unbounded.
    assert!(
        calls.load(Ordering::SeqCst) <= 2,
        "looped {} times against a stuck fetcher",
        calls.load(Ordering::SeqCst)
    );
    let times: Vec<i64> = result.iter().map(|c| c.open_time_ms).collect();
    assert_eq!(times, vec![5 * H1, 6 * H1, 7 * H1], "deduplicated");
}

#[tokio::test]
async fn candles_outside_the_requested_range_are_excluded() {
    // The fetcher hands back a page wider than the requested window (Bybit's
    // start/end are advisory on some responses; the walker must not trust
    // the page to already be trimmed).
    let start_ms = 2 * H1;
    let end_ms = 4 * H1;
    let result = walk_kline_pages(start_ms, end_ms, |_start, _end| {
        let page = page_newest_first(&[0, H1, 2 * H1, 3 * H1, 4 * H1, 5 * H1, 6 * H1]);
        async move { Ok(page) }
    })
    .await
    .expect("walk succeeds");

    let times: Vec<i64> = result.iter().map(|c| c.open_time_ms).collect();
    assert_eq!(times, vec![2 * H1, 3 * H1, 4 * H1]);
}

#[tokio::test]
async fn a_fetch_error_propagates_instead_of_being_swallowed() {
    let err = walk_kline_pages(0, H1, |_start, _end| async {
        Err(ExchangeError::Decode("boom".into()))
    })
    .await
    .expect_err("fetch error must propagate");
    assert!(matches!(err, ExchangeError::Decode(_)));
}
