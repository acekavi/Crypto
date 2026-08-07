use std::sync::Arc;

use bot::engine_loop::{CandleOutcome, EngineLoop, SkipReason};
use botcore::{Candle, Instrument, Symbol, Timeframe};
use engine::mock::MockExchange;
use persistence::Journal;
use risk::{Refusal, RiskManager, RiskParams};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use strategy::{PullbackStrategy, pullback::StrategyParams};

const H1: i64 = 3_600_000;

fn instrument() -> Instrument {
    Instrument {
        symbol: Symbol::new("BTCUSDT"),
        tick_size: dec!(0.1),
        qty_step: dec!(0.001),
        min_order_qty: dec!(0.001),
        launch_time_ms: 0,
    }
}

fn candle(open_time_ms: i64) -> Candle {
    Candle {
        open_time_ms,
        open: Decimal::from(100),
        high: Decimal::from(101),
        low: Decimal::from(99),
        close: Decimal::from(100),
        volume: Decimal::ZERO,
        turnover: Decimal::ZERO,
    }
}

fn candle_hlc(open_time_ms: i64, high: Decimal, low: Decimal, close: Decimal) -> Candle {
    Candle {
        open_time_ms,
        open: close,
        high,
        low,
        close,
        volume: Decimal::ZERO,
        turnover: Decimal::ZERO,
    }
}

async fn loop_with(mock: Arc<MockExchange>) -> (EngineLoop, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let j = Arc::new(
        Journal::open_local(dir.path().join("l.db").to_str().unwrap())
            .await
            .expect("journal"),
    );
    let el = EngineLoop::new(
        Box::new(PullbackStrategy::new(StrategyParams::defaults())),
        RiskManager::new(RiskParams::defaults(), dec!(0.3)),
        mock,
        j,
        vec![instrument()],
        "cfg".into(),
        3,
        250,
    );
    (el, dir)
}

#[tokio::test]
async fn a_cold_stream_never_places_an_order() {
    // Acting on a half-warm indicator is how a restart produces a bad trade.
    let mock = Arc::new(MockExchange::new());
    let (mut el, _d) = loop_with(mock.clone()).await;

    let out = el
        .on_candle_closed(&Symbol::new("BTCUSDT"), Timeframe::H1, &candle(0))
        .await
        .expect("handled");

    assert!(matches!(out, CandleOutcome::Skipped(SkipReason::NotWarm)));
    assert!(mock.placed_orders().is_empty());
}

#[tokio::test]
async fn a_duplicate_candle_is_skipped_without_reaching_the_strategy() {
    let mock = Arc::new(MockExchange::new());
    let (mut el, _d) = loop_with(mock.clone()).await;

    el.on_candle_closed(&Symbol::new("BTCUSDT"), Timeframe::H1, &candle(H1))
        .await
        .expect("first");
    let out = el
        .on_candle_closed(&Symbol::new("BTCUSDT"), Timeframe::H1, &candle(H1))
        .await
        .expect("duplicate");

    assert!(matches!(
        out,
        CandleOutcome::Skipped(SkipReason::NotAccepted)
    ));
}

#[tokio::test]
async fn a_symbol_with_no_instrument_metadata_is_skipped() {
    // Without tick size and qty step no valid order could be formed.
    let mock = Arc::new(MockExchange::new());
    let (mut el, _d) = loop_with(mock.clone()).await;

    let out = el
        .on_candle_closed(&Symbol::new("GHOSTUSDT"), Timeframe::H1, &candle(0))
        .await
        .expect("handled");

    assert!(matches!(
        out,
        CandleOutcome::Skipped(SkipReason::UnknownInstrument)
    ));
    assert!(mock.placed_orders().is_empty());
}

#[tokio::test]
async fn an_expired_resting_order_is_cancelled_on_a_later_candle() {
    use botcore::Side;
    use engine::RestingOrder;

    let mock = Arc::new(MockExchange::new());
    let (mut el, _d) = loop_with(mock.clone()).await;

    el.track_for_test(RestingOrder {
        link_id: "old".into(),
        symbol: Symbol::new("BTCUSDT"),
        side: Side::Buy,
        qty: dec!(1),
        cum_exec_qty: Decimal::ZERO,
        placed_at_candle_ms: 0,
    });

    el.on_candle_closed(&Symbol::new("BTCUSDT"), Timeframe::H1, &candle(3 * H1))
        .await
        .expect("handled");

    assert_eq!(
        mock.cancelled(),
        vec!["old".to_string()],
        "an expired entry must be cancelled"
    );
}

