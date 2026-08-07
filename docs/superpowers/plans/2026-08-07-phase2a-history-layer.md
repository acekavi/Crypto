# Phase 2a: History Layer Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Download and store Bybit historical klines and funding rates in a local database, detect gaps, and reconstruct the historical trading universe from recorded turnover.

**Architecture:** A new `crates/history` owns storage and orchestration. All Bybit HTTP stays in `crates/exchange` (its `get` is `pub(crate)`, and duplicating the transport would fork the rate limiting and retry logic). `crates/history` depends on `crates/exchange` and calls new inherent range-fetch methods on `BybitRest`.

**Tech Stack:** Rust edition 2024, `turso` (libSQL) for storage matching `crates/persistence`, `rust_decimal::Decimal` for all prices and turnover, `tokio`, `async_trait`.

## Global Constraints

- Money, prices, quantities and turnover are `rust_decimal::Decimal`. **Never `f64`.**
- Decimals persist as **TEXT**, never SQLite REAL. Compare numerically in Rust, never lexicographically in SQL.
- The domain crate is `botcore`, never `core` (which shadows Rust's sysroot crate and breaks `thiserror` derives).
- No quoted `"Market"` literal anywhere in the workspace. `cargo test -p exchange --test no_market_orders` must keep passing.
- `dec!` (`rust_decimal_macros`) in **test code only**.
- Rust edition 2024, rust-version 1.97.
- Comments explain **why**, not what. Match the density and voice of the surrounding code.
- **Never run bare `cargo test` / `cargo test --workspace` without checking it completes.** Always wrap cargo commands in `timeout 600`.
- Per-crate formatting only: `cargo fmt -p <crate>`, never `--all`.
- No AI-attribution trailers in commits. Commit with:
  `git -c user.email=avishkakavinda@proton.me -c user.name=acekavi commit`

## File Structure

| File | Responsibility |
|---|---|
| `crates/history/Cargo.toml` | New crate manifest |
| `crates/history/src/lib.rs` | Re-exports |
| `crates/history/src/schema.rs` | `MIGRATIONS` statements |
| `crates/history/src/db.rs` | `HistoryDb`: open, migrate, candle + funding storage, range bookkeeping |
| `crates/history/src/gaps.rs` | Gap detection over a stored range |
| `crates/history/src/universe.rs` | Historical universe reconstruction from turnover |
| `crates/history/src/download.rs` | Resumable download orchestration |
| `crates/exchange/src/bybit/rest.rs` | **Modify** — add `klines_range`, `funding_history` |
| `crates/exchange/src/bybit/wire.rs` | **Modify** — add funding-rate wire types |
| `bot/src/bin/download_history.rs` | `download-history` binary |
| `Cargo.toml` | **Modify** — add `crates/history` to workspace members |

---

### Task 1: Crate scaffold, schema, and open/migrate

**Files:**
- Create: `crates/history/Cargo.toml`, `crates/history/src/lib.rs`, `crates/history/src/schema.rs`, `crates/history/src/db.rs`
- Modify: `Cargo.toml` (workspace members)
- Test: `crates/history/tests/db_roundtrip.rs`

**Interfaces:**
- Consumes: nothing (first task)
- Produces:
  - `HistoryDb::open_local(path: &str) -> Result<HistoryDb, HistoryError>`
  - `pub enum HistoryError { Db(String), Decode(String) }`

Read `crates/persistence/src/schema.rs` and `crates/persistence/src/journal.rs` first and follow their structure exactly — same `turso` version, same `MIGRATIONS: &[&str]` const shape, same `From<turso::Error>` impl, same TEXT-for-decimals rule.

- [ ] **Step 1: Add the crate to the workspace**

In root `Cargo.toml`, append `"crates/history"` to `members`.

- [ ] **Step 2: Write `crates/history/Cargo.toml`**

```toml
[package]
name = "history"
version = "0.1.0"
edition.workspace = true
rust-version.workspace = true

[dependencies]
botcore = { path = "../botcore" }
exchange = { path = "../exchange" }
rust_decimal.workspace = true
thiserror.workspace = true
tokio.workspace = true
tracing.workspace = true
# NOT a workspace dependency — `crates/persistence` declares it directly and
# this must match it exactly, or two libSQL versions end up linked at once.
turso = { version = "0.7.2", features = ["sync"] }

[dev-dependencies]
rust_decimal_macros.workspace = true
tempfile = "3"
```

- [ ] **Step 3: Write the failing test**

`crates/history/tests/db_roundtrip.rs`:

```rust
use history::HistoryDb;

async fn temp_db() -> (HistoryDb, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("h.db");
    let db = HistoryDb::open_local(path.to_str().unwrap())
        .await
        .expect("opens");
    (db, dir)
}

#[tokio::test]
async fn opening_creates_the_schema_and_is_idempotent() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("h.db");
    let p = path.to_str().unwrap();

    // Opening twice must not fail: every migration is CREATE TABLE IF NOT
    // EXISTS, so a restart against an existing database is normal.
    let _first = HistoryDb::open_local(p).await.expect("first open");
    let _second = HistoryDb::open_local(p).await.expect("second open");
}

#[tokio::test]
async fn a_fresh_database_holds_no_candles() {
    let (db, _dir) = temp_db().await;
    assert_eq!(db.candle_count().await.expect("count"), 0);
}
```

- [ ] **Step 4: Run it to verify it fails**

`timeout 600 cargo test -p history` — expected: does not compile, `HistoryDb` not found.

- [ ] **Step 5: Write `schema.rs`**

```rust
/// Applied in order on every open. Each is `IF NOT EXISTS`, so opening an
/// existing database is a no-op rather than an error.
pub const MIGRATIONS: &[&str] = &[
    // Decimals are TEXT for the same reason as the trading journal: SQLite
    // REAL is binary floating point and would silently round prices.
    "CREATE TABLE IF NOT EXISTS candles (
        symbol TEXT NOT NULL,
        timeframe TEXT NOT NULL,
        open_time_ms INTEGER NOT NULL,
        open TEXT NOT NULL,
        high TEXT NOT NULL,
        low TEXT NOT NULL,
        close TEXT NOT NULL,
        volume TEXT NOT NULL,
        turnover TEXT NOT NULL,
        PRIMARY KEY (symbol, timeframe, open_time_ms)
    )",
    "CREATE TABLE IF NOT EXISTS funding_rates (
        symbol TEXT NOT NULL,
        funding_time_ms INTEGER NOT NULL,
        rate TEXT NOT NULL,
        PRIMARY KEY (symbol, funding_time_ms)
    )",
    // What has actually been fetched, so an interrupted download resumes
    // instead of restarting or leaving an unnoticed hole.
    "CREATE TABLE IF NOT EXISTS download_ranges (
        symbol TEXT NOT NULL,
        timeframe TEXT NOT NULL,
        earliest_ms INTEGER NOT NULL,
        latest_ms INTEGER NOT NULL,
        PRIMARY KEY (symbol, timeframe)
    )",
];
```

- [ ] **Step 6: Write `db.rs` with `open_local`, `migrate`, `candle_count`**

Mirror `Journal::open_local` / `Journal::migrate` exactly. `candle_count` is `SELECT COUNT(*) FROM candles`.

- [ ] **Step 7: Write `lib.rs`**

```rust
pub mod db;
pub mod schema;

pub use db::{HistoryDb, HistoryError};
```

- [ ] **Step 8: Run tests to verify they pass**

`timeout 600 cargo test -p history`

- [ ] **Step 9: Commit**

```bash
cargo fmt -p history
git add Cargo.toml Cargo.lock crates/history
git -c user.email=avishkakavinda@proton.me -c user.name=acekavi commit -m "feat(history): crate scaffold and history database schema"
```

---

### Task 2: Candle storage — idempotent batch insert and range query

**Files:**
- Modify: `crates/history/src/db.rs`
- Test: `crates/history/tests/db_roundtrip.rs`

**Interfaces:**
- Consumes: `HistoryDb`, `HistoryError` from Task 1
- Produces:
  - `HistoryDb::insert_candles(&self, symbol: &Symbol, tf: Timeframe, candles: &[Candle]) -> Result<(), HistoryError>`
  - `HistoryDb::candles_in_range(&self, symbol: &Symbol, tf: Timeframe, start_ms: i64, end_ms: i64) -> Result<Vec<Candle>, HistoryError>` — inclusive of both bounds, ordered ascending by `open_time_ms`
  - `HistoryDb::recorded_range(&self, symbol: &Symbol, tf: Timeframe) -> Result<Option<(i64, i64)>, HistoryError>`

`Candle` is `botcore::Candle { open_time_ms, open, high, low, close, volume, turnover }`. `Timeframe::as_bybit_interval()` gives `"60"`/`"240"` — use that as the stored `timeframe` text so the column matches what Bybit itself uses.

- [ ] **Step 1: Write the failing tests**

```rust
use botcore::{Candle, Symbol, Timeframe};
use rust_decimal_macros::dec;

fn candle(open_time_ms: i64, close: rust_decimal::Decimal) -> Candle {
    Candle {
        open_time_ms,
        open: dec!(100),
        high: dec!(101),
        low: dec!(99),
        close,
        volume: dec!(10),
        turnover: dec!(1000),
    }
}

#[tokio::test]
async fn candles_round_trip_with_full_decimal_precision() {
    let (db, _dir) = temp_db().await;
    let sym = Symbol::new("BTCUSDT");
    // A value that binary floating point cannot represent exactly. If this
    // ever comes back changed, decimals have been routed through a REAL
    // column somewhere.
    let precise = dec!(0.1) + dec!(0.2);
    db.insert_candles(&sym, Timeframe::H1, &[candle(1000, precise)])
        .await
        .expect("insert");

    let got = db
        .candles_in_range(&sym, Timeframe::H1, 0, 2000)
        .await
        .expect("query");
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].close, dec!(0.3));
}

#[tokio::test]
async fn reinserting_the_same_candle_updates_rather_than_duplicating() {
    // A resumed or overlapping download must never create a second row for
    // one timestamp, or every later aggregate double-counts it.
    let (db, _dir) = temp_db().await;
    let sym = Symbol::new("BTCUSDT");
    db.insert_candles(&sym, Timeframe::H1, &[candle(1000, dec!(5))])
        .await
        .expect("first");
    db.insert_candles(&sym, Timeframe::H1, &[candle(1000, dec!(7))])
        .await
        .expect("second");

    let got = db
        .candles_in_range(&sym, Timeframe::H1, 0, 2000)
        .await
        .expect("query");
    assert_eq!(got.len(), 1, "one timestamp must hold exactly one row");
    assert_eq!(got[0].close, dec!(7), "the later write must win");
}

#[tokio::test]
async fn a_range_query_is_inclusive_at_both_bounds_and_ordered() {
    let (db, _dir) = temp_db().await;
    let sym = Symbol::new("BTCUSDT");
    db.insert_candles(
        &sym,
        Timeframe::H1,
        &[candle(3000, dec!(3)), candle(1000, dec!(1)), candle(2000, dec!(2))],
    )
    .await
    .expect("insert");

    let got = db
        .candles_in_range(&sym, Timeframe::H1, 1000, 3000)
        .await
        .expect("query");
    let closes: Vec<_> = got.iter().map(|c| c.close).collect();
    assert_eq!(closes, vec![dec!(1), dec!(2), dec!(3)], "ascending, inclusive");
}

#[tokio::test]
async fn timeframes_and_symbols_do_not_bleed_into_each_other() {
    let (db, _dir) = temp_db().await;
    let btc = Symbol::new("BTCUSDT");
    let eth = Symbol::new("ETHUSDT");
    db.insert_candles(&btc, Timeframe::H1, &[candle(1000, dec!(1))]).await.expect("i");
    db.insert_candles(&btc, Timeframe::H4, &[candle(1000, dec!(2))]).await.expect("i");
    db.insert_candles(&eth, Timeframe::H1, &[candle(1000, dec!(3))]).await.expect("i");

    let got = db.candles_in_range(&btc, Timeframe::H1, 0, 9999).await.expect("q");
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].close, dec!(1));
}

#[tokio::test]
async fn recorded_range_reports_the_stored_bounds() {
    let (db, _dir) = temp_db().await;
    let sym = Symbol::new("BTCUSDT");
    assert_eq!(db.recorded_range(&sym, Timeframe::H1).await.expect("q"), None);

    db.insert_candles(&sym, Timeframe::H1, &[candle(1000, dec!(1)), candle(5000, dec!(2))])
        .await
        .expect("insert");
    assert_eq!(
        db.recorded_range(&sym, Timeframe::H1).await.expect("q"),
        Some((1000, 5000))
    );
}
```

- [ ] **Step 2: Run to verify failure**

`timeout 600 cargo test -p history` — expected: methods not found.

- [ ] **Step 3: Implement `insert_candles`**

Use `INSERT OR REPLACE` on the composite primary key so a resumed download is idempotent. Write each `Decimal` with `.to_string()`. Insert inside a single transaction — a partially written page is a silent gap, and batches are large.

Also update `download_ranges` in the same transaction: `earliest_ms = MIN(existing, batch_min)`, `latest_ms = MAX(existing, batch_max)`. Doing it in the same transaction is what keeps the bookkeeping honest if the process dies mid-write.

- [ ] **Step 4: Implement `candles_in_range` and `recorded_range`**

`WHERE symbol = ?1 AND timeframe = ?2 AND open_time_ms BETWEEN ?3 AND ?4 ORDER BY open_time_ms ASC`. Parse each TEXT column back through `Decimal::from_str`, mapping failures to `HistoryError::Decode` — follow `parse_dec` in `crates/persistence/src/journal.rs`.

- [ ] **Step 5: Run tests to verify they pass**

`timeout 600 cargo test -p history`

- [ ] **Step 6: Commit**

```bash
cargo fmt -p history
git add crates/history
git -c user.email=avishkakavinda@proton.me -c user.name=acekavi commit -m "feat(history): idempotent candle storage and range queries"
```

---

### Task 3: Gap detection

**Files:**
- Create: `crates/history/src/gaps.rs`
- Modify: `crates/history/src/lib.rs`
- Test: `crates/history/tests/gaps.rs`

**Interfaces:**
- Consumes: `HistoryDb::candles_in_range` from Task 2
- Produces:
  - `pub struct Gap { pub from_ms: i64, pub to_ms: i64 }` — `from_ms` is the last present candle's open time, `to_ms` the next present one; the missing candles lie strictly between
  - `pub fn find_gaps(candles: &[Candle], tf: Timeframe) -> Vec<Gap>`
  - `Timeframe::duration_ms(self) -> i64` added to `crates/botcore/src/candle.rs` (H1 → 3_600_000, H4 → 14_400_000)

A backtest that runs across a hole prices a move that never happened continuously. The live engine already refuses to advance past an unfilled hole; this is the same rule applied to history.

- [ ] **Step 1: Confirm `Timeframe::duration_ms` already exists — do NOT re-add it**

**Plan correction (2026-08-07):** this step originally said to add
`Timeframe::duration_ms`. It has existed in `crates/botcore/src/candle.rs`
since Phase 1 (commit `bc22901`) with its own passing tests, and
`Candle::close_time_ms` already uses it. Verify it is there and move on:

```bash
grep -n "duration_ms" crates/botcore/src/candle.rs
```

Re-adding it would be a duplicate-definition compile error.

- [ ] **Step 2: Write the failing tests**

`crates/history/tests/gaps.rs`:

```rust
use botcore::Timeframe;
use history::{Gap, find_gaps};

use botcore::Candle;
use rust_decimal_macros::dec;

const H1: i64 = 3_600_000;

/// Only `open_time_ms` matters for gap detection; the price fields are
/// filler so the test reads as one line per candle.
fn c(open_time_ms: i64) -> Candle {
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

#[test]
fn consecutive_candles_have_no_gaps() {
    let candles: Vec<_> = (0..5).map(|i| c(i * H1)).collect();
    assert!(find_gaps(&candles, Timeframe::H1).is_empty());
}

#[test]
fn a_single_missing_candle_is_reported() {
    let candles = vec![c(0), c(H1), c(3 * H1)];
    assert_eq!(
        find_gaps(&candles, Timeframe::H1),
        vec![Gap { from_ms: H1, to_ms: 3 * H1 }]
    );
}

#[test]
fn several_separate_gaps_are_all_reported() {
    let candles = vec![c(0), c(2 * H1), c(5 * H1)];
    assert_eq!(
        find_gaps(&candles, Timeframe::H1),
        vec![
            Gap { from_ms: 0, to_ms: 2 * H1 },
            Gap { from_ms: 2 * H1, to_ms: 5 * H1 },
        ]
    );
}

#[test]
fn an_empty_or_single_candle_series_has_no_gaps() {
    assert!(find_gaps(&[], Timeframe::H1).is_empty());
    assert!(find_gaps(&[c(0)], Timeframe::H1).is_empty());
}

#[test]
fn the_h4_timeframe_uses_its_own_spacing() {
    // Candles one hour apart are a GAP on H4, not consecutive — using the
    // wrong duration here would hide every hole on the higher timeframe.
    let candles = vec![c(0), c(H1)];
    assert_eq!(
        find_gaps(&candles, Timeframe::H4),
        vec![Gap { from_ms: 0, to_ms: H1 }]
    );
}
```

- [ ] **Step 3: Run to verify failure**

`timeout 600 cargo test -p history --test gaps`

- [ ] **Step 4: Implement `find_gaps`**

Walk consecutive pairs; a gap exists where `next.open_time_ms - prev.open_time_ms > tf.duration_ms()`. Assumes the slice is ascending, which `candles_in_range` guarantees — state that in a doc comment.

- [ ] **Step 5: Run tests to verify they pass**

- [ ] **Step 6: Commit**

```bash
cargo fmt -p history
git add crates/history
git -c user.email=avishkakavinda@proton.me -c user.name=acekavi commit -m "feat(history): detect gaps in a stored candle series"
```

---

### Task 4: Paginated kline range fetch on `BybitRest`

**Files:**
- Modify: `crates/exchange/src/bybit/rest.rs`
- Test: `crates/exchange/tests/kline_range.rs`

**Interfaces:**
- Consumes: existing `BybitRest::get`, `RateLimiter`, `with_retry`
- Produces: `BybitRest::klines_range(&self, symbol: &Symbol, tf: Timeframe, start_ms: i64, end_ms: i64) -> Result<Vec<Candle>, ExchangeError>` — inherent method, **not** on the `ExchangeClient` trait

**Do not add this to the `ExchangeClient` trait.** The trait is what `SimulatedExchange` must implement in Plan 2b, and a simulated exchange has no business implementing a historical downloader. Read the trait's doc comment before starting — it explains why the trait stays minimal.

- [ ] **Step 1: Verify Bybit's actual response shape before writing the parser**

The `/v5/market/kline` endpoint is public — no signing needed. Confirm the parameter names, the maximum `limit`, and the ordering of the returned list:

```bash
curl -s 'https://api-testnet.bybit.com/v5/market/kline?category=linear&symbol=BTCUSDT&interval=60&limit=3' | head -c 800
```

Record what you observe in a comment on `klines_range` — specifically whether the list is newest-first or oldest-first, because the pagination loop depends on it. **Do not guess this**; the existing `klines` sorts ascending after fetching, which implies the API does not return ascending, but confirm rather than infer.

- [ ] **Step 2: Write the failing test**

`crates/exchange/tests/kline_range.rs`. This tests the *pagination logic*, not the network, so drive it through whatever seam lets you inject pages. If `BybitRest` cannot be tested without a network, extract the page-walking into a pure helper and test that:

```rust
// Pure page-walking logic, independent of HTTP.
// Given a fetcher that returns pages, it must:
//  - stop when a page comes back empty
//  - stop when the oldest candle reaches start_ms
//  - never loop forever when the API returns the same page repeatedly
```

Write at minimum:
1. A range needing three pages assembles all candles, ascending, with no duplicates.
2. An empty page terminates the walk.
3. **A stuck API returning the same page forever terminates rather than looping.** This is the one that matters: an infinite download loop against a rate-limited API is the worst failure mode here.
4. Candles outside the requested range are excluded.

- [ ] **Step 3: Run to verify failure**

- [ ] **Step 4: Implement `klines_range`**

Page using `start`/`end`/`limit` params alongside the existing `category`/`symbol`/`interval`. Walk in the direction the API's ordering makes natural (confirmed in Step 1), advancing the cursor past the oldest/newest candle received each iteration.

Three non-negotiables:
- **Terminate if a page does not advance the cursor.** Return what has been collected and log a warning; never spin.
- Deduplicate by `open_time_ms` — overlapping pages are normal.
- Sort ascending before returning, matching `klines`.

Reuse `self.get(...)`, which already applies the rate limiter and retry/backoff. Do not add a second HTTP path.

- [ ] **Step 5: Run tests to verify they pass**

`timeout 600 cargo test -p exchange`

- [ ] **Step 6: Verify against the live API**

Fetch a known 48h range for BTCUSDT H1 and assert you received 48 candles, ascending, with no duplicates. A one-off script under the scratchpad directory is fine — do not commit it.

- [ ] **Step 7: Commit**

```bash
cargo fmt -p exchange
git add crates/exchange
git -c user.email=avishkakavinda@proton.me -c user.name=acekavi commit -m "feat(exchange): paginated historical kline range fetch"
```

---

### Task 5: Funding-rate history — wire types, fetch, and storage

**Files:**
- Modify: `crates/exchange/src/bybit/wire.rs`, `crates/exchange/src/bybit/rest.rs`, `crates/history/src/db.rs`
- Test: `crates/exchange/tests/funding_wire.rs`, `crates/history/tests/db_roundtrip.rs`

**Interfaces:**
- Consumes: `BybitRest::get`, `HistoryDb` from Task 2
- Produces:
  - `pub struct FundingRate { pub symbol: Symbol, pub funding_time_ms: i64, pub rate: Decimal }` in `wire.rs`
  - `BybitRest::funding_history(&self, symbol: &Symbol, start_ms: i64, end_ms: i64) -> Result<Vec<FundingRate>, ExchangeError>`
  - `HistoryDb::insert_funding(&self, rates: &[FundingRate]) -> Result<(), HistoryError>`
  - `HistoryDb::funding_in_range(&self, symbol: &Symbol, start_ms: i64, end_ms: i64) -> Result<Vec<FundingRate>, HistoryError>`

Funding is charged per 8h period held. Omitting it flatters every long position in a bull market, which is precisely the regime a backtest is most likely to run over.

- [ ] **Step 1: Verify the endpoint shape**

```bash
curl -s 'https://api-testnet.bybit.com/v5/market/funding/history?category=linear&symbol=BTCUSDT&limit=3' | head -c 600
```

Confirm the field names (expected `fundingRate` and `fundingRateTimestamp`, both string-encoded), the max `limit`, and the ordering. Record what you observe in a comment. **Do not guess.**

- [ ] **Step 2: Write the failing wire test**

Follow the existing wire-type tests in `crates/exchange`. Assert that a captured JSON payload (paste the real one from Step 1) deserialises into `FundingRate` with the rate as an exact `Decimal` — including a negative rate, since shorts receive funding when it is negative and a sign error silently inverts every short's cost.

- [ ] **Step 3: Write the failing storage test**

```rust
#[tokio::test]
async fn funding_rates_round_trip_including_negative_rates() {
    // A negative rate means shorts are PAID. Dropping the sign would invert
    // the cost of every short position in the backtest.
    let (db, _dir) = temp_db().await;
    let sym = Symbol::new("BTCUSDT");
    db.insert_funding(&[
        FundingRate { symbol: sym.clone(), funding_time_ms: 1000, rate: dec!(0.0001) },
        FundingRate { symbol: sym.clone(), funding_time_ms: 2000, rate: dec!(-0.00025) },
    ])
    .await
    .expect("insert");

    let got = db.funding_in_range(&sym, 0, 9999).await.expect("query");
    assert_eq!(got.len(), 2);
    assert_eq!(got[0].rate, dec!(0.0001));
    assert_eq!(got[1].rate, dec!(-0.00025));
}

#[tokio::test]
async fn reinserting_a_funding_timestamp_does_not_duplicate_it() {
    let (db, _dir) = temp_db().await;
    let sym = Symbol::new("BTCUSDT");
    let r = FundingRate { symbol: sym.clone(), funding_time_ms: 1000, rate: dec!(0.0001) };
    db.insert_funding(&[r.clone()]).await.expect("first");
    db.insert_funding(&[r]).await.expect("second");
    assert_eq!(db.funding_in_range(&sym, 0, 9999).await.expect("q").len(), 1);
}
```

- [ ] **Step 4: Run to verify failure**

- [ ] **Step 5: Implement the wire type, `funding_history`, and the storage methods**

`funding_history` pages the same way `klines_range` does, with the same non-advancing-cursor guard. Storage uses `INSERT OR REPLACE` on `(symbol, funding_time_ms)`.

- [ ] **Step 6: Run tests to verify they pass**

`timeout 600 cargo test -p exchange` and `timeout 600 cargo test -p history`

- [ ] **Step 7: Commit**

```bash
cargo fmt -p exchange -p history
git add crates/exchange crates/history
git -c user.email=avishkakavinda@proton.me -c user.name=acekavi commit -m "feat(exchange,history): funding-rate history fetch and storage"
```

---

### Task 6: Historical universe reconstruction

**Files:**
- Create: `crates/history/src/universe.rs`
- Modify: `crates/history/src/lib.rs`
- Test: `crates/history/tests/universe.rs`

**Interfaces:**
- Consumes: `HistoryDb::candles_in_range` from Task 2
- Produces:
  - `pub struct UniverseSnapshot { pub at_ms: i64, pub symbols: Vec<Symbol> }`
  - `HistoryDb::rolling_turnover_24h(&self, symbol: &Symbol, at_ms: i64) -> Result<Option<Decimal>, HistoryError>`
  - `pub async fn reconstruct_universe(db: &HistoryDb, symbols: &[Symbol], at_ms: i64, filter: &HistoricalUniverseFilter, top_n: usize) -> Result<UniverseSnapshot, HistoryError>`
  - `pub struct HistoricalUniverseFilter { pub min_turnover_24h: Decimal, pub min_listing_age_days: i64 }`

**This is what removes the survivorship bias the spec commits to addressing.** The live bot re-ranks daily by 24h turnover with a floor; the backtest must select the universe the same way at each point in history, or it trades symbols that were illiquid or unranked at the time.

24h turnover is the **sum of the `turnover` column over the 24 H1 candles ending at `at_ms`** — no new timeframe needed.

Read `crates/engine/src/universe.rs` first. `select_universe` filters on **both** `min_turnover_24h` **and** `min_listing_age_days`, then sorts by turnover descending with **ties broken on symbol name** for determinism. Mirror all three exactly — a different ordering here silently tests a different bot than the one that trades.

Live listing age comes from `Instrument::age_days`. There is no historical instrument feed, so derive it instead: a symbol's listing age at `at_ms` is `at_ms - <its earliest stored candle>`. This under-estimates age when history was downloaded from a later start date, which is the safe direction — it excludes a symbol rather than trading one the live bot would have skipped.

- [ ] **Step 1: Write the failing tests**

```rust
use botcore::{Candle, Symbol, Timeframe};
use history::{HistoricalUniverseFilter, HistoryDb, reconstruct_universe};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

const H1: i64 = 3_600_000;
const DAY: i64 = 86_400_000;

/// `count` hourly candles ending at `end_ms`, each carrying `turnover`.
async fn seed(db: &HistoryDb, sym: &Symbol, end_ms: i64, count: i64, turnover: Decimal) {
    let candles: Vec<Candle> = (0..count)
        .map(|i| Candle {
            open_time_ms: end_ms - i * H1,
            open: dec!(100), high: dec!(101), low: dec!(99), close: dec!(100),
            volume: dec!(1),
            turnover,
        })
        .collect();
    db.insert_candles(sym, Timeframe::H1, &candles).await.expect("seed");
}

fn filter(min_turnover: Decimal) -> HistoricalUniverseFilter {
    HistoricalUniverseFilter { min_turnover_24h: min_turnover, min_listing_age_days: 0 }
}

#[tokio::test]
async fn rolling_turnover_sums_exactly_the_trailing_24_hours() {
    let (db, _dir) = temp_db().await;
    let sym = Symbol::new("BTCUSDT");
    // 48 hourly candles of 100 each. Only the last 24 may count.
    seed(&db, &sym, 100 * DAY, 48, dec!(100)).await;

    assert_eq!(
        db.rolling_turnover_24h(&sym, 100 * DAY).await.expect("q"),
        Some(dec!(2400)),
        "24 candles x 100, and nothing from outside the window"
    );
}

#[tokio::test]
async fn a_symbol_below_the_floor_is_excluded_entirely() {
    // The live bot would never have selected it, so the backtest must not
    // trade it — this is the thin-liquidity bias the spec removes.
    let (db, _dir) = temp_db().await;
    let thin = Symbol::new("THINUSDT");
    seed(&db, &thin, 100 * DAY, 24, dec!(1)).await; // 24 total

    let snap = reconstruct_universe(&db, &[thin], 100 * DAY, &filter(dec!(1000)), 20)
        .await
        .expect("reconstruct");
    assert!(snap.symbols.is_empty());
}

#[tokio::test]
async fn only_the_top_n_are_selected_ordered_by_turnover_descending() {
    let (db, _dir) = temp_db().await;
    let syms: Vec<Symbol> = ["A", "B", "C", "D", "E"].iter().map(|s| Symbol::new(*s)).collect();
    // A=500/h ... E=100/h, so ranking is A > B > C > D > E.
    for (i, s) in syms.iter().enumerate() {
        seed(&db, s, 100 * DAY, 24, Decimal::from(500 - (i as i64) * 100)).await;
    }

    let snap = reconstruct_universe(&db, &syms, 100 * DAY, &filter(dec!(0)), 3)
        .await
        .expect("reconstruct");
    let got: Vec<&str> = snap.symbols.iter().map(|s| s.as_str()).collect();
    assert_eq!(got, vec!["A", "B", "C"]);
}

#[tokio::test]
async fn equal_turnover_breaks_the_tie_on_symbol_name() {
    // Mirrors crates/engine/src/universe.rs. Without this the ordering
    // depends on storage order and two identical runs can disagree.
    let (db, _dir) = temp_db().await;
    let syms: Vec<Symbol> = ["ZZZUSDT", "AAAUSDT"].iter().map(|s| Symbol::new(*s)).collect();
    for s in &syms {
        seed(&db, s, 100 * DAY, 24, dec!(100)).await;
    }

    let snap = reconstruct_universe(&db, &syms, 100 * DAY, &filter(dec!(0)), 2)
        .await
        .expect("reconstruct");
    assert_eq!(snap.symbols[0].as_str(), "AAAUSDT");
}

#[tokio::test]
async fn a_symbol_with_no_candles_in_the_window_is_excluded_not_ranked_as_zero() {
    // Absent data is not evidence of zero turnover. It must drop out of the
    // ranking entirely rather than appear at the bottom of it.
    let (db, _dir) = temp_db().await;
    let missing = Symbol::new("GONEUSDT");
    let snap = reconstruct_universe(&db, &[missing], 100 * DAY, &filter(dec!(0)), 20)
        .await
        .expect("reconstruct");
    assert!(snap.symbols.is_empty());
}

#[tokio::test]
async fn a_symbol_younger_than_the_listing_age_floor_is_excluded() {
    let (db, _dir) = temp_db().await;
    let fresh = Symbol::new("NEWUSDT");
    // Only 24h of history exists, so at this timestamp it is 1 day old.
    seed(&db, &fresh, 100 * DAY, 24, dec!(10000)).await;

    let f = HistoricalUniverseFilter { min_turnover_24h: dec!(0), min_listing_age_days: 30 };
    let snap = reconstruct_universe(&db, &[fresh], 100 * DAY, &f, 20).await.expect("r");
    assert!(snap.symbols.is_empty(), "too young to have been traded live");
}

#[tokio::test]
async fn the_universe_changes_as_history_moves() {
    // THE POINT OF THIS TASK. A symbol thin early and liquid later must be
    // excluded at the early timestamp and included at the later one. A fixed
    // top-N snapshot gets this wrong in exactly the way the spec calls out.
    let (db, _dir) = temp_db().await;
    let sym = Symbol::new("GROWUSDT");
    seed(&db, &sym, 100 * DAY, 24, dec!(1)).await;      // thin, early
    seed(&db, &sym, 200 * DAY, 24, dec!(10000)).await;  // liquid, later

    let f = filter(dec!(1000));
    let early = reconstruct_universe(&db, &[sym.clone()], 100 * DAY, &f, 20).await.expect("r");
    let late = reconstruct_universe(&db, &[sym], 200 * DAY, &f, 20).await.expect("r");

    assert!(early.symbols.is_empty(), "was below the floor at this point in history");
    assert_eq!(late.symbols.len(), 1, "cleared the floor by this point");
}
```

- [ ] **Step 2: Run to verify failure**

`timeout 600 cargo test -p history --test universe`

- [ ] **Step 3: Implement `rolling_turnover_24h` and `reconstruct_universe`**

Sum `turnover` over `candles_in_range(symbol, H1, at_ms - 24h + H1, at_ms)` — note the window start, so exactly 24 candles are included rather than 25. Return `None` when the window holds no candles, distinct from `Some(0)`.

`reconstruct_universe` filters on turnover floor and derived listing age, sorts by turnover descending with symbol-name tie-break, and truncates to `top_n`.

- [ ] **Step 4: Run tests to verify they pass**

- [ ] **Step 5: Commit**

```bash
cargo fmt -p history
git add crates/history
git -c user.email=avishkakavinda@proton.me -c user.name=acekavi commit -m "feat(history): reconstruct the historical universe from recorded turnover"
```

---

### Task 7: Resumable download orchestration and the `download-history` binary

**Files:**
- Create: `crates/history/src/download.rs`, `bot/src/bin/download_history.rs`
- Modify: `crates/history/src/lib.rs`
- Test: `crates/history/tests/download.rs`

**Interfaces:**
- Consumes: everything above
- Produces:
  - `pub struct DownloadReport { pub symbol: Symbol, pub timeframe: Timeframe, pub candles_written: usize, pub gaps: Vec<Gap> }`
  - `pub async fn download_symbol(rest: &BybitRest, db: &HistoryDb, symbol: &Symbol, tf: Timeframe, start_ms: i64, end_ms: i64) -> Result<DownloadReport, HistoryError>`
  - `HistoryError` gains an `Exchange(String)` variant so a fetch failure is distinguishable from a storage failure. Do not introduce a separate `DownloadError` — one error type per crate keeps the call sites readable.

- [ ] **Step 1: Write the failing tests**

Drive through a fake fetcher, not the network:

1. **A resumed download skips what is already stored.** Seed the db with a range, run the download, assert the fetcher was asked only for the missing portion. This is the whole point of `download_ranges`.
2. **Gaps are reported, never filled by interpolation.** Assert `report.gaps` is populated and that no candle exists at the missing timestamps.
3. A download that fails partway leaves the candles it did write, and `recorded_range` reflects only those — a crash must not corrupt bookkeeping into claiming a range it does not hold.

- [ ] **Step 2: Run to verify failure**

- [ ] **Step 3: Implement `download_symbol`**

Consult `recorded_range` first and fetch only what is missing. Write in batches via `insert_candles` (which updates `download_ranges` in the same transaction). After writing, run `find_gaps` over the stored range and return them in the report.

**Never interpolate or synthesise a missing candle.** A reported gap is recoverable; an invented one silently corrupts every backtest run over it.

- [ ] **Step 4: Write the binary**

`bot/src/bin/download_history.rs`: read the profile config the way `bot/src/main.rs` does, resolve the symbol list, and download H1 and H4 for each over the requested range. Log per-symbol progress and a final summary of candles written and gaps found. Public endpoints need no credentials — do not require them.

- [ ] **Step 5: Run tests to verify they pass**

`timeout 600 cargo test -p history`

- [ ] **Step 6: Run it for real against one symbol**

```bash
cargo run --bin download-history -- --symbol BTCUSDT --days 30
```

Then confirm the database actually holds what was claimed:

```bash
sqlite3 data/history.db "SELECT COUNT(*), MIN(open_time_ms), MAX(open_time_ms) FROM candles WHERE symbol='BTCUSDT' AND timeframe='60';"
```

30 days of H1 is 720 candles. A materially lower count means gaps — check the reported gap list rather than assuming success.

- [ ] **Step 7: Commit**

```bash
cargo fmt -p history -p bot
git add crates/history bot
git -c user.email=avishkakavinda@proton.me -c user.name=acekavi commit -m "feat(history): resumable download orchestration and download-history binary"
```

---

## Final verification

- [ ] `timeout 600 cargo build --workspace`
- [ ] `timeout 900 cargo test --workspace` — all previously passing tests still pass (257 before this plan)
- [ ] `timeout 900 cargo clippy --workspace --all-targets -- -D warnings`
- [ ] `timeout 600 cargo test -p exchange --test no_market_orders`
- [ ] `grep -rn "f64" crates/history/src/` returns nothing
- [ ] The `data/history.db` produced by a real run holds the candle count its report claimed
