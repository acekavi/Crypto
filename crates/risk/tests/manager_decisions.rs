use botcore::{Instrument, Position, Side, Symbol};
use risk::{AccountState, Decision, Refusal, RiskManager, RiskParams};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use strategy::Signal;

fn instrument() -> Instrument {
    Instrument {
        symbol: Symbol::new("BTCUSDT"),
        tick_size: dec!(0.1),
        qty_step: dec!(0.001),
        min_order_qty: dec!(0.001),
        min_notional: dec!(5),
        launch_time_ms: 0,
    }
}

fn long_signal() -> Signal {
    Signal {
        symbol: Symbol::new("BTCUSDT"),
        side: Side::Buy,
        entry_price: dec!(100),
        stop_price: dec!(95),
        target_price: dec!(110),
        atr: dec!(2),
        signal_candle_open_ms: 1_700_000_000_000,
        breakeven_at_r: None,
    }
}

fn healthy() -> AccountState {
    AccountState {
        equity: dec!(10000),
        available: dec!(9000),
        open_positions: vec![],
        day_start_equity: dec!(10000),
        high_water_mark: dec!(10000),
        entries_filled_today: 0,
        halt_reason: None,
    }
}

fn manager() -> RiskManager {
    RiskManager::new(RiskParams::defaults(), dec!(0.3))
}

#[test]
fn a_clean_signal_becomes_a_sized_intent() {
    let d = manager().evaluate(&long_signal(), &healthy(), &instrument(), None);
    let Decision::Enter(intent) = d else {
        panic!("expected Enter, got {d:?}");
    };
    // Risk is measured entry -> STOP-LIMIT, not entry -> trigger, because the
    // stop-limit is where a stopped trade actually fills.
    //   trigger    95.0
    //   stop-limit 94.4   (95 - 0.3 x ATR(2))
    //   risk/unit   5.6
    //   1% of 10,000 = 100 risked  =>  100 / 5.6 = 17.857 units
    //   and 17.857 x 5.6 = 100 exactly, which is the point.
    //
    // Sizing on the 5.0 trigger distance instead gave 20 units, and a fill at
    // 94.4 then lost 20 x 5.6 = 112 — 12% more than the rules allow. Across
    // 1486 backtested trades that turned a nominal 1:2 into a realised 1.60
    // and moved breakeven from 33.3% to 38.4%.
    assert_eq!(intent.qty, dec!(17.857));
    assert_eq!(intent.entry_price, dec!(100));
    assert_eq!(intent.stop_price, dec!(95));
    // Target is 2R on the SAME risk distance, so the reward really is twice
    // the loss: 100 + 2 x 5.6 = 111.2.
    assert_eq!(intent.target_price, dec!(111.2));
    assert_eq!(intent.side, Side::Buy);
    assert_eq!(intent.signal_candle_open_ms, 1_700_000_000_000);
}

#[test]
fn the_stop_limit_sits_beyond_the_trigger_for_a_long() {
    // Offset 0.3 x ATR(2) = 0.6, placed BELOW the stop on a long so the order
    // fills into the move rather than at its edge.
    let d = manager().evaluate(&long_signal(), &healthy(), &instrument(), None);
    let Decision::Enter(intent) = d else {
        panic!("expected Enter");
    };
    assert_eq!(intent.stop_limit_price, dec!(94.4));
}

#[test]
fn the_stop_limit_sits_beyond_the_trigger_for_a_short() {
    let mut sig = long_signal();
    sig.side = Side::Sell;
    sig.stop_price = dec!(105);
    sig.target_price = dec!(90);

    let d = manager().evaluate(&sig, &healthy(), &instrument(), None);
    let Decision::Enter(intent) = d else {
        panic!("expected Enter");
    };
    // On a short the stop lies above, so the limit is placed above it.
    assert_eq!(intent.stop_limit_price, dec!(105.6));
}

#[test]
fn a_halted_account_refuses_before_sizing() {
    let mut s = healthy();
    s.halt_reason = Some("daily drawdown".into());
    let d = manager().evaluate(&long_signal(), &s, &instrument(), None);
    assert!(matches!(d, Decision::Refuse(Refusal::Halted { .. })));
}

#[test]
fn a_drawdown_breach_refuses_even_when_no_halt_is_persisted_yet() {
    // The halt flag is written by the engine after this fires; the manager
    // must refuse on the measurement itself, not wait for the flag.
    let mut s = healthy();
    s.equity = dec!(9000); // −10% on the day
    let d = manager().evaluate(&long_signal(), &s, &instrument(), None);
    assert!(matches!(d, Decision::Refuse(Refusal::DailyDrawdown { .. })));
}

#[test]
fn liquidation_inside_the_buffer_refuses_rather_than_resizing() {
    // Stop distance 5, buffer 3 => liquidation must be at or below 85.
    let d = manager().evaluate(&long_signal(), &healthy(), &instrument(), Some(dec!(90)));
    assert!(matches!(
        d,
        Decision::Refuse(Refusal::LiquidationTooClose { .. })
    ));
}

#[test]
fn zero_equity_refuses_with_size_too_small() {
    // An unfunded account must produce a named refusal, not a zero-size order.
    let mut s = healthy();
    s.equity = dec!(0);
    s.day_start_equity = dec!(0);
    s.high_water_mark = dec!(0);
    let d = manager().evaluate(&long_signal(), &s, &instrument(), None);
    assert!(matches!(d, Decision::Refuse(Refusal::SizeTooSmall)));
}

