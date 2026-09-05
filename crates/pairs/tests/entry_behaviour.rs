mod support;

use botcore::{Instrument, Symbol, Timeframe};
use pairs::executor::{ExecutionError, ExecutorConfig, LegQuote, open_pair};
use pairs::{PairParams, PairSide};
use rust_decimal_macros::dec;
use support::{FaultExchange, LegAction};

fn instrument(symbol: &str) -> Instrument {
    Instrument {
        symbol: Symbol::new(symbol),
        tick_size: dec!(0.01),
        qty_step: dec!(0.01),
        min_order_qty: dec!(0.01),
        min_notional: dec!(5),
        launch_time_ms: 0,
    }
}

fn quote(symbol: &str) -> LegQuote {
    LegQuote {
        instrument: instrument(symbol),
        bid: dec!(99.90),
        ask: dec!(100.10),
        last: dec!(100.00),
    }
}

fn params() -> PairParams {
    PairParams {
        leg_a: Symbol::new("AAVEUSDT"),
        leg_b: Symbol::new("ETHUSDT"),
        timeframe: Timeframe::H1,
        rolling_window: 180,
        entry_z: 3.0,
        stop_z: 4.0,
        target_z: 0.0,
        max_hold_bars: 48,
        fee_per_leg: dec!(0.0002),
        per_leg_notional_usdt: dec!(25),
        risk_pct_of_equity: dec!(0.03),
        max_notional_multiple_of_equity: dec!(1),
        enable_breakeven: false,
        breakeven_r_multiple: dec!(2),
    }
}

/// Zero delays so the tests exercise the logic, not the clock.
fn cfg() -> ExecutorConfig {
    ExecutorConfig {
        ticks_through: dec!(5),
        fill_timeout: std::time::Duration::ZERO,
        poll_interval: std::time::Duration::ZERO,
        unwind_ladder: vec![dec!(10), dec!(25), dec!(60)],
    }
}

#[tokio::test]
async fn both_legs_filling_opens_the_position() {
    let ex = FaultExchange::new();
    ex.script("AAVEUSDT", vec![LegAction::Fills { price: dec!(100.10) }]);
    ex.script("ETHUSDT", vec![LegAction::Fills { price: dec!(99.90) }]);

    let opened = open_pair(
        &ex,
        &cfg(),
        &params(),
        (&quote("AAVEUSDT"), &quote("ETHUSDT")),
        PairSide::LongSpread,
        dec!(1000),
        1_700_000_000_000,
    )
    .await
    .expect("no execution error")
    .expect("a position was opened");

    assert_eq!(opened.side, PairSide::LongSpread);
    assert_eq!(opened.a.avg_price, dec!(100.10));
    assert_eq!(opened.b.avg_price, dec!(99.90));
}

#[tokio::test]
async fn a_rejected_second_leg_flattens_the_first_and_opens_nothing() {
    // The Python's worst bug, asserted against directly: leg A fills, leg B is
    // rejected, and the account must end flat rather than holding naked risk.
    let ex = FaultExchange::new();
    ex.script(
        "AAVEUSDT",
        vec![
            LegAction::Fills { price: dec!(100.10) },
            // The unwind order.
            LegAction::Fills { price: dec!(99.80) },
        ],
    );
    ex.script("ETHUSDT", vec![LegAction::Rejected]);

    let got = open_pair(
        &ex,
        &cfg(),
        &params(),
        (&quote("AAVEUSDT"), &quote("ETHUSDT")),
        PairSide::LongSpread,
        dec!(1000),
        1_700_000_000_000,
    )
    .await
    .expect("the unwind succeeded, so this is not an error");

    assert!(got.is_none(), "no position may be reported");
    assert_eq!(
        ex.net_exposure("AAVEUSDT"),
        dec!(0),
        "leg A must have been flattened"
    );
    let placed = ex.placed.lock().unwrap();
    let unwind = placed.last().expect("an unwind order was placed");
    assert!(unwind.reduce_only, "an unwind must be reduce-only");
}

#[tokio::test]
async fn a_placement_that_times_out_after_the_exchange_filled_it_is_discovered() {
    // `place` returns Err while the order exists and is filled. Trusting the
    // Err and moving on would leave a naked leg with nothing recording it.
    let ex = FaultExchange::new();
    ex.script(
        "AAVEUSDT",
        vec![
            LegAction::TimesOutButFills { price: dec!(100.10) },
            LegAction::Fills { price: dec!(99.80) },
        ],
    );
    ex.script("ETHUSDT", vec![LegAction::Rejected]);

    let got = open_pair(
        &ex,
        &cfg(),
        &params(),
        (&quote("AAVEUSDT"), &quote("ETHUSDT")),
        PairSide::LongSpread,
        dec!(1000),
        1_700_000_000_000,
    )
    .await
    .expect("the unwind succeeded");

    assert!(got.is_none());
    assert_eq!(ex.net_exposure("AAVEUSDT"), dec!(0));
}

