// Captured live from api-testnet.bybit.com/v5/market/funding/history
// (2026-08-07, category=linear, symbol=BTCUSDT, limit=3):
//
// {"retCode":0,"retMsg":"OK","result":{"category":"linear","list":[
//   {"symbol":"BTCUSDT","fundingRate":"0.0001","fundingRateTimestamp":"1786060800000"},
//   {"symbol":"BTCUSDT","fundingRate":"-0.005","fundingRateTimestamp":"1786032000000"},
//   {"symbol":"BTCUSDT","fundingRate":"-0.005","fundingRateTimestamp":"1786003200000"}
// ]},"retExtInfo":{},"time":1786083673313}
//
// `list` is newest-first (timestamps strictly decrease), the same ordering
// `/v5/market/kline` uses. Field names are `fundingRate` and
// `fundingRateTimestamp`, both string-encoded.

use exchange::bybit::wire::FundingRateRow;
use rust_decimal_macros::dec;

#[test]
fn a_positive_funding_rate_row_parses_into_an_exact_decimal() {
    let raw =
        r#"{"symbol":"BTCUSDT","fundingRate":"0.0001","fundingRateTimestamp":"1786060800000"}"#;
    let row: FundingRateRow = serde_json::from_str(raw).expect("row parses");
    let rate = row.into_funding_rate().expect("row converts");

    assert_eq!(rate.symbol.as_str(), "BTCUSDT");
    assert_eq!(rate.funding_time_ms, 1_786_060_800_000);
    assert_eq!(rate.rate, dec!(0.0001));
}

#[test]
fn a_negative_funding_rate_row_keeps_its_sign() {
    // Captured live: shorts were paid this period. Losing or flipping the
    // sign here would silently invert the cost of every short position in
    // every backtest run over this data.
    let raw =
        r#"{"symbol":"BTCUSDT","fundingRate":"-0.005","fundingRateTimestamp":"1786032000000"}"#;
    let row: FundingRateRow = serde_json::from_str(raw).expect("row parses");
    let rate = row.into_funding_rate().expect("row converts");

    assert_eq!(rate.rate, dec!(-0.005));
    assert!(rate.rate.is_sign_negative());
}
