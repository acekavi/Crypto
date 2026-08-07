use backtest::{CostModel, ExitReason, SimulatedExchange};
use botcore::{Candle, Instrument, LimitEntry, Side, Symbol};
use exchange::ExchangeClient;
use exchange::bybit::wire::FundingRate;
use rust_decimal_macros::dec;

fn instrument(sym: &Symbol) -> Instrument {
    Instrument {
        symbol: sym.clone(),
        tick_size: dec!(0.01),
        qty_step: dec!(0.01),
        min_order_qty: dec!(0.01),
        launch_time_ms: 0,
    }
}

/// A candle fixture matching `crates/backtest/tests/fills.rs`'s convention:
/// open/close fixed at 100 so a test's high/low are the only levers.
fn candle_at(open_time_ms: i64, high: rust_decimal::Decimal, low: rust_decimal::Decimal) -> Candle {
    Candle {
        open_time_ms,
        open: dec!(100),
        high,
        low,
        close: dec!(100),
        volume: dec!(1),
        turnover: dec!(1),
    }
}

fn oracle_entry(sym: &Symbol, link_id: &str) -> LimitEntry {
    LimitEntry {
        symbol: sym.clone(),
        side: Side::Buy,
        qty: dec!(50),
        price: dec!(100),
        order_link_id: link_id.into(),
        stop_loss: dec!(98),
        stop_limit_price: dec!(98),
        take_profit: dec!(104),
    }
}

#[tokio::test]
async fn place_limit_entry_leaves_the_order_resting_not_filled() {
    let sym = Symbol::new("BTCUSDT");
    let sim = SimulatedExchange::new(
        dec!(10000),
        vec![instrument(&sym)],
        CostModel {
            maker_fee_rate: dec!(0.0002),
        },
    );

    sim.place_limit_entry(oracle_entry(&sym, "resting-1"))
        .await
        .expect("placed");

    // No candle has arrived yet: place_limit_entry itself must never fill.
    assert!(sim.positions().await.expect("positions").is_empty());
}

#[tokio::test]
async fn a_resting_entry_traded_through_fills_and_charges_the_entry_fee() {
    let sym = Symbol::new("BTCUSDT");
    let sim = SimulatedExchange::new(
        dec!(10000),
        vec![instrument(&sym)],
        CostModel {
            maker_fee_rate: dec!(0.0002),
        },
    );

    sim.place_limit_entry(oracle_entry(&sym, "fill-1"))
        .await
        .expect("placed");

    let closed = sim.advance(&sym, &candle_at(0, dec!(101), dec!(99)), &[]);
    assert!(closed.is_empty(), "a fill is not itself a closed trade");

    let positions = sim.positions().await.expect("positions");
    assert_eq!(positions.len(), 1);
    assert_eq!(positions[0].entry_price, dec!(100));

    // 50 * 100 * 0.0002 = 1.00, charged immediately on fill.
    let bal = sim.balance().await.expect("balance");
    assert_eq!(bal.equity, dec!(10000) - dec!(1.00));
}

#[tokio::test]
async fn an_entry_only_touched_does_not_fill() {
    let sym = Symbol::new("BTCUSDT");
    let sim = SimulatedExchange::new(
        dec!(10000),
        vec![instrument(&sym)],
        CostModel {
            maker_fee_rate: dec!(0.0002),
        },
    );

    sim.place_limit_entry(oracle_entry(&sym, "touch-1"))
        .await
        .expect("placed");

    // Low reaches exactly the limit price (100) and no further: a touch, not
    // a trade-through. Trade-through applies to entries as much as exits.
    let closed = sim.advance(&sym, &candle_at(0, dec!(105), dec!(100)), &[]);
    assert!(closed.is_empty());
    assert!(sim.positions().await.expect("positions").is_empty());
}

