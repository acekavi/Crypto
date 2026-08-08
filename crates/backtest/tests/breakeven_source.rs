//! The breakeven threshold belongs to the trade, not to the simulator.
//!
//! It reaches `SimulatedExchange` on the `LimitEntry` the executor built from
//! the strategy's `Signal`, so the backtester and the live engine read one
//! value from one place. A simulator-wide setting would be a second,
//! independent copy of the rule — the class of drift that made `warm()` feed
//! the CandleStore but not the strategy's indicators.
//!
//! Assertions are made on exit prices rather than on a peeked-at stop level:
//! where the stop actually sits is only observable in what a later candle
//! settles at, which is also the only thing that changes a backtest's numbers.

use backtest::{CostModel, SimulatedExchange};
use botcore::{Candle, Instrument, LimitEntry, Side, Symbol};
use exchange::ExchangeClient;
use rust_decimal::Decimal;
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

fn sim(syms: &[&Symbol]) -> SimulatedExchange {
    SimulatedExchange::new(
        dec!(100000),
        syms.iter().map(|s| instrument(s)).collect(),
        CostModel {
            maker_fee_rate: dec!(0.0002),
        },
    )
}

/// Entry 100 with its stop-limit 10 away, so 1R is exactly 10 and a 2R
/// breakeven arms at 120 for a long. The target sits far enough out that no
/// test candle reaches it.
fn entry(sym: &Symbol, link_id: &str, side: Side, breakeven_at_r: Option<Decimal>) -> LimitEntry {
    let (stop, target) = match side {
        Side::Buy => (dec!(90), dec!(500)),
        Side::Sell => (dec!(110), dec!(1)),
    };
    LimitEntry {
        symbol: sym.clone(),
        side,
        qty: dec!(1),
        price: dec!(100),
        order_link_id: link_id.into(),
        stop_loss: stop,
        stop_limit_price: stop,
        take_profit: target,
        breakeven_at_r,
    }
}

fn candle(open_time_ms: i64, high: Decimal, low: Decimal) -> Candle {
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

#[tokio::test]
async fn a_signal_carrying_two_r_does_not_arm_one_tick_short_of_it() {
    let sym = Symbol::new("BTCUSDT");
    let sim = sim(&[&sym]);
    sim.place_limit_entry(entry(&sym, "be-short-of-2r", Side::Buy, Some(dec!(2))))
        .await
        .expect("placed");

    sim.advance(&sym, &candle(0, dec!(101), dec!(99)), &[]);
    // 119 against a 1R of 10: one short of the 120 the signal asked for.
    sim.advance(&sym, &candle(1, dec!(119), dec!(100)), &[]);
    let closed = sim.advance(&sym, &candle(2, dec!(101), dec!(89)), &[]);

    assert_eq!(closed.len(), 1);
    assert_eq!(
        closed[0].exit_price,
        dec!(90),
        "the stop must still be where the entry placed it"
    );
}

#[tokio::test]
async fn a_signal_carrying_two_r_arms_at_exactly_two_r() {
    let sym = Symbol::new("BTCUSDT");
    let sim = sim(&[&sym]);
    sim.place_limit_entry(entry(&sym, "be-at-2r", Side::Buy, Some(dec!(2))))
        .await
        .expect("placed");

    sim.advance(&sym, &candle(0, dec!(101), dec!(99)), &[]);
    sim.advance(&sym, &candle(1, dec!(120), dec!(100)), &[]);
    let closed = sim.advance(&sym, &candle(2, dec!(101), dec!(89)), &[]);

    assert_eq!(closed.len(), 1);
    assert_eq!(
        closed[0].exit_price,
        dec!(100),
        "the stop moves to entry exactly, not entry plus a tick"
    );
}

#[tokio::test]
async fn a_signal_without_a_breakeven_never_moves_its_stop() {
    let sym = Symbol::new("BTCUSDT");
    let sim = sim(&[&sym]);
    sim.place_limit_entry(entry(&sym, "be-none", Side::Buy, None))
        .await
        .expect("placed");

    sim.advance(&sym, &candle(0, dec!(101), dec!(99)), &[]);
    // 20R in favour. A simulator-wide default would arm here; nothing must.
    sim.advance(&sym, &candle(1, dec!(300), dec!(100)), &[]);
    let closed = sim.advance(&sym, &candle(2, dec!(101), dec!(89)), &[]);

    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0].exit_price, dec!(90));
}

#[tokio::test]
async fn two_positions_in_one_simulator_use_their_own_thresholds() {
    // The point of the refactor: one simulator settling two trades that
    // disagree about breakeven. Nothing simulator-wide can express this.
    let with_be = Symbol::new("BTCUSDT");
    let without = Symbol::new("ETHUSDT");
    let sim = sim(&[&with_be, &without]);
    sim.place_limit_entry(entry(&with_be, "be-2r", Side::Buy, Some(dec!(2))))
        .await
        .expect("placed");
    sim.place_limit_entry(entry(&without, "be-none", Side::Buy, None))
        .await
        .expect("placed");

    for c in [
        candle(0, dec!(101), dec!(99)),
        candle(1, dec!(130), dec!(100)),
    ] {
        sim.advance(&with_be, &c, &[]);
        sim.advance(&without, &c, &[]);
    }

    let retrace = candle(2, dec!(101), dec!(89));
    let armed = sim.advance(&with_be, &retrace, &[]);
    let unarmed = sim.advance(&without, &retrace, &[]);

    assert_eq!(armed.len(), 1);
    assert_eq!(unarmed.len(), 1);
    assert_eq!(armed[0].exit_price, dec!(100));
    assert_eq!(unarmed[0].exit_price, dec!(90));
}

#[tokio::test]
async fn a_short_arms_on_its_own_threshold_too() {
    let sym = Symbol::new("BTCUSDT");
    let sim = sim(&[&sym]);
    sim.place_limit_entry(entry(&sym, "be-short", Side::Sell, Some(dec!(2))))
        .await
        .expect("placed");

    sim.advance(&sym, &candle(0, dec!(101), dec!(99)), &[]);
    sim.advance(&sym, &candle(1, dec!(100), dec!(80)), &[]);
    let closed = sim.advance(&sym, &candle(2, dec!(111), dec!(99)), &[]);

    assert_eq!(closed.len(), 1);
    assert_eq!(closed[0].exit_price, dec!(100), "a short exits at entry");
}

#[tokio::test]
async fn a_candle_that_reaches_the_threshold_and_the_stop_resolves_as_the_stop() {
    // The ordering the refactor must not disturb: breakeven is applied only
    // AFTER the exit is resolved for the candle, so a trade already stopped
    // out on this bar cannot be retroactively rescued into a scratch.
    let sym = Symbol::new("BTCUSDT");
    let sim = sim(&[&sym]);
    sim.place_limit_entry(entry(&sym, "be-ordering", Side::Buy, Some(dec!(2))))
        .await
        .expect("placed");

    sim.advance(&sym, &candle(0, dec!(101), dec!(99)), &[]);
    let closed = sim.advance(&sym, &candle(1, dec!(125), dec!(89)), &[]);

    assert_eq!(closed.len(), 1);
    assert_eq!(
        closed[0].exit_price,
        dec!(90),
        "same-candle 2R and stop must resolve as the stop"
    );
}