#[tokio::test]
async fn a_fatal_cancel_failure_on_an_expired_order_halts_instead_of_being_swallowed() {
    use botcore::{ErrorClass, Side};
    use engine::RestingOrder;
    use exchange::bybit::transport::ExchangeError;

    // 10004 is Bybit's "bad sign" retCode, which classifies Fatal. A revoked
    // or invalid key means every subsequent exchange call is doomed the same
    // way, so cancelling an expired entry must propagate this rather than
    // being logged and skipped like an ordinary cancel failure.
    let fatal = ExchangeError::Api {
        code: 10004,
        msg: "bad sign".into(),
    };
    assert_eq!(fatal.class(), ErrorClass::Fatal);

    let mock = Arc::new(MockExchange::new().fail_cancel_always(fatal));
    let (mut el, _d) = loop_with(mock.clone()).await;

    el.track_for_test(RestingOrder {
        link_id: "old".into(),
        symbol: Symbol::new("BTCUSDT"),
        side: Side::Buy,
        qty: dec!(1),
        cum_exec_qty: Decimal::ZERO,
        placed_at_candle_ms: 0,
    });

    let err = el
        .on_candle_closed(&Symbol::new("BTCUSDT"), Timeframe::H1, &candle(3 * H1))
        .await
        .expect_err("a Fatal cancel failure must propagate, not be swallowed");

    assert_eq!(err.class(), ErrorClass::Fatal);
    assert!(
        mock.cancelled().is_empty(),
        "a failed cancel must not be recorded as cancelled"
    );
}

#[tokio::test]
async fn an_account_event_marking_an_order_filled_stops_it_from_resting() {
    // Without EngineLoop::on_account_event feeding the private feed into the
    // tracker, the tracker never learns a resting entry filled: it would
    // still believe the entry is resting and, on expiry, try to cancel an
    // order the exchange already executed.
    use botcore::{OpenOrder, OrderState, Side};
    use engine::RestingOrder;
    use exchange::bybit::ws_private::AccountEvent;

    let mock = Arc::new(MockExchange::new());
    let (mut el, _d) = loop_with(mock.clone()).await;

    el.track_for_test(RestingOrder {
        link_id: "filled-entry".into(),
        symbol: Symbol::new("BTCUSDT"),
        side: Side::Buy,
        qty: dec!(1),
        cum_exec_qty: Decimal::ZERO,
        placed_at_candle_ms: 0,
    });
    assert!(
        el.tracker_mut().is_resting("filled-entry"),
        "setup: the order must start out resting"
    );

    let event = AccountEvent::OrderUpdate(OpenOrder {
        symbol: Symbol::new("BTCUSDT"),
        order_id: "oid-1".into(),
        order_link_id: "filled-entry".into(),
        side: Side::Buy,
        price: dec!(100),
        qty: dec!(1),
        cum_exec_qty: dec!(1),
        state: OrderState::Filled,
        created_time_ms: 0,
        updated_time_ms: 0,
    });

    el.on_account_event(&event).await;

    assert!(
        !el.tracker_mut().is_resting("filled-entry"),
        "a Filled account event must reach OrderTracker::on_order_update so it stops \
         treating the order as resting"
    );
}