#[tokio::test]
async fn a_long_position_stopped_out_closes_at_the_stop_limit_price() {
    let sym = Symbol::new("BTCUSDT");
    let sim = SimulatedExchange::new(
        dec!(10000),
        vec![instrument(&sym)],
        CostModel {
            maker_fee_rate: dec!(0.0002),
        },
    );

    sim.place_limit_entry(oracle_entry(&sym, "stop-1"))
        .await
        .expect("placed");
    sim.advance(&sym, &candle_at(0, dec!(101), dec!(99)), &[]);

    // Trades through the stop (98) but not the target (104).
    let closed = sim.advance(&sym, &candle_at(1, dec!(100), dec!(97)), &[]);

    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0].exit_reason, ExitReason::Stop);
    assert_eq!(closed[0].exit_price, dec!(98));
    assert!(!closed[0].was_ambiguous);
}

#[tokio::test]
async fn a_candle_reaching_both_stop_and_target_closes_as_stop_and_flags_ambiguous() {
    let sym = Symbol::new("BTCUSDT");
    let sim = SimulatedExchange::new(
        dec!(10000),
        vec![instrument(&sym)],
        CostModel {
            maker_fee_rate: dec!(0.0002),
        },
    );

    sim.place_limit_entry(oracle_entry(&sym, "ambiguous-1"))
        .await
        .expect("placed");
    sim.advance(&sym, &candle_at(0, dec!(101), dec!(99)), &[]);

    // Trades through both the stop (98) and the target (104) in one candle.
    let closed = sim.advance(&sym, &candle_at(1, dec!(105), dec!(97)), &[]);

    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0].exit_reason, ExitReason::Stop);
    assert!(closed[0].was_ambiguous);
}

#[tokio::test]
async fn equity_after_a_closed_trade_equals_starting_equity_plus_net_pnl() {
    let sym = Symbol::new("BTCUSDT");
    let sim = SimulatedExchange::new(
        dec!(10000),
        vec![instrument(&sym)],
        CostModel {
            maker_fee_rate: dec!(0.0002),
        },
    );

    sim.place_limit_entry(oracle_entry(&sym, "equity-1"))
        .await
        .expect("placed");
    sim.advance(&sym, &candle_at(0, dec!(101), dec!(99)), &[]);
    let closed = sim.advance(&sym, &candle_at(1, dec!(105), dec!(100.5)), &[]);

    assert_eq!(closed.len(), 1);
    let bal = sim.balance().await.expect("balance");
    assert_eq!(bal.equity, dec!(10000) + closed[0].net_pnl);
}

#[tokio::test]
async fn cancel_order_removes_a_resting_order_so_it_never_fills() {
    let sym = Symbol::new("BTCUSDT");
    let sim = SimulatedExchange::new(
        dec!(10000),
        vec![instrument(&sym)],
        CostModel {
            maker_fee_rate: dec!(0.0002),
        },
    );

    sim.place_limit_entry(oracle_entry(&sym, "cancel-1"))
        .await
        .expect("placed");
    sim.cancel_order(&sym, "cancel-1").await.expect("cancelled");

    let closed = sim.advance(&sym, &candle_at(0, dec!(101), dec!(99)), &[]);
    assert!(closed.is_empty());
    assert!(sim.positions().await.expect("positions").is_empty());
}

