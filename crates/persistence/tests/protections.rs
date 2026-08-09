use botcore::{Side, Symbol};
use persistence::{Journal, ProtectionRecord};
use rust_decimal_macros::dec;

async fn temp_journal() -> (Journal, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("test.db");
    let j = Journal::open_local(path.to_str().unwrap())
        .await
        .expect("journal opens");
    (j, dir)
}

fn sample_protection(symbol: &str) -> ProtectionRecord {
    ProtectionRecord {
        symbol: Symbol::new(symbol),
        order_link_id: format!("link-{symbol}"),
        side: Side::Buy,
        trigger: dec!(41000.5),
        atr: dec!(250.25),
        entry_price: dec!(42000.5),
        initial_risk: dec!(1000.5),
        stop_limit_offset: dec!(5.5),
        breakeven_at_r: Some(dec!(2)),
        moved_to_breakeven: false,
        updated_at_ms: 1_700_000_000_000,
    }
}

#[tokio::test]
async fn a_protection_round_trips_through_the_journal() {
    let (j, _dir) = temp_journal().await;
    let p = sample_protection("BTCUSDT");
    j.upsert_protection(&p).await.expect("upsert");

    let loaded = j.load_protections().await.expect("load");
    assert_eq!(loaded, vec![p]);
}

#[tokio::test]
async fn upserting_the_same_symbol_replaces_rather_than_duplicates() {
    let (j, _dir) = temp_journal().await;
    j.upsert_protection(&sample_protection("BTCUSDT"))
        .await
        .expect("first");
    let moved = ProtectionRecord {
        moved_to_breakeven: true,
        trigger: dec!(100),
        ..sample_protection("BTCUSDT")
    };
    j.upsert_protection(&moved).await.expect("second");

    let loaded = j.load_protections().await.expect("load");
    assert_eq!(loaded.len(), 1, "one position per symbol means one row");
    assert!(loaded[0].moved_to_breakeven);
    assert_eq!(loaded[0].trigger, dec!(100));
}

#[tokio::test]
async fn deleting_a_protection_removes_only_that_symbol() {
    let (j, _dir) = temp_journal().await;
    j.upsert_protection(&sample_protection("BTCUSDT"))
        .await
        .expect("btc");
    j.upsert_protection(&sample_protection("ETHUSDT"))
        .await
        .expect("eth");
    j.delete_protection(&Symbol::new("BTCUSDT"))
        .await
        .expect("delete");

    let loaded = j.load_protections().await.expect("load");
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].symbol, Symbol::new("ETHUSDT"));
}

#[tokio::test]
async fn a_protection_with_no_breakeven_round_trips_as_none_not_zero() {
    // Zero would mean "move the stop to entry immediately"; None means never.
    let (j, _dir) = temp_journal().await;
    let p = ProtectionRecord {
        breakeven_at_r: None,
        ..sample_protection("BTCUSDT")
    };
    j.upsert_protection(&p).await.expect("upsert");

    let loaded = j.load_protections().await.expect("load");
    assert_eq!(loaded[0].breakeven_at_r, None);
}

#[tokio::test]
async fn a_short_protection_round_trips_with_its_side() {
    let (j, _dir) = temp_journal().await;
    let p = ProtectionRecord {
        side: Side::Sell,
        ..sample_protection("BTCUSDT")
    };
    j.upsert_protection(&p).await.expect("upsert");

    let loaded = j.load_protections().await.expect("load");
    assert_eq!(loaded[0].side, Side::Sell);
}

#[tokio::test]
async fn decimal_values_survive_magnitudes_that_break_text_ordering() {
    // Decimals persist as TEXT. "9" sorts above "10000" lexicographically, so
    // any SQL comparison on these columns is wrong; this pins the round-trip
    // values rather than any ordering.
    let (j, _dir) = temp_journal().await;
    let small = ProtectionRecord {
        trigger: dec!(9),
        ..sample_protection("AAA")
    };
    let large = ProtectionRecord {
        trigger: dec!(10000.00000001),
        ..sample_protection("BBB")
    };
    j.upsert_protection(&small).await.expect("small");
    j.upsert_protection(&large).await.expect("large");

    let loaded = j.load_protections().await.expect("load");
    let by = |s: &str| {
        loaded
            .iter()
            .find(|p| p.symbol.as_str() == s)
            .expect("present")
            .trigger
    };
    assert_eq!(by("AAA"), dec!(9));
    assert_eq!(by("BBB"), dec!(10000.00000001));
}

#[tokio::test]
async fn protections_survive_reopening_the_database() {
    // The whole point: a restart must be able to rebuild the in-memory map.
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("protections.db");
    let p = path.to_str().unwrap();

    {
        let j = Journal::open_local(p).await.expect("opens");
        j.upsert_protection(&sample_protection("BTCUSDT"))
            .await
            .expect("upsert");
    }

    let j = Journal::open_local(p).await.expect("reopens");
    let loaded = j.load_protections().await.expect("load");
    assert_eq!(loaded, vec![sample_protection("BTCUSDT")]);
}
