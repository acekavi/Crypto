use botcore::{OrderState, Side};
use exchange::bybit::sign::Credentials;
use exchange::bybit::ws_private::{build_auth_frame, parse_private_message, AccountEvent};
use rust_decimal_macros::dec;

#[test]
fn auth_frame_has_the_documented_shape() {
    let creds = Credentials { api_key: "mykey".into(), api_secret: "mysecret".into() };
    let frame = build_auth_frame(&creds, 1_700_000_005_000);
    let v: serde_json::Value = serde_json::from_str(&frame).expect("valid json");

    assert_eq!(v["op"], "auth");
    let args = v["args"].as_array().expect("args is an array");
    assert_eq!(args.len(), 3);
    assert_eq!(args[0], "mykey");
    assert_eq!(args[1], 1_700_000_005_000i64);
    assert_eq!(args[2].as_str().expect("signature is a string").len(), 64);
}

#[test]
fn order_updates_parse_with_state_and_fill_quantity() {
    let raw = r#"{"topic":"order","data":[{
        "symbol":"BTCUSDT","orderId":"oid-1","orderLinkId":"link-1","side":"Buy",
        "price":"42000","qty":"0.01","cumExecQty":"0.004",
        "orderStatus":"PartiallyFilled","createdTime":"1700000000000"}]}"#;

    let events = parse_private_message(raw).expect("parses");
    assert_eq!(events.len(), 1);
    match &events[0] {
        AccountEvent::OrderUpdate(o) => {
            assert_eq!(o.order_link_id, "link-1");
            assert_eq!(o.state, OrderState::PartiallyFilled);
            assert_eq!(o.cum_exec_qty, dec!(0.004));
            assert_eq!(o.side, Side::Buy);
        }
        other => panic!("expected OrderUpdate, got {other:?}"),
    }
}

#[test]
fn flat_position_updates_are_reported_as_closed() {
    // A stop firing produces a position update with size 0. The engine must
    // see this as "closed", not as an open position of zero size.
    let raw = r#"{"topic":"position","data":[{
        "symbol":"BTCUSDT","side":"","size":"0","avgPrice":"0",
        "liqPrice":"","unrealisedPnl":"0"}]}"#;

    let events = parse_private_message(raw).expect("parses");
    assert_eq!(events.len(), 1);
    match &events[0] {
        AccountEvent::PositionClosed { symbol } => assert_eq!(symbol.as_str(), "BTCUSDT"),
        other => panic!("expected PositionClosed, got {other:?}"),
    }
}

#[test]
fn wallet_updates_carry_equity() {
    let raw = r#"{"topic":"wallet","data":[{
        "totalEquity":"10500.5","totalAvailableBalance":"9000.25"}]}"#;
    let events = parse_private_message(raw).expect("parses");
    match &events[0] {
        AccountEvent::WalletUpdate(b) => assert_eq!(b.equity, dec!(10500.5)),
        other => panic!("expected WalletUpdate, got {other:?}"),
    }
}

#[test]
fn auth_and_subscribe_acks_produce_no_events() {
    assert!(parse_private_message(r#"{"op":"auth","success":true}"#).unwrap().is_empty());
    assert!(parse_private_message(r#"{"op":"subscribe","success":true}"#).unwrap().is_empty());
}
