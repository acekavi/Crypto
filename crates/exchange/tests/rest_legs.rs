use botcore::{LimitLeg, OrderState, Side, Symbol};
use exchange::ExchangeClient;
use exchange::bybit::rest::BybitRest;
use exchange::bybit::sign::Credentials;
use rust_decimal_macros::dec;
use wiremock::matchers::{body_partial_json, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn creds() -> Credentials {
    Credentials {
        api_key: "k".into(),
        api_secret: "s".into(),
    }
}

fn leg() -> LimitLeg {
    LimitLeg {
        symbol: Symbol::new("AAVEUSDT"),
        side: Side::Buy,
        qty: dec!(0.5),
        price: dec!(300.25),
        order_link_id: "aave_eth-a-1700000000".into(),
        reduce_only: false,
    }
}

#[tokio::test]
async fn a_leg_is_sent_as_a_gtc_limit_with_no_protection_attached() {
    let server = MockServer::start().await;

    // GTC, not PostOnly: the price crosses the book on purpose, and PostOnly
    // would be rejected. Not IOC: a partial fill that cancels the remainder
    // leaves the pair mismatched. And no stopLoss/takeProfit, because a pair's
    // stop is a spread z-score, not a price on either leg.
    Mock::given(method("POST"))
        .and(path("/v5/order/create"))
        .and(body_partial_json(serde_json::json!({
            "category": "linear",
            "symbol": "AAVEUSDT",
            "side": "Buy",
            "orderType": "Limit",
            "timeInForce": "GTC",
            "positionIdx": 0,
            "qty": "0.5",
            "price": "300.25",
            "orderLinkId": "aave_eth-a-1700000000",
            "reduceOnly": false
        })))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "retCode": 0, "retMsg": "OK",
            "result": {"orderId": "oid-9", "orderLinkId": "aave_eth-a-1700000000"},
            "time": 1700007200000i64
        })))
        .mount(&server)
        .await;

    let client = BybitRest::new(server.uri(), creds());
    let ack = client.place_limit_leg(leg()).await.expect("leg placed");
    assert_eq!(ack.order_id, "oid-9");
}

#[tokio::test]
async fn a_closing_leg_is_marked_reduce_only() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v5/order/create"))
        .and(body_partial_json(serde_json::json!({"reduceOnly": true})))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "retCode": 0, "retMsg": "OK",
            "result": {"orderId": "oid-10", "orderLinkId": "x"},
            "time": 1700007200000i64
        })))
        .mount(&server)
        .await;

    let client = BybitRest::new(server.uri(), creds());
    client
        .place_limit_leg(LimitLeg {
            reduce_only: true,
            ..leg()
        })
        .await
        .expect("closing leg placed");
}

#[tokio::test]
async fn a_resting_order_is_found_in_the_realtime_table() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/v5/order/realtime"))
        .and(query_param("orderLinkId", "link-1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "retCode": 0, "retMsg": "OK",
            "result": {"list": [{
                "symbol": "AAVEUSDT", "orderId": "oid-1", "orderLinkId": "link-1",
                "side": "Buy", "price": "300.25", "qty": "0.5",
                "cumExecQty": "0", "avgPrice": "", "orderStatus": "New",
                "createdTime": "1700007200000", "updatedTime": "1700007201000"
            }]},
            "time": 1700007202000i64
        })))
        .mount(&server)
        .await;

    let client = BybitRest::new(server.uri(), creds());
    let got = client
        .order_by_link_id(&Symbol::new("AAVEUSDT"), "link-1")
        .await
        .expect("query succeeded")
        .expect("order found");
    assert_eq!(got.state, OrderState::New);
    assert_eq!(got.cum_exec_qty, dec!(0));
    // An empty avgPrice means nothing filled; it must decode to zero rather
    // than failing the whole order.
    assert_eq!(got.avg_price, dec!(0));
    assert!(!got.state.is_terminal());
}

#[tokio::test]
async fn a_filled_order_that_has_left_the_realtime_table_is_found_in_history() {
    let server = MockServer::start().await;
    // Bybit drops terminal orders out of /order/realtime after a short window.
    // Checking only realtime is how the Python's wait loop could time out on an
    // order that had in fact filled.
    Mock::given(method("GET"))
        .and(path("/v5/order/realtime"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "retCode": 0, "retMsg": "OK", "result": {"list": []}, "time": 1700007202000i64
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/v5/order/history"))
        .and(query_param("orderLinkId", "link-2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "retCode": 0, "retMsg": "OK",
            "result": {"list": [{
                "symbol": "AAVEUSDT", "orderId": "oid-2", "orderLinkId": "link-2",
                "side": "Sell", "price": "300.25", "qty": "0.5",
                "cumExecQty": "0.5", "avgPrice": "300.31", "orderStatus": "Filled",
                "createdTime": "1700007200000", "updatedTime": "1700007205000"
            }]},
            "time": 1700007206000i64
        })))
        .mount(&server)
        .await;

    let client = BybitRest::new(server.uri(), creds());
    let got = client
        .order_by_link_id(&Symbol::new("AAVEUSDT"), "link-2")
        .await
        .expect("query succeeded")
        .expect("order found in history");
    assert_eq!(got.state, OrderState::Filled);
    assert_eq!(got.avg_price, dec!(300.31));
    assert_eq!(got.cum_exec_qty, dec!(0.5));
    assert!(got.state.is_terminal());
}

#[tokio::test]
async fn an_unknown_link_id_is_none_rather_than_an_error() {
    // A placement that never reached the exchange must be distinguishable
    // from one that was rejected. `None` says "no such order"; an Err would
    // send the executor down the unwind path for a leg that does not exist.
    let server = MockServer::start().await;
    for p in ["/v5/order/realtime", "/v5/order/history"] {
        Mock::given(method("GET"))
            .and(path(p))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "retCode": 0, "retMsg": "OK", "result": {"list": []}, "time": 1700007202000i64
            })))
            .mount(&server)
            .await;
    }

    let client = BybitRest::new(server.uri(), creds());
    let got = client
        .order_by_link_id(&Symbol::new("AAVEUSDT"), "nope")
        .await
        .expect("query succeeded");
    assert!(got.is_none());
}