#[tokio::test]
async fn a_lopsided_fill_unwinds_both_legs() {
    let ex = FaultExchange::new();
    ex.script(
        "AAVEUSDT",
        vec![
            LegAction::Fills { price: dec!(100.10) },
            LegAction::Fills { price: dec!(99.80) },
        ],
    );
    ex.script(
        "ETHUSDT",
        vec![
            LegAction::PartiallyFills { qty: dec!(0.5), price: dec!(99.90) },
            LegAction::Fills { price: dec!(100.20) },
        ],
    );

    let got = open_pair(
        &ex,
        &cfg(),
        &params(),
        (&quote("AAVEUSDT"), &quote("ETHUSDT")),
        PairSide::LongSpread,
        dec!(1000),
        1_700_000_000_000,
    )
    .await
    .expect("the unwind succeeded");

    assert!(got.is_none());
    assert_eq!(ex.net_exposure("AAVEUSDT"), dec!(0));
    assert_eq!(ex.net_exposure("ETHUSDT"), dec!(0));
}

#[tokio::test]
async fn neither_leg_filling_leaves_the_account_flat_with_no_unwind() {
    let ex = FaultExchange::new();
    ex.script("AAVEUSDT", vec![LegAction::Rejected]);
    ex.script("ETHUSDT", vec![LegAction::Rejected]);

    let got = open_pair(
        &ex,
        &cfg(),
        &params(),
        (&quote("AAVEUSDT"), &quote("ETHUSDT")),
        PairSide::ShortSpread,
        dec!(1000),
        1_700_000_000_000,
    )
    .await
    .expect("nothing to unwind is not an error");

    assert!(got.is_none());
    assert!(
        ex.placed.lock().unwrap().iter().all(|o| !o.reduce_only),
        "no unwind order should have been placed"
    );
}

#[tokio::test]
async fn an_unwind_escalates_further_through_the_book_on_each_attempt() {
    let ex = FaultExchange::new();
    ex.quote("AAVEUSDT", dec!(99.90), dec!(100.10));
    ex.script(
        "AAVEUSDT",
        vec![
            LegAction::Fills { price: dec!(100.10) },
            LegAction::Rests, // first unwind attempt does not fill
            LegAction::Rests, // second does not either
            LegAction::Fills { price: dec!(99.30) }, // third gets there
        ],
    );
    ex.script("ETHUSDT", vec![LegAction::Rejected]);

    open_pair(
        &ex,
        &cfg(),
        &params(),
        (&quote("AAVEUSDT"), &quote("ETHUSDT")),
        PairSide::LongSpread,
        dec!(1000),
        1_700_000_000_000,
    )
    .await
    .expect("the third rung filled");

    let placed = ex.placed.lock().unwrap();
    let unwinds: Vec<_> = placed.iter().filter(|o| o.reduce_only).collect();
    assert_eq!(unwinds.len(), 3);
    // Selling out of a long: each rung is priced further below the bid.
    assert!(
        unwinds[0].price > unwinds[1].price && unwinds[1].price > unwinds[2].price,
        "prices were {:?}",
        unwinds.iter().map(|o| o.price).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn an_exhausted_unwind_ladder_reports_the_stranded_exposure_and_never_sends_a_market_order() {
    let ex = FaultExchange::new();
    ex.script(
        "AAVEUSDT",
        vec![
            LegAction::Fills { price: dec!(100.10) },
            LegAction::Rests,
            LegAction::Rests,
            LegAction::Rests,
        ],
    );
    ex.script("ETHUSDT", vec![LegAction::Rejected]);

    let err = open_pair(
        &ex,
        &cfg(),
        &params(),
        (&quote("AAVEUSDT"), &quote("ETHUSDT")),
        PairSide::LongSpread,
        dec!(1000),
        1_700_000_000_000,
    )
    .await
    .expect_err("stranded exposure must be an error, not a silent success");

    match err {
        ExecutionError::UnwindExhausted { legs } => {
            assert_eq!(legs.len(), 1);
            assert_eq!(legs[0].symbol.as_str(), "AAVEUSDT");
        }
        other => panic!("expected UnwindExhausted, got {other:?}"),
    }
    // The standing rule: no market-order fallback exists, so there is nothing
    // in `placed` that is not a limit. `LimitLeg` makes this structural, and
    // the assertion documents it.
    assert_eq!(ex.placed.lock().unwrap().len(), 5);
}

#[tokio::test]
async fn order_link_ids_are_derived_from_the_bar_so_a_retry_cannot_double_up() {
    // The Python used int(time.time()), so a retry inside the same bar minted
    // a fresh id and Bybit had no way to deduplicate it.
    let ex = FaultExchange::new();
    ex.script("AAVEUSDT", vec![LegAction::Fills { price: dec!(100.10) }]);
    ex.script("ETHUSDT", vec![LegAction::Fills { price: dec!(99.90) }]);
    open_pair(
        &ex,
        &cfg(),
        &params(),
        (&quote("AAVEUSDT"), &quote("ETHUSDT")),
        PairSide::LongSpread,
        dec!(1000),
        1_700_000_000_000,
    )
    .await
    .unwrap();

    let placed = ex.placed.lock().unwrap();
    assert_eq!(placed[0].order_link_id, "aave_eth-a-1700000000000");
    assert_eq!(placed[1].order_link_id, "aave_eth-b-1700000000000");
    assert!(placed.iter().all(|o| o.order_link_id.len() <= 36));
}