#[tokio::test]
async fn a_full_round_trip_reproduces_the_hand_computed_oracle() {
    // Same figures as crates/backtest/tests/costs.rs, but driven end to end
    // through place_limit_entry -> advance -> advance rather than by calling
    // the cost functions directly. If these disagree, the simulator and the
    // cost model have drifted apart and every backtest number is suspect.
    //   gross pnl = 50 * (104 - 100)      = 200
    //   entry fee = 50 * 100  * 0.0002    =   1.00
    //   exit  fee = 50 * 104  * 0.0002    =   1.04
    //   funding   = 50 * 100  * 0.0001    =   0.50  (long pays)
    //   net       = 200 - 1.00 - 1.04 - 0.50 = 197.46
    let sym = Symbol::new("BTCUSDT");
    let sim = SimulatedExchange::new(
        dec!(10000),
        vec![instrument(&sym)],
        CostModel {
            maker_fee_rate: dec!(0.0002),
        },
    );

    sim.place_limit_entry(LimitEntry {
        symbol: sym.clone(),
        side: Side::Buy,
        qty: dec!(50),
        price: dec!(100),
        order_link_id: "oracle-1".into(),
        stop_loss: dec!(98),
        stop_limit_price: dec!(98),
        take_profit: dec!(104),
    })
    .await
    .expect("placed");

    // Candle 1 trades through 100, filling the entry. It must NOT also
    // resolve an exit on the same bar.
    let entry_candle = candle_at(0, dec!(101), dec!(99));
    let closed = sim.advance(&sym, &entry_candle, &[]);
    assert!(closed.is_empty(), "an entry must not close on its own bar");
    assert_eq!(sim.positions().await.expect("positions").len(), 1);

    // Candle 2 trades through the target, with one funding timestamp
    // strictly inside the hold.
    let funding = vec![FundingRate {
        symbol: sym.clone(),
        funding_time_ms: 1,
        rate: dec!(0.0001),
    }];
    let exit_candle = candle_at(2, dec!(105), dec!(100.5));
    let closed = sim.advance(&sym, &exit_candle, &funding);

    assert_eq!(closed.len(), 1);
    let t = &closed[0];
    assert_eq!(t.exit_reason, ExitReason::Target);
    assert_eq!(t.gross_pnl, dec!(200));
    assert_eq!(t.fees, dec!(2.04), "1.00 entry + 1.04 exit");
    assert_eq!(t.funding, dec!(0.5), "positive: the long paid");
    assert_eq!(t.net_pnl, dec!(197.46));
    assert!(!t.was_ambiguous);

    let bal = sim.balance().await.expect("balance");
    assert_eq!(bal.equity, dec!(10197.46), "starting equity plus net pnl");
}

/// A candle whose close differs from the default 100, so a funding period
/// charged at this candle is visibly distinguishable from one charged at
/// another.
fn candle_closing_at(
    open_time_ms: i64,
    high: rust_decimal::Decimal,
    low: rust_decimal::Decimal,
    close: rust_decimal::Decimal,
) -> Candle {
    Candle {
        open_time_ms,
        open: dec!(100),
        high,
        low,
        close,
        volume: dec!(1),
        turnover: dec!(1),
    }
}

fn funding_at(sym: &Symbol, at_ms: i64, rate: rust_decimal::Decimal) -> FundingRate {
    FundingRate {
        symbol: sym.clone(),
        funding_time_ms: at_ms,
        rate,
    }
}

