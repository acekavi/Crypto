use botcore::Symbol;
use persistence::{Journal, PairHeartbeat, PairPositionRecord};
use rust_decimal_macros::dec;

async fn temp_journal() -> (Journal, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("test.db");
    let j = Journal::open_local(path.to_str().unwrap())
        .await
        .expect("journal opens");
    (j, dir)
}

fn sample_pair_position() -> PairPositionRecord {
    PairPositionRecord {
        bot_id: "aave_eth".into(),
        side: "long_spread".into(),
        opened_at_ms: 1_700_000_000_000,
        entry_z: dec!(-3.21),
        a_symbol: Symbol::new("AAVEUSDT"),
        a_qty: dec!(1.5),
        a_entry: dec!(300.25),
        a_order_id: "oid-a".into(),
        b_symbol: Symbol::new("ETHUSDT"),
        b_qty: dec!(0.4),
        b_entry: dec!(3000.5),
        b_order_id: "oid-b".into(),
        breakeven_armed: false,
        per_leg_notional: dec!(450),
        capped_by: Some("available_equity".into()),
    }
}

#[tokio::test]
async fn a_pair_position_round_trips_through_a_reopened_journal() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("j.db");
    let path = path.to_str().unwrap();
    let record = sample_pair_position();

    {
        let j = Journal::open_local(path).await.unwrap();
        j.upsert_pair_position(&record).await.unwrap();
    }

    let j = Journal::open_local(path).await.unwrap();
    assert_eq!(j.pair_position("aave_eth").await.unwrap(), Some(record));
}

#[tokio::test]
async fn upserting_a_pair_position_replaces_rather_than_duplicating() {
    let (j, _dir) = temp_journal().await;
    let mut r = sample_pair_position();
    j.upsert_pair_position(&r).await.unwrap();
    r.breakeven_armed = true;
    j.upsert_pair_position(&r).await.unwrap();
    assert!(j.pair_position("aave_eth").await.unwrap().unwrap().breakeven_armed);
}

#[tokio::test]
async fn clearing_a_pair_position_leaves_the_bot_flat() {
    let (j, _dir) = temp_journal().await;
    j.upsert_pair_position(&sample_pair_position()).await.unwrap();
    j.clear_pair_position("aave_eth").await.unwrap();
    assert_eq!(j.pair_position("aave_eth").await.unwrap(), None);
}

#[tokio::test]
async fn two_bots_keep_separate_positions() {
    let (j, _dir) = temp_journal().await;
    let a = sample_pair_position();
    let mut b = sample_pair_position();
    b.bot_id = "ena_xrp".into();
    j.upsert_pair_position(&a).await.unwrap();
    j.upsert_pair_position(&b).await.unwrap();
    j.clear_pair_position("aave_eth").await.unwrap();
    assert_eq!(j.pair_position("aave_eth").await.unwrap(), None);
    assert!(j.pair_position("ena_xrp").await.unwrap().is_some());
}

#[tokio::test]
async fn a_heartbeat_survives_a_restart_so_a_stalled_bot_is_visible() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("j.db");
    let path = path.to_str().unwrap();
    let hb = PairHeartbeat {
        bot_id: "aave_eth".into(),
        last_bar_ms: Some(1_700_000_000_000),
        last_loop_ms: Some(1_700_000_060_000),
        last_z: Some(dec!(-1.25)),
        last_signal: None,
        last_guard_reason: None,
    };
    {
        let j = Journal::open_local(path).await.unwrap();
        j.upsert_pair_heartbeat(&hb).await.unwrap();
    }
    let j = Journal::open_local(path).await.unwrap();
    assert_eq!(j.pair_heartbeat("aave_eth").await.unwrap(), Some(hb));
}
