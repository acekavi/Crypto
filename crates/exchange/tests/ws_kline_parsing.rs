use botcore::Symbol;
use botcore::Timeframe;
use exchange::Subscription;
use exchange::bybit::ws_public::{parse_kline_message, topic_for};
use rust_decimal_macros::dec;

#[test]
fn topic_uses_bybit_interval_notation() {
    let sub = Subscription {
        symbol: Symbol::new("BTCUSDT"),
        timeframe: Timeframe::H1,
    };
    assert_eq!(topic_for(&sub), "kline.60.BTCUSDT");
    let sub4 = Subscription {
        symbol: Symbol::new("ETHUSDT"),
        timeframe: Timeframe::H4,
    };
    assert_eq!(topic_for(&sub4), "kline.240.ETHUSDT");
}

#[test]
fn only_confirmed_candles_are_emitted() {
    // Bybit streams the in-progress candle continuously with confirm=false and
    // sends confirm=true exactly once when it closes. Acting on an unconfirmed
    // candle would mean trading a bar that can still change.
    let raw = r#"{
      "topic":"kline.60.BTCUSDT","type":"snapshot","ts":1700003600000,
      "data":[{"start":1700000000000,"end":1700003599999,"interval":"60",
               "open":"100","close":"102","high":"103","low":"99",
               "volume":"5","turnover":"510","confirm":false,"timestamp":1700003599000}]
    }"#;
    let parsed = parse_kline_message(raw).expect("parses");
    assert_eq!(
        parsed,
        Some(vec![]),
        "unconfirmed candle must not be emitted"
    );
}

#[test]
fn confirmed_candle_parses_into_domain_candle() {
    let raw = r#"{
      "topic":"kline.60.BTCUSDT","type":"snapshot","ts":1700003600000,
      "data":[{"start":1700000000000,"end":1700003599999,"interval":"60",
               "open":"100.5","close":"102.25","high":"103","low":"99.75",
               "volume":"5.5","turnover":"510.25","confirm":true,"timestamp":1700003600000}]
    }"#;
    let parsed = parse_kline_message(raw)
        .expect("parses")
        .expect("is a kline frame");
    assert_eq!(parsed.len(), 1);
    let (symbol, tf, candle) = &parsed[0];
    assert_eq!(symbol.as_str(), "BTCUSDT");
    assert_eq!(*tf, Timeframe::H1);
    assert_eq!(candle.open_time_ms, 1_700_000_000_000);
    assert_eq!(candle.open, dec!(100.5));
    assert_eq!(candle.close, dec!(102.25));
    assert_eq!(candle.high, dec!(103));
    assert_eq!(candle.low, dec!(99.75));
}

#[test]
fn non_kline_frames_are_ignored_not_errors() {
    // Subscription acks and pongs share the socket; they must not be treated
    // as failures or the feed would reconnect in a loop.
    assert_eq!(
        parse_kline_message(r#"{"success":true,"op":"subscribe"}"#).unwrap(),
        None
    );
    assert_eq!(
        parse_kline_message(r#"{"op":"pong","success":true}"#).unwrap(),
        None
    );
}

#[test]
fn unknown_interval_in_topic_is_an_error() {
    let raw = r#"{"topic":"kline.5.BTCUSDT","type":"snapshot","ts":1,"data":[]}"#;
    assert!(
        parse_kline_message(raw).is_err(),
        "unsupported interval must not be silently dropped"
    );
}

use exchange::bybit::ws_public::missing_candle_count;

#[test]
fn consecutive_candles_show_no_gap() {
    let prev = 1_700_000_000_000;
    let next = prev + Timeframe::H1.duration_ms();
    assert_eq!(missing_candle_count(prev, next, Timeframe::H1), 0);
}

#[test]
fn a_skipped_candle_is_detected() {
    // Two hours elapsed on a 1h feed means exactly one candle went missing.
    let prev = 1_700_000_000_000;
    let next = prev + 2 * Timeframe::H1.duration_ms();
    assert_eq!(missing_candle_count(prev, next, Timeframe::H1), 1);
}

#[test]
fn duplicate_or_out_of_order_candles_report_no_gap() {
    let prev = 1_700_000_000_000;
    assert_eq!(missing_candle_count(prev, prev, Timeframe::H1), 0);
    assert_eq!(
        missing_candle_count(prev, prev - 3_600_000, Timeframe::H1),
        0
    );
}

use exchange::bybit::ws_public::{GapAction, gap_action};

#[test]
fn first_ever_candle_for_a_stream_is_just_emitted() {
    let next = 1_700_000_000_000;
    assert_eq!(gap_action(None, next, Timeframe::H1), GapAction::Emit);
}

#[test]
fn consecutive_candle_is_emitted_with_no_backfill() {
    let prev = 1_700_000_000_000;
    let next = prev + Timeframe::H1.duration_ms();
    assert_eq!(gap_action(Some(prev), next, Timeframe::H1), GapAction::Emit);
}

#[test]
fn one_missing_candle_requires_backfill_before_emit() {
    let prev = 1_700_000_000_000;
    let next = prev + 2 * Timeframe::H1.duration_ms();
    assert_eq!(
        gap_action(Some(prev), next, Timeframe::H1),
        GapAction::BackfillThenEmit { missing: 1 }
    );
}

#[test]
fn duplicate_or_out_of_order_candle_is_just_emitted() {
    let prev = 1_700_000_000_000;
    assert_eq!(gap_action(Some(prev), prev, Timeframe::H1), GapAction::Emit);
    assert_eq!(
        gap_action(Some(prev), prev - 3_600_000, Timeframe::H1),
        GapAction::Emit
    );
}
