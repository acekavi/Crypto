use botcore::{Candle, Symbol, Timeframe};
use exchange::bybit::rest::BybitRest;
use tracing::info;

use crate::db::{HistoryDb, HistoryError};
use crate::gaps::{Gap, find_gaps};

/// What one `download_symbol` call did: how many candles it wrote and any
/// holes left in the range it was asked to cover.
///
/// `gaps` is the ONLY place a missing candle is ever surfaced. A gap is
/// recoverable by a later run; a synthesised candle is not — it would
/// silently corrupt every backtest that runs over it — so nothing in this
/// module ever fabricates one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DownloadReport {
    pub symbol: Symbol,
    pub timeframe: Timeframe,
    pub candles_written: usize,
    pub gaps: Vec<Gap>,
}

/// The portion(s) of `[start_ms, end_ms]` not already covered by `recorded`
/// (the current `download_ranges` envelope), as at most one segment before
/// it and one after it.
///
/// `download_ranges` tracks a single contiguous `(earliest_ms, latest_ms)`
/// envelope, not a set of covered sub-ranges, so a hole *inside* an already
/// recorded envelope is deliberately not re-fetched here — that is exactly
/// what `find_gaps` exists to catch and report after the write, rather than
/// this function silently re-requesting data a caller may not expect.
fn missing_segments(
    start_ms: i64,
    end_ms: i64,
    recorded: Option<(i64, i64)>,
    step_ms: i64,
) -> Vec<(i64, i64)> {
    let Some((earliest_ms, latest_ms)) = recorded else {
        return vec![(start_ms, end_ms)];
    };

    let mut segments = Vec::new();
    if start_ms < earliest_ms {
        let seg_end = (earliest_ms - step_ms).min(end_ms);
        if start_ms <= seg_end {
            segments.push((start_ms, seg_end));
        }
    }
    if end_ms > latest_ms {
        let seg_start = (latest_ms + step_ms).max(start_ms);
        if seg_start <= end_ms {
            segments.push((seg_start, end_ms));
        }
    }
    segments
}

/// Downloads and stores `[start_ms, end_ms]` for `symbol`/`tf`, fetching only
/// what `recorded_range` says is missing so a resumed run never re-requests
/// data it already holds.
///
/// Generic over the fetch step so resumability, gap reporting, and
/// partial-failure bookkeeping can all be exercised against a fake fetcher in
/// tests, without a real network call — mirroring how
/// `exchange::bybit::rest::walk_pages_newest_first` separates pagination
/// logic from HTTP. `download_symbol` below is the concrete, network-backed
/// entry point built on top of this.
pub async fn download_symbol_with<F, Fut>(
    db: &HistoryDb,
    symbol: &Symbol,
    tf: Timeframe,
    start_ms: i64,
    end_ms: i64,
    mut fetch: F,
) -> Result<DownloadReport, HistoryError>
where
    F: FnMut(i64, i64) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<Candle>, HistoryError>>,
{
    let recorded = db.recorded_range(symbol, tf).await?;
    let segments = missing_segments(start_ms, end_ms, recorded, tf.duration_ms());

    let mut candles_written = 0usize;
    // One fetch + one `insert_candles` per missing segment, sequentially: if
    // a later segment's fetch fails, everything a prior segment already
    // wrote stays committed and `recorded_range` keeps reflecting exactly
    // that — `insert_candles` updates the candle rows and `download_ranges`
    // in the same transaction, so there is nothing left half-written for a
    // segment that does complete.
    for (seg_start, seg_end) in segments {
        let candles = fetch(seg_start, seg_end).await?;
        db.insert_candles(symbol, tf, &candles).await?;
        info!(
            %symbol, ?tf, seg_start, seg_end,
            count = candles.len(),
            "wrote history segment"
        );
        candles_written += candles.len();
    }

    let stored = db.candles_in_range(symbol, tf, start_ms, end_ms).await?;
    let gaps = find_gaps(&stored, tf);

    Ok(DownloadReport {
        symbol: symbol.clone(),
        timeframe: tf,
        candles_written,
        gaps,
    })
}

/// Concrete, network-backed entry point: fetches via `BybitRest::klines_range`
/// (which already pages, rate-limits, and retries) for whatever
/// `download_symbol_with` determines is missing.
pub async fn download_symbol(
    rest: &BybitRest,
    db: &HistoryDb,
    symbol: &Symbol,
    tf: Timeframe,
    start_ms: i64,
    end_ms: i64,
) -> Result<DownloadReport, HistoryError> {
    download_symbol_with(db, symbol, tf, start_ms, end_ms, |seg_start, seg_end| {
        let symbol = symbol.clone();
        async move {
            rest.klines_range(&symbol, tf, seg_start, seg_end)
                .await
                .map_err(|e| HistoryError::Exchange(e.to_string()))
        }
    })
    .await
}
