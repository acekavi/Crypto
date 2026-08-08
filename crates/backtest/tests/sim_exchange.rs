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
        breakeven_at_r: None,
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
        breakeven_at_r: None,
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

#[tokio::test]
async fn with_several_resting_orders_the_one_price_reaches_first_fills() {
    // THE BUG THIS GUARDS. `resting` is a HashMap, and picking a fill by
    // iterating it made the choice depend on hash order. Every existing
    // determinism test used a strategy that rests ONE order per symbol, so
    // none of them could see it — the same backtest produced 255 trades on one
    // run and 257 on the next, and the gate's comparisons all assume
    // reproducibility.
    //
    // Three buy limits at 100, 98 and 96. Price trades down through all three,
    // so all are fillable — but price reaches 100 first, so that is the one
    // that fills.
    let sym = Symbol::new("BTCUSDT");
    let sim = SimulatedExchange::new(
        dec!(100000),
        vec![instrument(&sym)],
        CostModel {
            maker_fee_rate: dec!(0.0002),
        },
    );

    for (link, price) in [("low", dec!(96)), ("high", dec!(100)), ("mid", dec!(98))] {
        sim.place_limit_entry(LimitEntry {
            symbol: sym.clone(),
            side: Side::Buy,
            qty: dec!(1),
            price,
            order_link_id: link.into(),
            stop_loss: price - dec!(5),
            stop_limit_price: price - dec!(5),
            take_profit: price + dec!(10),
            breakeven_at_r: None,
        })
        .await
        .expect("placed");
    }

    // Low enough to trade through every one of them.
    sim.advance(&sym, &candle_at(0, dec!(101), dec!(90)), &[]);

    let positions = sim.positions().await.expect("positions");
    assert_eq!(positions.len(), 1, "one-way mode holds one position");
    assert_eq!(
        positions[0].entry_price,
        dec!(100),
        "price reaches the highest buy limit first, so that one fills"
    );
}

#[tokio::test]
async fn a_sell_side_pick_is_the_lowest_limit_price_reaches_first() {
    let sym = Symbol::new("BTCUSDT");
    let sim = SimulatedExchange::new(
        dec!(100000),
        vec![instrument(&sym)],
        CostModel {
            maker_fee_rate: dec!(0.0002),
        },
    );

    for (link, price) in [("high", dec!(104)), ("low", dec!(100)), ("mid", dec!(102))] {
        sim.place_limit_entry(LimitEntry {
            symbol: sym.clone(),
            side: Side::Sell,
            qty: dec!(1),
            price,
            order_link_id: link.into(),
            stop_loss: price + dec!(5),
            stop_limit_price: price + dec!(5),
            take_profit: price - dec!(10),
            breakeven_at_r: None,
        })
        .await
        .expect("placed");
    }

    sim.advance(&sym, &candle_at(0, dec!(110), dec!(99)), &[]);

    let positions = sim.positions().await.expect("positions");
    assert_eq!(positions.len(), 1);
    assert_eq!(
        positions[0].entry_price,
        dec!(100),
        "price rises through the lowest sell limit first"
    );
}

fn entry_at(link: &str, side: Side, price: rust_decimal::Decimal) -> LimitEntry {
    LimitEntry {
        symbol: Symbol::new("BTCUSDT"),
        side,
        qty: dec!(1),
        price,
        order_link_id: link.into(),
        stop_loss: price,
        stop_limit_price: price,
        take_profit: price,
        breakeven_at_r: None,
    }
}

#[test]
fn fill_priority_is_deterministic_on_a_plain_slice() {
    // Tested on a Vec, NOT through the exchange's HashMap. Driving this rule
    // through the map made the test probabilistic — under the original bug it
    // caught the defect only about two runs in three, because the buggy pick
    // was random rather than wrong. A pure function has no such excuse.
    let buys = [
        entry_at("a", Side::Buy, dec!(96)),
        entry_at("b", Side::Buy, dec!(100)),
        entry_at("c", Side::Buy, dec!(98)),
    ];

    // Every ordering of the same three orders must give the same answer.
    for perm in [
        [0, 1, 2],
        [2, 1, 0],
        [1, 0, 2],
        [0, 2, 1],
        [2, 0, 1],
        [1, 2, 0],
    ] {
        let mut v: Vec<&LimitEntry> = perm.iter().map(|i| &buys[*i]).collect();
        let picked = backtest::best_fillable(&mut v).expect("a candidate");
        assert_eq!(
            picked.price,
            dec!(100),
            "price reaches the highest buy limit first, whatever order they arrive in"
        );
    }

    let sells = [
        entry_at("a", Side::Sell, dec!(104)),
        entry_at("b", Side::Sell, dec!(100)),
        entry_at("c", Side::Sell, dec!(102)),
    ];
    for perm in [[0, 1, 2], [2, 1, 0], [1, 0, 2]] {
        let mut v: Vec<&LimitEntry> = perm.iter().map(|i| &sells[*i]).collect();
        assert_eq!(
            backtest::best_fillable(&mut v).expect("a candidate").price,
            dec!(100),
            "price rises through the lowest sell limit first"
        );
    }
}

#[test]
fn equal_prices_break_the_tie_on_link_id() {
    // Two orders at the same price must still resolve identically every run,
    // or the map ordering leaks back in through the tie.
    let a = entry_at("aaa", Side::Buy, dec!(100));
    let b = entry_at("zzz", Side::Buy, dec!(100));
    for pair in [vec![&a, &b], vec![&b, &a]] {
        let mut v = pair;
        assert_eq!(
            backtest::best_fillable(&mut v)
                .expect("a candidate")
                .order_link_id,
            "aaa"
        );
    }
}

