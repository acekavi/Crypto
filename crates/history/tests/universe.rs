use botcore::{Candle, Symbol, Timeframe};
use history::{HistoricalUniverseFilter, HistoryDb, reconstruct_universe};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

const H1: i64 = 3_600_000;
const DAY: i64 = 86_400_000;

async fn temp_db() -> (HistoryDb, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("h.db");
    let db = HistoryDb::open_local(path.to_str().unwrap())
        .await
        .expect("opens");
    (db, dir)
}

/// `count` hourly candles ending at `end_ms`, each carrying `turnover`.
async fn seed(db: &HistoryDb, sym: &Symbol, end_ms: i64, count: i64, turnover: Decimal) {
    let candles: Vec<Candle> = (0..count)
        .map(|i| Candle {
            open_time_ms: end_ms - i * H1,
            open: dec!(100),
            high: dec!(101),
            low: dec!(99),
            close: dec!(100),
            volume: dec!(1),
            turnover,
        })
        .collect();
    db.insert_candles(sym, Timeframe::H1, &candles)
        .await
        .expect("seed");
}

fn filter(min_turnover: Decimal) -> HistoricalUniverseFilter {
    HistoricalUniverseFilter {
        min_turnover_24h: min_turnover,
        min_listing_age_days: 0,
    }
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
    let syms: Vec<Symbol> = ["A", "B", "C", "D", "E"]
        .iter()
        .map(|s| Symbol::new(*s))
        .collect();
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
    let syms: Vec<Symbol> = ["ZZZUSDT", "AAAUSDT"]
        .iter()
        .map(|s| Symbol::new(*s))
        .collect();
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

    let f = HistoricalUniverseFilter {
        min_turnover_24h: dec!(0),
        min_listing_age_days: 30,
    };
    let snap = reconstruct_universe(&db, &[fresh], 100 * DAY, &f, 20)
        .await
        .expect("r");
    assert!(
        snap.symbols.is_empty(),
        "too young to have been traded live"
    );
}

#[tokio::test]
async fn the_universe_changes_as_history_moves() {
    // THE POINT OF THIS TASK. A symbol thin early and liquid later must be
    // excluded at the early timestamp and included at the later one. A fixed
    // top-N snapshot gets this wrong in exactly the way the spec calls out.
    let (db, _dir) = temp_db().await;
    let sym = Symbol::new("GROWUSDT");
    seed(&db, &sym, 100 * DAY, 24, dec!(1)).await; // thin, early
    seed(&db, &sym, 200 * DAY, 24, dec!(10000)).await; // liquid, later

    let f = filter(dec!(1000));
    let early = reconstruct_universe(&db, std::slice::from_ref(&sym), 100 * DAY, &f, 20)
        .await
        .expect("r");
    let late = reconstruct_universe(&db, &[sym], 200 * DAY, &f, 20)
        .await
        .expect("r");

    assert!(
        early.symbols.is_empty(),
        "was below the floor at this point in history"
    );
    assert_eq!(late.symbols.len(), 1, "cleared the floor by this point");
}
