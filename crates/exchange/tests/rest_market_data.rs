use botcore::{Symbol, Timeframe};
use exchange::bybit::rest::BybitRest;
use exchange::bybit::sign::Credentials;
use rust_decimal_macros::dec;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn creds() -> Credentials {
    Credentials { api_key: "k".into(), api_secret: "s".into() }
}

#[tokio::test]
async fn klines_are_returned_oldest_first() {
    let server = MockServer::start().await;

    // Bybit returns klines newest-first; the client must reverse them so
    // indicators are fed in chronological order.
    let body = serde_json::json!({
        "retCode": 0,
        "retMsg": "OK",
        "result": {
            "list": [
                ["1700003600000","102","103","101","102.5","1","100"],
                ["1700000000000","100","101","99","100.5","1","100"]
            ]
        },
        "time": 1700007200000i64
    });

    Mock::given(method("GET"))
        .and(path("/v5/market/kline"))
        .and(query_param("category", "linear"))
        .and(query_param("symbol", "BTCUSDT"))
        .and(query_param("interval", "60"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;

    let client = BybitRest::new(server.uri(), creds());
    let candles = client
        .klines(&Symbol::new("BTCUSDT"), Timeframe::H1, 2)
        .await
        .expect("klines fetch succeeds");

    assert_eq!(candles.len(), 2);
    assert_eq!(candles[0].open_time_ms, 1_700_000_000_000, "oldest must come first");
    assert_eq!(candles[1].open_time_ms, 1_700_003_600_000);
    assert_eq!(candles[0].close, dec!(100.5));
}

#[tokio::test]
async fn non_trading_instruments_are_filtered_out() {
    let server = MockServer::start().await;

    let body = serde_json::json!({
        "retCode": 0,
        "retMsg": "OK",
        "result": {
            "list": [
                {
                    "symbol": "BTCUSDT", "status": "Trading", "launchTime": "1600000000000",
                    "priceFilter": {"tickSize": "0.1"},
                    "lotSizeFilter": {"qtyStep": "0.001", "minOrderQty": "0.001"}
                },
                {
                    "symbol": "DEADUSDT", "status": "Delivering", "launchTime": "1600000000000",
                    "priceFilter": {"tickSize": "0.1"},
                    "lotSizeFilter": {"qtyStep": "0.001", "minOrderQty": "0.001"}
                }
            ]
        },
        "time": 1700007200000i64
    });

    Mock::given(method("GET"))
        .and(path("/v5/market/instruments-info"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;

    let client = BybitRest::new(server.uri(), creds());
    let instruments = client.instruments().await.expect("instruments fetch succeeds");

    assert_eq!(instruments.len(), 1, "delivering instrument must be dropped");
    assert_eq!(instruments[0].symbol.as_str(), "BTCUSDT");
    assert_eq!(instruments[0].tick_size, dec!(0.1));
}

#[tokio::test]
async fn server_time_updates_the_clock_offset() {
    let server = MockServer::start().await;
    let body = serde_json::json!({
        "retCode": 0, "retMsg": "OK",
        "result": {"list": []},
        "time": 1700007200000i64
    });
    Mock::given(method("GET"))
        .and(path("/v5/market/tickers"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;

    let client = BybitRest::new(server.uri(), creds());
    client.tickers().await.expect("tickers fetch succeeds");

    // The mock's server time is far in the past relative to now, so the offset
    // must have moved off its zero default.
    assert_ne!(client.clock().offset_ms(), 0, "clock offset was never observed");
}

#[tokio::test]
async fn api_error_code_is_surfaced_not_swallowed() {
    let server = MockServer::start().await;
    let body = serde_json::json!({
        "retCode": 10001, "retMsg": "param error", "result": {}, "time": 1700007200000i64
    });
    Mock::given(method("GET"))
        .and(path("/v5/market/tickers"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;

    let client = BybitRest::new(server.uri(), creds());
    let err = client.tickers().await.expect_err("retCode 10001 must fail");
    assert!(err.to_string().contains("10001"), "error lost the retCode: {err}");
}
