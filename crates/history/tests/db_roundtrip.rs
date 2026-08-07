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