#[tokio::test]
async fn a_fill_that_crosses_utc_midnight_counts_toward_the_later_day() {
    // The daily fill cap counts at fill time, not placement time. An order
    // placed at 23:50 UTC that fills five minutes into the next day must be
    // attributed to the day it filled, not the day it was placed — otherwise
    // the cap could let a 6th entry through on the day the fill actually
    // landed.
    use botcore::{OpenOrder, OrderState, Side};
    use engine::utc_day_start_ms;
    use exchange::bybit::ws_private::AccountEvent;
    use persistence::{Journal, OrderRecord};

    let dir = tempfile::tempdir().expect("tempdir");
    let journal = Arc::new(
        Journal::open_local(dir.path().join("j.db").to_str().unwrap())
            .await
            .expect("journal"),
    );
    let mut el = EngineLoop::new(
        Box::new(PullbackStrategy::new(StrategyParams::defaults())),
        RiskManager::new(RiskParams::defaults(), dec!(0.3)),
        Arc::new(MockExchange::new()),
        Arc::clone(&journal),
        vec![instrument()],
        "cfg".into(),
        3,
        250,
    );

    // An arbitrary day boundary, far from the epoch, plus/minus ten minutes.
    let day2_start = utc_day_start_ms(10 * 86_400_000);
    let created_before_midnight = day2_start - 10 * 60_000;
    let updated_after_midnight = day2_start + 5 * 60_000;
    let day1_start = utc_day_start_ms(created_before_midnight);

    // The order must already exist in the journal for update_order_state to
    // have a row to update — mirrors what Executor::place_entry does at
    // placement time in production.
    journal
        .record_order(&OrderRecord {
            order_link_id: "cross-midnight".into(),
            order_id: Some("oid-cross".into()),
            symbol: Symbol::new("BTCUSDT"),
            side: Side::Buy,
            price: dec!(100),
            qty: dec!(1),
            stop_loss: dec!(90),
            take_profit: dec!(120),
            state: OrderState::New,
            cum_exec_qty: dec!(0),
            config_hash: "cfg".into(),
            created_at_ms: created_before_midnight,
        })
        .await
        .expect("seed the order");

    let event = AccountEvent::OrderUpdate(OpenOrder {
        symbol: Symbol::new("BTCUSDT"),
        order_id: "oid-cross".into(),
        order_link_id: "cross-midnight".into(),
        side: Side::Buy,
        price: dec!(100),
        qty: dec!(1),
        cum_exec_qty: dec!(1),
        state: OrderState::Filled,
        created_time_ms: created_before_midnight,
        updated_time_ms: updated_after_midnight,
    });

    el.on_account_event(&event).await;

    assert_eq!(
        journal.daily_fill_count(day1_start).await.expect("count"),
        0,
        "the fill must not be attributed to the day the order was placed"
    );
    assert_eq!(
        journal.daily_fill_count(day2_start).await.expect("count"),
        1,
        "the fill must be attributed to the day it actually filled"
    );
}

#[tokio::test]
async fn a_zero_equity_account_refuses_rather_than_placing() {
    // An unfunded testnet account must produce a named refusal, not an order.
    //
    // This drives a genuine engineered long setup (a rising 4h bias, a 1h
    // pullback into EMA20, and an RSI cross back through the long trigger)
    // all the way to `risk.evaluate`, so the "no orders placed" assertion
    // below actually proves the zero-equity refusal rather than merely
    // observing that a strategy which never produced a signal also never
    // placed an order — the flaw in the version this replaces. The numeric
    // shape (ramp, dip, reversal) is adapted from
    // crates/strategy/tests/pullback_setups.rs's proven
    // `an_engineered_long_setup_fires_with_coherent_geometry`, extended so a
    // full 260-candle 4h bias series can be interleaved at Bybit's real
    // cadence (1 four-hour candle per 4 one-hour candles) without either
    // stream ever going stale. A standalone probe against the strategy
    // crate directly confirmed offsets 1.5-3.5 (from the reversal candle's
    // close) all fire a signal on this longer ramp; 2.5 is used here for
    // margin.
    use botcore::Balance;
    let mock = Arc::new(MockExchange::new().with_balance(Balance {
        equity: Decimal::ZERO,
        available: Decimal::ZERO,
    }));
    let (mut el, _d) = loop_with(mock.clone()).await;
    let sym = Symbol::new("BTCUSDT");

    // Pre-seed both CandleStore windows so `is_warm` holds from the very
    // first real candle, and both streams share the same anchor (T=0) so
    // the cross-timeframe staleness gate never trips before a real candle
    // has been fed. `CandleStore::warm` does not require its seed to be
    // evenly spaced — only the count (>=250, for `is_warm`) and the latest
    // timestamp (which becomes `last_open_ms`) matter.
    let seed: Vec<Candle> = (-249..=0).map(candle).collect();
    el.warm(&sym, Timeframe::H1, seed.clone());
    el.warm(&sym, Timeframe::H4, seed);

    const RAMP: i64 = 1040;
    const DIP: i64 = 14;
    let mut px = Decimal::from(100);
    let mut px_h4 = Decimal::from(100);
    let mut refused = false;

    // Drive H1 forward one hour at a time; every 4th hour also drive H4 —
    // Bybit's real cadence, since a 4h candle is exactly 4 1h candles — so
    // neither stream ever goes stale relative to the other.
    for i in 1..=(RAMP + DIP + 1) {
        let t = i * H1;
        let h1 = if i <= RAMP {
            // Uptrend: warms EMA20/RSI14/ATR14 and, via the interleaved H4
            // candles below, establishes a long bias (EMA50 > EMA200).
            let c = candle_hlc(t, px + dec!(1), px - dec!(1), px);
            px += dec!(0.5);
            c
        } else if i <= RAMP + DIP {
            // Dip: walks RSI down through the 40 long trigger.
            px -= dec!(0.5);
            candle_hlc(t, px + dec!(1), px - dec!(1), px)
        } else {
            // Reversal: closes back up (crossing RSI back above 40) while
            // its low lands on EMA20, completing the pullback.
            let close = px + dec!(5);
            candle_hlc(t, close + dec!(1), close - dec!(2.5), close)
        };

        let out = el
            .on_candle_closed(&sym, Timeframe::H1, &h1)
            .await
            .expect("handled");
        if matches!(out, CandleOutcome::Refused(_)) {
            refused = true;
        }

        if i % 4 == 0 {
            let h4 = candle_hlc(t, px_h4 + dec!(1), px_h4 - dec!(1), px_h4);
            px_h4 += dec!(0.5);
            el.on_candle_closed(&sym, Timeframe::H4, &h4)
                .await
                .expect("handled");
        }
    }

    assert!(
        refused,
        "the engineered setup must reach the risk layer and be refused for zero equity"
    );
    assert!(
        mock.placed_orders().is_empty(),
        "a zero-equity account must never place an order"
    );
}

