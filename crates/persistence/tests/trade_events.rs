use std::str::FromStr;

use botcore::Symbol;
use persistence::{Journal, TradeEvent, TradeEventKind};

async fn temp_journal() -> (Journal, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = dir.path().join("test.db");
    let j = Journal::open_local(path.to_str().unwrap())
        .await
        .expect("journal opens");
    (j, dir)
}

fn event(symbol: &str, at_ms: i64, kind: TradeEventKind) -> TradeEvent {
    TradeEvent {
        at_ms,
        symbol: Symbol::new(symbol),
        order_link_id: Some(format!("link-{symbol}")),
        kind,
        detail: "stop 61234.5 -> 62000.0 (entry)".into(),
        config_hash: "deadbeef".into(),
    }
}

#[tokio::test]
async fn events_come_back_in_chronological_order() {
    let (j, _dir) = temp_journal().await;
    for (at, kind) in [
        (300, TradeEventKind::StopMovedToBreakeven),
        (100, TradeEventKind::EntryPlaced),
        (200, TradeEventKind::EntryFilled),
    ] {
        j.record_event(&event("BTCUSDT", at, kind))
            .await
            .expect("record");
    }

    let got = j.events_for(&Symbol::new("BTCUSDT")).await.expect("load");
    let kinds: Vec<_> = got.iter().map(|e| e.kind).collect();
    assert_eq!(
        kinds,
        vec![
            TradeEventKind::EntryPlaced,
            TradeEventKind::EntryFilled,
            TradeEventKind::StopMovedToBreakeven,
        ]
    );
}

#[tokio::test]
async fn two_events_in_the_same_millisecond_keep_insertion_order() {
    let (j, _dir) = temp_journal().await;
    j.record_event(&event("BTCUSDT", 100, TradeEventKind::EntryFilled))
        .await
        .expect("first");
    j.record_event(&event("BTCUSDT", 100, TradeEventKind::StopPlaced))
        .await
        .expect("second");

    let got = j.events_for(&Symbol::new("BTCUSDT")).await.expect("load");
    assert_eq!(got[0].kind, TradeEventKind::EntryFilled);
    assert_eq!(got[1].kind, TradeEventKind::StopPlaced);
}

#[tokio::test]
async fn events_are_scoped_to_their_symbol() {
    let (j, _dir) = temp_journal().await;
    j.record_event(&event("BTCUSDT", 100, TradeEventKind::EntryPlaced))
        .await
        .expect("btc");
    j.record_event(&event("ETHUSDT", 100, TradeEventKind::EntryPlaced))
        .await
        .expect("eth");

    assert_eq!(
        j.events_for(&Symbol::new("BTCUSDT"))
            .await
            .expect("load")
            .len(),
        1
    );
    assert_eq!(j.event_count().await.expect("count"), 2);
}

#[tokio::test]
async fn an_event_round_trips_every_field() {
    let (j, _dir) = temp_journal().await;
    let e = event("BTCUSDT", 1_700_000_000_000, TradeEventKind::StopEscalated);
    j.record_event(&e).await.expect("record");

    let got = j.events_for(&Symbol::new("BTCUSDT")).await.expect("load");
    assert_eq!(got, vec![e]);
}

#[tokio::test]
async fn an_event_without_an_order_link_id_round_trips_as_none() {
    let (j, _dir) = temp_journal().await;
    let e = TradeEvent {
        order_link_id: None,
        ..event("BTCUSDT", 100, TradeEventKind::HaltSet)
    };
    j.record_event(&e).await.expect("record");

    assert_eq!(
        j.events_for(&Symbol::new("BTCUSDT")).await.expect("load")[0].order_link_id,
        None
    );
}

#[tokio::test]
async fn the_log_is_append_only_so_recording_twice_keeps_both_rows() {
    // Unlike orders, an event has no idempotency key: two identical stop
    // escalations really did happen twice and both belong in the audit trail.
    let (j, _dir) = temp_journal().await;
    let e = event("BTCUSDT", 100, TradeEventKind::StopEscalated);
    j.record_event(&e).await.expect("first");
    j.record_event(&e).await.expect("second");

    assert_eq!(j.event_count().await.expect("count"), 2);
}

#[test]
fn every_event_kind_round_trips_through_its_string_form() {
    // A kind that fails to parse back would silently vanish from the audit
    // trail, which is the one thing this table exists to prevent.
    for kind in TradeEventKind::ALL {
        assert_eq!(
            TradeEventKind::from_str(kind.as_str()).expect("parses back"),
            kind
        );
    }
}

#[test]
fn no_two_event_kinds_share_a_string_form() {
    // Two kinds writing the same text would make the log ambiguous and would
    // silently rewrite one kind into the other on the way back out.
    let mut seen: Vec<&str> = TradeEventKind::ALL.iter().map(|k| k.as_str()).collect();
    let total = seen.len();
    seen.sort_unstable();
    seen.dedup();
    assert_eq!(seen.len(), total);
}

#[test]
fn an_unknown_kind_is_a_decode_error_not_a_silent_default() {
    assert!(TradeEventKind::from_str("NotAKind").is_err());
}