#[test]
fn a_size_below_the_instrument_minimum_refuses() {
    let mut inst = instrument();
    inst.min_order_qty = dec!(1000);
    let d = manager().evaluate(&long_signal(), &healthy(), &inst, None);
    assert!(matches!(
        d,
        Decision::Refuse(Refusal::BelowMinimumQty { .. })
    ));
}

#[test]
fn notional_exceeding_available_margin_refuses() {
    // 20 units at 100 = 2,000 notional against only 500 available.
    let mut s = healthy();
    s.available = dec!(500);
    let d = manager().evaluate(&long_signal(), &s, &instrument(), None);
    assert!(matches!(
        d,
        Decision::Refuse(Refusal::InsufficientMargin { .. })
    ));
}

#[test]
fn a_second_position_in_the_same_symbol_refuses() {
    let mut s = healthy();
    s.open_positions = vec![Position {
        symbol: Symbol::new("BTCUSDT"),
        side: Side::Buy,
        size: dec!(1),
        entry_price: dec!(100),
        liq_price: None,
        unrealized_pnl: dec!(0),
    }];
    let d = manager().evaluate(&long_signal(), &s, &instrument(), None);
    assert!(matches!(
        d,
        Decision::Refuse(Refusal::AlreadyInSymbol { .. })
    ));
}

#[test]
fn entry_and_stop_prices_are_rounded_to_the_instruments_tick() {
    let mut sig = long_signal();
    sig.entry_price = dec!(100.567);
    sig.stop_price = dec!(95.123);
    let d = manager().evaluate(&sig, &healthy(), &instrument(), None);
    let Decision::Enter(intent) = d else {
        panic!("expected Enter");
    };
    // tick 0.1; a buy limit rounds DOWN, away from the market.
    assert_eq!(intent.entry_price, dec!(100.5));
    // Every transmitted price must land on a tick or Bybit rejects the order.
    let ticks = intent.stop_price / dec!(0.1);
    assert_eq!(
        ticks.fract(),
        Decimal::ZERO,
        "stop {} is off-tick",
        intent.stop_price
    );
}

#[test]
fn a_stop_limit_price_falling_through_zero_refuses_rather_than_panicking() {
    // A cheap instrument whose ATR is huge relative to its price: entry 1.0,
    // stop 0.5, ATR 3.0 -> offset 0.9, so stop - offset = -0.4.
    let mut sig = long_signal();
    sig.entry_price = dec!(1.0);
    sig.stop_price = dec!(0.5);
    sig.target_price = dec!(2.0);
    sig.atr = dec!(3);

    let mut inst = instrument();
    inst.tick_size = dec!(0.0001);
    inst.qty_step = dec!(0.001);
    inst.min_order_qty = dec!(0.001);

    let d = manager().evaluate(&sig, &healthy(), &inst, None);
    assert!(
        matches!(d, Decision::Refuse(Refusal::NonPositiveStopLimit { .. })),
        "expected a refusal, got {d:?}"
    );
}

#[test]
fn a_short_target_price_falling_through_zero_refuses_rather_than_panicking() {
    // Short entry 1.0, stop 1.5 (distance 0.5); a signal target of -0.5 implies
    // a 3x reward multiple, so target = entry - reward = 1.0 - 1.5 = -0.5.
    let mut sig = long_signal();
    sig.side = Side::Sell;
    sig.entry_price = dec!(1.0);
    sig.stop_price = dec!(1.5);
    sig.target_price = dec!(-0.5);
    sig.atr = dec!(2);

    let mut inst = instrument();
    inst.tick_size = dec!(0.0001);
    inst.qty_step = dec!(0.001);
    inst.min_order_qty = dec!(0.001);

    let d = manager().evaluate(&sig, &healthy(), &inst, None);
    assert!(
        matches!(d, Decision::Refuse(Refusal::NonPositiveTargetPrice { .. })),
        "expected a refusal, got {d:?}"
    );
}

#[test]
fn a_stopped_trade_loses_exactly_the_configured_risk_fraction() {
    // The property the sizing fix exists to restore, asserted directly rather
    // than inferred from a quantity: filling at the stop-limit must cost 1% of
    // equity, not 1% plus the stop-limit offset.
    let d = manager().evaluate(&long_signal(), &healthy(), &instrument(), None);
    let Decision::Enter(intent) = d else {
        panic!("expected Enter");
    };

    let realised_loss = intent.qty * (intent.entry_price - intent.stop_limit_price);
    let intended = healthy().equity * RiskParams::defaults().risk_pct;

    // Quantity rounds DOWN to the instrument's step, so the realised loss
    // lands just under the intended risk and never over it. That direction
    // matters: rounding up would breach the 1% rule on every trade.
    assert!(
        realised_loss <= intended,
        "a stop fill cost {realised_loss}, more than the intended {intended}"
    );
    // And it must be genuinely close, not merely under — one qty_step of
    // slack at most, or the sizing is wrong in a different way.
    let slack = instrument().qty_step * (intent.entry_price - intent.stop_limit_price);
    assert!(
        intended - realised_loss < slack,
        "realised {realised_loss} is more than one qty_step below the intended {intended}"
    );
}

#[test]
fn the_target_pays_twice_what_a_stop_fill_costs() {
    // "1:2" has to mean the realised reward is twice the realised loss. Before
    // the fix the target was 2x the TRIGGER distance while losses realised at
    // the wider stop-limit distance, so the true ratio was 1.67, not 2.
    let d = manager().evaluate(&long_signal(), &healthy(), &instrument(), None);
    let Decision::Enter(intent) = d else {
        panic!("expected Enter");
    };

    let loss = intent.entry_price - intent.stop_limit_price;
    let gain = intent.target_price - intent.entry_price;
    assert_eq!(gain, loss * dec!(2), "realised reward must be exactly 2R");
}