#[tokio::test]
async fn a_stale_required_timeframe_blocks_an_otherwise_valid_candle() {
    // The pullback strategy needs both H1 and H4: H4 sets the bias, H1 times
    // the entry. If the H4 feed dies while H1 keeps flowing, trading on that
    // stale bias must be refused even though the H1 stream itself is fine.
    let mock = Arc::new(MockExchange::new());
    let (mut el, _d) = loop_with(mock.clone()).await;
    let sym = Symbol::new("BTCUSDT");

    // Warm H1 to satisfy the warmup gate on its own.
    let h1_history: Vec<Candle> = (0..250).map(|i| candle(i * H1)).collect();
    el.warm(&sym, Timeframe::H1, h1_history);

    // H4's last candle sits at time 0 and is never advanced.
    el.warm(&sym, Timeframe::H4, vec![candle(0)]);

    // The next H1 candle (index 250) opens at 250h, far more than 2*H4
    // (8h) after H4's last candle at 0 — H4 has gone stale.
    let out = el
        .on_candle_closed(&sym, Timeframe::H1, &candle(250 * H1))
        .await
        .expect("handled");

    assert!(
        matches!(out, CandleOutcome::Skipped(SkipReason::Stale)),
        "a stale H4 bias feed must block the candle even though H1 is fine, got {out:?}"
    );
}

#[tokio::test]
async fn a_fresh_pair_of_required_timeframes_does_not_trigger_staleness() {
    let mock = Arc::new(MockExchange::new());
    let (mut el, _d) = loop_with(mock.clone()).await;
    let sym = Symbol::new("BTCUSDT");

    // H4's last candle is recent relative to the incoming H1 candle.
    el.warm(&sym, Timeframe::H4, vec![candle(0)]);

    let out = el
        .on_candle_closed(&sym, Timeframe::H1, &candle(H1))
        .await
        .expect("handled");

    // Deliberately not asserting which gate produced the outcome (this
    // stream is not warm yet) — only that staleness is not it.
    assert!(!matches!(out, CandleOutcome::Skipped(SkipReason::Stale)));
}

const DAY: i64 = 86_400_000;

#[tokio::test]
async fn the_day_start_equity_baseline_rolls_over_at_the_utc_day_boundary() {
    // Not `is_zero()`: a baseline captured only once for the life of the
    // process would let the "daily" drawdown halt keep measuring against
    // whatever equity existed at first startup, silently degenerating into
    // a permanent since-launch check the longer the bot stays up.
    //
    // `MockExchange`'s balance is fixed at construction with no setter, and
    // changing that is outside this task — so this asserts the mechanism
    // that was actually broken (the day boundary the baseline is captured
    // for) rather than the baseline's value, which cannot vary here anyway.
    // Comfortably past the epoch UTC day so `day_start_ms`'s initial `0`
    // sentinel cannot coincide with a legitimately-epoch-adjacent boundary.
    let mock = Arc::new(MockExchange::new());
    let (mut el, _d) = loop_with(mock.clone()).await;

    let day5_start = 5 * DAY;
    el.account_state_for_test(day5_start + H1)
        .await
        .expect("day 5 state");
    let (captured_day, equity) = el.day_baseline_for_test();
    assert_eq!(captured_day, day5_start, "must capture day 5's own start");
    assert_eq!(equity, dec!(10000), "must capture the current equity");

    // Still day 5: the baseline must not move.
    el.account_state_for_test(day5_start + 20 * H1)
        .await
        .expect("still day 5");
    let (unmoved_day, _) = el.day_baseline_for_test();
    assert_eq!(unmoved_day, day5_start, "must not move within the same day");

    // Cross into day 6.
    let day6_start = 6 * DAY;
    el.account_state_for_test(day6_start + H1)
        .await
        .expect("day 6 state");
    let (rolled_day, _) = el.day_baseline_for_test();
    assert_eq!(
        rolled_day, day6_start,
        "must roll over to day 6's own start"
    );
}