#[test]
fn no_candidates_means_no_fill() {
    let mut v: Vec<&LimitEntry> = Vec::new();
    assert!(backtest::best_fillable(&mut v).is_none());
}

/// The simulator holds no breakeven setting of its own — the entries placed
/// below carry `breakeven_at_r: Some(dec!(1))`, so each trade arms at 1R
/// because its own signal said so.
fn sim_for_breakeven_tests(sym: &Symbol) -> SimulatedExchange {
    SimulatedExchange::new(
        dec!(100000),
        vec![instrument(sym)],
        CostModel {
            maker_fee_rate: dec!(0.0002),
        },
    )
}

#[tokio::test]
async fn reaching_one_r_pulls_the_stop_to_entry() {
    // Entry 100, stop-limit 95, so 1R is 5 and breakeven arms at 105.
    // A later candle that dips to 99 must now close the trade AT ENTRY
    // rather than running on to the old stop at 95.
    let sym = Symbol::new("BTCUSDT");
    let sim = sim_for_breakeven_tests(&sym);
    sim.place_limit_entry(LimitEntry {
        symbol: sym.clone(),
        side: Side::Buy,
        qty: dec!(1),
        price: dec!(100),
        order_link_id: "be-1".into(),
        stop_loss: dec!(95),
        stop_limit_price: dec!(95),
        take_profit: dec!(120),
        breakeven_at_r: Some(dec!(1)),
    })
    .await
    .expect("placed");

    // Fill.
    sim.advance(&sym, &candle_at(0, dec!(101), dec!(99)), &[]);
    assert_eq!(sim.positions().await.expect("p").len(), 1);

    // Travels to 106 without touching anything: arms breakeven.
    let closed = sim.advance(&sym, &candle_at(1, dec!(106), dec!(100.5)), &[]);
    assert!(closed.is_empty(), "1R is not an exit, only a stop move");

    // Retraces through entry. Old stop was 95; the trade must close at 100.
    let closed = sim.advance(&sym, &candle_at(2, dec!(101), dec!(96)), &[]);
    assert_eq!(closed.len(), 1);
    assert_eq!(
        closed[0].exit_price,
        dec!(100),
        "must exit at entry, not at the original stop"
    );
    assert_eq!(closed[0].gross_pnl, dec!(0), "breakeven means zero gross");
}

#[tokio::test]
async fn breakeven_does_not_arm_before_one_r() {
    // Travels only to 104 against a 1R of 5. The stop must stay at 95.
    let sym = Symbol::new("BTCUSDT");
    let sim = sim_for_breakeven_tests(&sym);
    sim.place_limit_entry(LimitEntry {
        symbol: sym.clone(),
        side: Side::Buy,
        qty: dec!(1),
        price: dec!(100),
        order_link_id: "be-2".into(),
        stop_loss: dec!(95),
        stop_limit_price: dec!(95),
        take_profit: dec!(120),
        breakeven_at_r: Some(dec!(1)),
    })
    .await
    .expect("placed");

    sim.advance(&sym, &candle_at(0, dec!(101), dec!(99)), &[]);
    sim.advance(&sym, &candle_at(1, dec!(104), dec!(100.5)), &[]);
    let closed = sim.advance(&sym, &candle_at(2, dec!(101), dec!(94)), &[]);

    assert_eq!(closed.len(), 1);
    assert_eq!(
        closed[0].exit_price,
        dec!(95),
        "stop must remain where it was placed"
    );
}

#[tokio::test]
async fn a_candle_that_reaches_one_r_and_the_stop_resolves_as_a_full_loss() {
    // THE ORDERING THAT MATTERS. This candle both reaches 1R (high 106) and
    // trades through the original stop (low 94). OHLC cannot say which came
    // first, so the pessimistic rule stands: it is a full loss at 95, not a
    // scratch. Applying breakeven before resolving the exit would silently
    // turn every such loss into a free trade.
    let sym = Symbol::new("BTCUSDT");
    let sim = sim_for_breakeven_tests(&sym);
    sim.place_limit_entry(LimitEntry {
        symbol: sym.clone(),
        side: Side::Buy,
        qty: dec!(1),
        price: dec!(100),
        order_link_id: "be-3".into(),
        stop_loss: dec!(95),
        stop_limit_price: dec!(95),
        take_profit: dec!(120),
        breakeven_at_r: Some(dec!(1)),
    })
    .await
    .expect("placed");

    sim.advance(&sym, &candle_at(0, dec!(101), dec!(99)), &[]);
    let closed = sim.advance(&sym, &candle_at(1, dec!(106), dec!(94)), &[]);

    assert_eq!(closed.len(), 1);
    assert_eq!(
        closed[0].exit_price,
        dec!(95),
        "same-candle 1R and stop must resolve as the stop"
    );
}

#[tokio::test]
async fn a_short_moves_to_breakeven_on_a_downward_move() {
    let sym = Symbol::new("BTCUSDT");
    let sim = sim_for_breakeven_tests(&sym);
    sim.place_limit_entry(LimitEntry {
        symbol: sym.clone(),
        side: Side::Sell,
        qty: dec!(1),
        price: dec!(100),
        order_link_id: "be-4".into(),
        stop_loss: dec!(105),
        stop_limit_price: dec!(105),
        take_profit: dec!(80),
        breakeven_at_r: Some(dec!(1)),
    })
    .await
    .expect("placed");

    sim.advance(&sym, &candle_at(0, dec!(101), dec!(99)), &[]);
    sim.advance(&sym, &candle_at(1, dec!(99.5), dec!(94)), &[]);
    let closed = sim.advance(&sym, &candle_at(2, dec!(104), dec!(99)), &[]);

    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0].exit_price, dec!(100), "short exits at entry");
}
