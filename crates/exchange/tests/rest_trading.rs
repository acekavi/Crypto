use botcore::{LimitEntry, Side, Symbol};
use exchange::ExchangeClient;
use exchange::bybit::rest::BybitRest;
use exchange::bybit::sign::Credentials;
use rust_decimal_macros::dec;
use wiremock::matchers::{body_partial_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn creds() -> Credentials {
    Credentials {
        api_key: "k".into(),
        api_secret: "s".into(),
    }
}

fn entry() -> LimitEntry {
    LimitEntry {
        symbol: Symbol::new("BTCUSDT"),
        side: Side::Buy,
        qty: dec!(0.01),
        price: dec!(42000.5),
        order_link_id: "abc123".into(),
        stop_loss: dec!(41000),
        stop_limit_price: dec!(40900),
        take_profit: dec!(44001),
    }
}

#[tokio::test]
async fn entry_is_sent_as_a_postonly_limit_with_protection_attached() {
    let server = MockServer::start().await;

    // Assert on the exact wire body: orderType Limit, PostOnly, and both
    // stop and target present in the same request so no unprotected window
    // can exist between entry and protection.
    Mock::given(method("POST"))
        .and(path("/v5/order/create"))
        .and(body_partial_json(serde_json::json!({
            "category": "linear",
            "symbol": "BTCUSDT",
            "side": "Buy",
            "orderType": "Limit",
            "timeInForce": "PostOnly",
            "positionIdx": 0,
            "qty": "0.01",
            "price": "42000.5",
            "orderLinkId": "abc123",
            "stopLoss": "41000",
            "slLimitPrice": "40900",
            "slOrderType": "Limit",
            "slTriggerBy": "MarkPrice",
            "takeProfit": "44001",
            "tpOrderType": "Limit"
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "retCode": 0, "retMsg": "OK",
            "result": {"orderId": "oid-1", "orderLinkId": "abc123"},
            "time": 1700007200000i64
        })))
        .mount(&server)
        .await;

    let client = BybitRest::new(server.uri(), creds());
    let ack = client
        .place_limit_entry(entry())
        .await
        .expect("order placed");
    assert_eq!(ack.order_id, "oid-1");
    assert_eq!(ack.order_link_id, "abc123");
}

#[tokio::test]
async fn positions_parse_liquidation_price_as_optional() {
    let server = MockServer::start().await;

    // Bybit sends an empty string when there is no liquidation price.
    Mock::given(method("GET"))
        .and(path("/v5/position/list"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "retCode": 0, "retMsg": "OK",
            "result": {"list": [
                {"symbol":"BTCUSDT","side":"Buy","size":"0.01","avgPrice":"42000",
                 "liqPrice":"38000","unrealisedPnl":"12.5"},
                {"symbol":"ETHUSDT","side":"Sell","size":"0.5","avgPrice":"2500",
                 "liqPrice":"","unrealisedPnl":"-3"}
            ]},
            "time": 1700007200000i64
        })))
        .mount(&server)
        .await;

    let client = BybitRest::new(server.uri(), creds());
    let positions = client.positions().await.expect("positions fetched");

    assert_eq!(positions.len(), 2);
    assert_eq!(positions[0].liq_price, Some(dec!(38000)));
    assert_eq!(
        positions[1].liq_price, None,
        "empty liqPrice must become None"
    );
    assert_eq!(positions[1].side, Side::Sell);
}

#[tokio::test]
async fn zero_size_positions_are_excluded() {
    let server = MockServer::start().await;

    // Bybit reports closed positions with size "0"; treating those as open
    // would make the reconciler adopt phantom positions.
    Mock::given(method("GET"))
        .and(path("/v5/position/list"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "retCode": 0, "retMsg": "OK",
            "result": {"list": [
                {"symbol":"BTCUSDT","side":"Buy","size":"0","avgPrice":"0",
                 "liqPrice":"","unrealisedPnl":"0"}
            ]},
            "time": 1700007200000i64
        })))
        .mount(&server)
        .await;

    let client = BybitRest::new(server.uri(), creds());
    assert!(client.positions().await.expect("fetched").is_empty());
}

#[tokio::test]
async fn balance_reads_equity_and_available_margin() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v5/account/wallet-balance"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "retCode": 0, "retMsg": "OK",
            "result": {"list": [{
                "totalEquity": "10250.75",
                "totalAvailableBalance": "9800.25"
            }]},
            "time": 1700007200000i64
        })))
        .mount(&server)
        .await;

    let client = BybitRest::new(server.uri(), creds());
    let bal = client.balance().await.expect("balance fetched");
    assert_eq!(bal.equity, dec!(10250.75));
    assert_eq!(bal.available, dec!(9800.25));
}
