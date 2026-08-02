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
    // 1% of 10,000 = 100 risked, $5 stop distance => 20 units.
    assert_eq!(intent.qty, dec!(20));
    assert_eq!(intent.entry_price, dec!(100));
    assert_eq!(intent.stop_price, dec!(95));
    assert_eq!(intent.target_price, dec!(110));
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