#[tokio::test]
async fn a_drawdown_refusal_persists_a_halt_that_a_fresh_engine_loop_then_sees() {
    // Proves the durability fix end-to-end: a live drawdown breach detected
    // by `on_candle_closed` must be written to the journal, not merely
    // returned as an in-memory `Refusal` that vanishes the moment the
    // process restarts.
    use botcore::Balance;

    let dir = tempfile::tempdir().expect("tempdir");
    let db_path = dir.path().join("l.db");
    let db_path = db_path.to_str().unwrap().to_string();

    // The account is far down from a recorded all-time peak, so the first
    // signal the engineered setup below produces must be refused for
    // TotalDrawdown before any sizing is even attempted.
    let mock = Arc::new(MockExchange::new().with_balance(Balance {
        equity: dec!(8000),
        available: dec!(7000),
    }));
    let sym = Symbol::new("BTCUSDT");

    let observed_refusal = {
        let j = Arc::new(Journal::open_local(&db_path).await.expect("journal opens"));
        j.record_equity(dec!(20000), 0)
            .await
            .expect("seed the all-time peak");

        let mut el = EngineLoop::new(
            Box::new(PullbackStrategy::new(StrategyParams::defaults())),
            RiskManager::new(RiskParams::defaults(), dec!(0.3)),
            mock.clone(),
            j.clone(),
            vec![instrument()],
            "cfg".into(),
            3,
            250,
        );
        el.load_baselines(H1).await.expect("load baselines");

        // Same engineered long setup as
        // `a_zero_equity_account_refuses_rather_than_placing`: a rising 4h
        // bias, a 1h pullback into EMA20, and an RSI cross back through the
        // long trigger, driving the pullback strategy all the way to a real
        // signal so `risk.evaluate` is actually exercised.
        let seed: Vec<Candle> = (-249..=0).map(candle).collect();
        el.warm(&sym, Timeframe::H1, seed.clone());
        el.warm(&sym, Timeframe::H4, seed);

        const RAMP: i64 = 1040;
        const DIP: i64 = 14;
        let mut px = Decimal::from(100);
        let mut px_h4 = Decimal::from(100);
        let mut observed_refusal = None;

        for i in 1..=(RAMP + DIP + 1) {
            let t = i * H1;
            let h1 = if i <= RAMP {
                let c = candle_hlc(t, px + dec!(1), px - dec!(1), px);
                px += dec!(0.5);
                c
            } else if i <= RAMP + DIP {
                px -= dec!(0.5);
                candle_hlc(t, px + dec!(1), px - dec!(1), px)
            } else {
                let close = px + dec!(5);
                candle_hlc(t, close + dec!(1), close - dec!(2.5), close)
            };

            let out = el
                .on_candle_closed(&sym, Timeframe::H1, &h1)
                .await
                .expect("handled");
            if let CandleOutcome::Refused(r) = out {
                observed_refusal = Some(r);
            }

            if i % 4 == 0 {
                let h4 = candle_hlc(t, px_h4 + dec!(1), px_h4 - dec!(1), px_h4);
                px_h4 += dec!(0.5);
                el.on_candle_closed(&sym, Timeframe::H4, &h4)
                    .await
                    .expect("handled");
            }
        }

        observed_refusal
    };

    let refusal =
        observed_refusal.expect("the engineered setup must reach risk.evaluate and be refused");
    assert!(
        matches!(refusal, Refusal::TotalDrawdown { .. }),
        "expected a TotalDrawdown refusal, got {refusal:?}"
    );

    // A fresh EngineLoop over a fresh handle to the SAME database file: this
    // is the only thing that can prove the halt reached disk rather than
    // living only in the first EngineLoop's memory.
    let j2 = Arc::new(
        Journal::open_local(&db_path)
            .await
            .expect("journal reopens"),
    );
    let mut el2 = EngineLoop::new(
        Box::new(PullbackStrategy::new(StrategyParams::defaults())),
        RiskManager::new(RiskParams::defaults(), dec!(0.3)),
        mock.clone(),
        j2,
        vec![instrument()],
        "cfg".into(),
        3,
        250,
    );
    let state = el2.account_state_for_test(H1).await.expect("account state");
    assert!(
        state.halt_reason.is_some(),
        "a fresh EngineLoop must see the halt the first one persisted"
    );
}