#[tokio::test]
async fn funding_is_charged_at_each_candles_own_close_not_all_at_the_exit_price() {
    // The bias this guards against is SYSTEMATIC, not random: pricing a whole
    // hold at the exit mark means a winning long has every funding period
    // marked up, and the error grows with hold time — exactly the regime a
    // swing strategy operates in.
    //
    // Long 50 units. Three funding periods at 0.0001, each falling in a
    // candle with a different close:
    //   period 1 @ close 100 -> 50 * 100 * 0.0001 = 0.50
    //   period 2 @ close 120 -> 50 * 120 * 0.0001 = 0.60
    //   period 3 @ close 140 -> 50 * 140 * 0.0001 = 0.70
    //   total                                     = 1.80
    // Charging all three at the exit close (140) would give 2.10 — the
    // number this test exists to reject.
    let sym = Symbol::new("BTCUSDT");
    let sim = SimulatedExchange::new(
        dec!(10000),
        vec![instrument(&sym)],
        CostModel {
            maker_fee_rate: dec!(0.0002),
        },
    );

    sim.place_limit_entry(oracle_entry(&sym, "accrual-1"))
        .await
        .expect("placed");
    // Entry fills here; nothing is charged for the entry candle itself.
    sim.advance(&sym, &candle_at(0, dec!(101), dec!(99)), &[]);

    let rates = vec![
        funding_at(&sym, 10, dec!(0.0001)),
        funding_at(&sym, 20, dec!(0.0001)),
        funding_at(&sym, 30, dec!(0.0001)),
    ];

    // Each candle settles the one period that fell due since the last one.
    sim.advance(
        &sym,
        &candle_closing_at(10, dec!(101), dec!(99), dec!(100)),
        &rates,
    );
    sim.advance(
        &sym,
        &candle_closing_at(20, dec!(101), dec!(99), dec!(120)),
        &rates,
    );
    // The third candle also trades through the target, closing the position.
    let closed = sim.advance(
        &sym,
        &candle_closing_at(30, dec!(105), dec!(100.5), dec!(140)),
        &rates,
    );

    assert_eq!(closed.len(), 1);
    assert_eq!(
        closed[0].funding,
        dec!(1.80),
        "each period must be marked at its own candle's close, not all at the exit price (2.10)"
    );
}

#[tokio::test]
async fn a_funding_timestamp_on_a_candle_boundary_is_charged_exactly_once() {
    // The spans are half-open — (last_funded_ms, open_time_ms] — so a
    // timestamp landing exactly on a boundary belongs to one candle and one
    // only. Dropping it understates costs; charging it twice overstates them.
    let sym = Symbol::new("BTCUSDT");
    let sim = SimulatedExchange::new(
        dec!(10000),
        vec![instrument(&sym)],
        CostModel {
            maker_fee_rate: dec!(0.0002),
        },
    );

    sim.place_limit_entry(oracle_entry(&sym, "boundary-1"))
        .await
        .expect("placed");
    sim.advance(&sym, &candle_at(0, dec!(101), dec!(99)), &[]);

    // Exactly on the second candle's open time.
    let rates = vec![funding_at(&sym, 10, dec!(0.0001))];

    sim.advance(
        &sym,
        &candle_closing_at(10, dec!(101), dec!(99), dec!(100)),
        &rates,
    );
    // Passing the same rate slice again must not charge it a second time.
    let closed = sim.advance(
        &sym,
        &candle_closing_at(20, dec!(105), dec!(100.5), dec!(100)),
        &rates,
    );

    assert_eq!(closed.len(), 1);
    assert_eq!(
        closed[0].funding,
        dec!(0.5),
        "one period at mark 100 = 0.5; charged twice would be 1.0, dropped would be 0"
    );
}

#[tokio::test]
async fn a_position_is_not_charged_for_funding_that_predates_its_entry() {
    // The rate fell due before this position existed. Charging it would
    // attribute another period's cost to this trade.
    let sym = Symbol::new("BTCUSDT");
    let sim = SimulatedExchange::new(
        dec!(10000),
        vec![instrument(&sym)],
        CostModel {
            maker_fee_rate: dec!(0.0002),
        },
    );

    sim.place_limit_entry(oracle_entry(&sym, "predate-1"))
        .await
        .expect("placed");

    // Funding at 5 and at the entry candle's own open time (10); neither may
    // be charged to a position that opens at 10.
    let rates = vec![
        funding_at(&sym, 5, dec!(0.0001)),
        funding_at(&sym, 10, dec!(0.0001)),
    ];

    sim.advance(
        &sym,
        &candle_closing_at(10, dec!(101), dec!(99), dec!(100)),
        &rates,
    );
    let closed = sim.advance(
        &sym,
        &candle_closing_at(20, dec!(105), dec!(100.5), dec!(100)),
        &rates,
    );

    assert_eq!(closed.len(), 1);
    assert_eq!(
        closed[0].funding,
        dec!(0),
        "both timestamps precede or coincide with entry"
    );
}
