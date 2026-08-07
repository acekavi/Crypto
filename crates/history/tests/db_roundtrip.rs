use botcore::{Candle, Symbol, Timeframe};
use exchange::bybit::wire::FundingRate;
use history::HistoryDb;
use rust_decimal_macros::dec;

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
        &[
            candle(3000, dec!(3)),
            candle(1000, dec!(1)),
            candle(2000, dec!(2)),
        ],
    )
    .await
    .expect("insert");

    let got = db
        .candles_in_range(&sym, Timeframe::H1, 1000, 3000)
        .await
        .expect("query");
    let closes: Vec<_> = got.iter().map(|c| c.close).collect();
    assert_eq!(
        closes,
        vec![dec!(1), dec!(2), dec!(3)],
        "ascending, inclusive"
    );
}

#[tokio::test]
async fn timeframes_and_symbols_do_not_bleed_into_each_other() {
    let (db, _dir) = temp_db().await;
    let btc = Symbol::new("BTCUSDT");
    let eth = Symbol::new("ETHUSDT");
    db.insert_candles(&btc, Timeframe::H1, &[candle(1000, dec!(1))])
        .await
        .expect("i");
    db.insert_candles(&btc, Timeframe::H4, &[candle(1000, dec!(2))])
        .await
        .expect("i");
    db.insert_candles(&eth, Timeframe::H1, &[candle(1000, dec!(3))])
        .await
        .expect("i");

    let got = db
        .candles_in_range(&btc, Timeframe::H1, 0, 9999)
        .await
        .expect("q");
    assert_eq!(got.len(), 1);
    assert_eq!(got[0].close, dec!(1));
}

#[tokio::test]
async fn recorded_range_reports_the_stored_bounds() {
    let (db, _dir) = temp_db().await;
    let sym = Symbol::new("BTCUSDT");
    assert_eq!(
        db.recorded_range(&sym, Timeframe::H1).await.expect("q"),
        None
    );

    db.insert_candles(
        &sym,
        Timeframe::H1,
        &[candle(1000, dec!(1)), candle(5000, dec!(2))],
    )
    .await
    .expect("insert");
    assert_eq!(
        db.recorded_range(&sym, Timeframe::H1).await.expect("q"),
        Some((1000, 5000))
    );
}

#[tokio::test]
async fn funding_rates_round_trip_including_negative_rates() {
    // A negative rate means shorts are PAID. Dropping the sign would invert
    // the cost of every short position in the backtest.
    let (db, _dir) = temp_db().await;
    let sym = Symbol::new("BTCUSDT");
    db.insert_funding(&[
        FundingRate {
            symbol: sym.clone(),
            funding_time_ms: 1000,
            rate: dec!(0.0001),
        },
        FundingRate {
            symbol: sym.clone(),
            funding_time_ms: 2000,
            rate: dec!(-0.00025),
        },
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
    let r = FundingRate {
        symbol: sym.clone(),
        funding_time_ms: 1000,
        rate: dec!(0.0001),
    };
    db.insert_funding(std::slice::from_ref(&r))
        .await
        .expect("first");
    db.insert_funding(&[r]).await.expect("second");
    assert_eq!(
        db.funding_in_range(&sym, 0, 9999).await.expect("q").len(),
        1
    );
}
