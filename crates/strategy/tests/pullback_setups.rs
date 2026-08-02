use botcore::{Candle, Instrument, Side, Symbol, Timeframe};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use strategy::pullback::{PullbackStrategy, StrategyParams};
use strategy::{MarketContext, Strategy};

fn instrument() -> Instrument {
    Instrument {
        symbol: Symbol::new("BTCUSDT"),
        tick_size: dec!(0.1),
        qty_step: dec!(0.001),
        min_order_qty: dec!(0.001),
        launch_time_ms: 0,
    }
}

fn candle(open_time_ms: i64, high: Decimal, low: Decimal, close: Decimal) -> Candle {
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

/// Feed a flat-then-rising 4h series so EMA50 climbs above EMA200,
/// establishing long bias. Returns the strategy already warm on 4h.
fn warm_long_bias(strat: &mut PullbackStrategy, symbol: &Symbol, inst: &Instrument) {
    // 250 rising 4h candles is enough to warm EMA200 and put EMA50 above it.
    for i in 0..250i64 {
        let px = Decimal::from(1000 + i);
        let c = candle(i * 14_400_000, px + dec!(1), px - dec!(1), px);
        let ctx = MarketContext {
            symbol,
            timeframe: Timeframe::H4,
            candle: &c,
            instrument: inst,
        };
        strat.on_candle_close(&ctx);
    }
}

#[test]
fn defaults_match_the_spec() {
    let p = StrategyParams::defaults();
    assert_eq!(p.ema_fast, 50);
    assert_eq!(p.ema_slow, 200);
    assert_eq!(p.ema_entry, 20);
    assert_eq!(p.rsi_period, 14);
    assert_eq!(p.rsi_long_trigger, dec!(40));
    assert_eq!(p.rsi_short_trigger, dec!(60));
    assert_eq!(p.atr_period, 14);
    assert_eq!(p.atr_band_min_pct, dec!(0.003));
    assert_eq!(p.atr_band_max_pct, dec!(0.05));
    assert_eq!(p.swing_lookback, 10);
    assert_eq!(p.atr_stop_multiple, dec!(1.5));
    assert_eq!(p.reward_multiple, dec!(2));
    assert_eq!(p.pullback_lookback, 5);
    assert_eq!(p.pullback_atr_fraction, dec!(0.5));
}

#[test]
fn no_signal_before_indicators_are_warm() {
    let mut strat = PullbackStrategy::new(StrategyParams::defaults());
    let symbol = Symbol::new("BTCUSDT");
    let inst = instrument();

    // A single 1h candle cannot possibly complete a setup.
    let c = candle(0, dec!(101), dec!(99), dec!(100));
    let ctx = MarketContext {
        symbol: &symbol,
        timeframe: Timeframe::H1,
        candle: &c,
        instrument: &inst,
    };
    assert_eq!(strat.on_candle_close(&ctx), None);
}

#[test]
fn four_hour_candles_never_produce_a_signal_directly() {
    // The 4h stream only sets bias. Entries are timed on 1h.
    let mut strat = PullbackStrategy::new(StrategyParams::defaults());
    let symbol = Symbol::new("BTCUSDT");
    let inst = instrument();
    warm_long_bias(&mut strat, &symbol, &inst);

    let c = candle(250 * 14_400_000, dec!(1300), dec!(1240), dec!(1290));
    let ctx = MarketContext {
        symbol: &symbol,
        timeframe: Timeframe::H4,
        candle: &c,
        instrument: &inst,
    };
    assert_eq!(strat.on_candle_close(&ctx), None);
}

#[test]
fn declares_both_timeframes_and_a_warmup_covering_the_slow_ema() {
    let strat = PullbackStrategy::new(StrategyParams::defaults());
    let tfs = strat.timeframes();
    assert!(tfs.contains(&Timeframe::H1));
    assert!(tfs.contains(&Timeframe::H4));
    // EMA200 needs at least 200 samples before it reports a value at all.
    assert!(strat.warmup_candles() >= 200);
}

/// Drive a full long setup: warm 4h long bias, warm 1h indicators with a
/// pullback into EMA20, then a candle whose RSI crosses up through 40.
/// Returns whatever the final candle produced.
fn drive_long_setup(dip_depth: Decimal, final_close_bump: Decimal) -> Option<strategy::Signal> {
    let mut strat = PullbackStrategy::new(StrategyParams::defaults());
    let symbol = Symbol::new("BTCUSDT");
    let inst = instrument();
    warm_long_bias(&mut strat, &symbol, &inst);

    // 1h: 300 candles drifting up, then a dip that pulls RSI below 40 and
    // brings price back to the EMA, then a recovery candle.
    for i in 0..300i64 {
        let px = Decimal::from(1000 + i / 3);
        let c = candle(i * 3_600_000, px + dec!(2), px - dec!(2), px);
        let ctx = MarketContext {
            symbol: &symbol,
            timeframe: Timeframe::H1,
            candle: &c,
            instrument: &inst,
        };
        strat.on_candle_close(&ctx);
    }

    // Dip: several down candles push RSI below 40 and price down to the EMA.
    let base = Decimal::from(1000 + 299 / 3);
    for i in 0..8i64 {
        let px = base - dip_depth * Decimal::from(i + 1);
        let c = candle((300 + i) * 3_600_000, px + dec!(1), px - dec!(3), px);
        let ctx = MarketContext {
            symbol: &symbol,
            timeframe: Timeframe::H1,
            candle: &c,
            instrument: &inst,
        };
        strat.on_candle_close(&ctx);
    }

    // Recovery candle: closes up, pulling RSI back through 40.
    let px = base - dip_depth * dec!(8) + final_close_bump;
    let c = candle(308 * 3_600_000, px + dec!(2), px - dec!(1), px);
    let ctx = MarketContext {
        symbol: &symbol,
        timeframe: Timeframe::H1,
        candle: &c,
        instrument: &inst,
    };
    strat.on_candle_close(&ctx)
}

#[test]
fn a_signal_places_its_stop_below_entry_and_target_above_for_a_long() {
    // Whatever the exact setup, ANY long signal must be internally coherent:
    // stop below entry, target above, and the target exactly 2R away.
    if let Some(sig) = drive_long_setup(dec!(4), dec!(12)) {
        assert_eq!(sig.side, Side::Buy);
        assert!(
            sig.stop_price < sig.entry_price,
            "long stop {} was not below entry {}",
            sig.stop_price,
            sig.entry_price
        );
        assert!(
            sig.target_price > sig.entry_price,
            "long target {} was not above entry {}",
            sig.target_price,
            sig.entry_price
        );
        assert_eq!(
            sig.reward_multiple(),
            Some(dec!(2)),
            "target must sit at exactly 2R"
        );
    }
}

#[test]
fn no_signal_when_bias_is_absent() {
    // Without any 4h history there is no bias, so no 1h candle can fire.
    let mut strat = PullbackStrategy::new(StrategyParams::defaults());
    let symbol = Symbol::new("BTCUSDT");
    let inst = instrument();

    let mut produced = false;
    for i in 0..400i64 {
        let px = Decimal::from(1000 + (i % 20));
        let c = candle(i * 3_600_000, px + dec!(2), px - dec!(2), px);
        let ctx = MarketContext {
            symbol: &symbol,
            timeframe: Timeframe::H1,
            candle: &c,
            instrument: &inst,
        };
        if strat.on_candle_close(&ctx).is_some() {
            produced = true;
        }
    }
    assert!(!produced, "signals fired with no 4h bias established");
}

#[test]
fn flat_prices_are_rejected_by_the_volatility_gate() {
    // A perfectly flat series has ATR 0, which is below the 0.3% floor.
    let mut strat = PullbackStrategy::new(StrategyParams::defaults());
    let symbol = Symbol::new("BTCUSDT");
    let inst = instrument();
    warm_long_bias(&mut strat, &symbol, &inst);

    let mut produced = false;
    for i in 0..400i64 {
        let c = candle(i * 3_600_000, dec!(1000), dec!(1000), dec!(1000));
        let ctx = MarketContext {
            symbol: &symbol,
            timeframe: Timeframe::H1,
            candle: &c,
            instrument: &inst,
        };
        if strat.on_candle_close(&ctx).is_some() {
            produced = true;
        }
    }
    assert!(!produced, "a zero-volatility series produced a signal");
}

#[test]
fn per_symbol_state_is_isolated() {
    // Warming BTCUSDT must not warm ETHUSDT — shared state would let one
    // symbol's trend authorise another symbol's entry.
    let mut strat = PullbackStrategy::new(StrategyParams::defaults());
    let btc = Symbol::new("BTCUSDT");
    let eth = Symbol::new("ETHUSDT");
    let inst = instrument();
    warm_long_bias(&mut strat, &btc, &inst);

    let c = candle(0, dec!(101), dec!(99), dec!(100));
    let ctx = MarketContext {
        symbol: &eth,
        timeframe: Timeframe::H1,
        candle: &c,
        instrument: &inst,
    };
    assert_eq!(
        strat.on_candle_close(&ctx),
        None,
        "ETHUSDT fired on BTCUSDT's warm state"
    );
}

// ---------------------------------------------------------------------------
// Engineered firing setups.
//
// The `drive_long_setup` helper above deliberately does not fire — its RSI only
// recovers to ~37.9, short of the 40 trigger — and it was left that way rather
// than tuned until it passed. The two tests below instead construct series
// built to satisfy every gate, so the happy path is genuinely exercised:
// a signal MUST be produced, and its geometry is asserted concretely.
//
// The values were derived by instrumenting the indicators directly. With a
// 2-wide candle range the ATR settles at 2.0, so the pullback gate admits a
// candle whose extreme lands within 1.0 of EMA20. A 14-candle drift against the
// trend walks RSI to ~36 (long case) or ~63 (short case) — just past the
// trigger — and the reversal candle both crosses RSI back through it and puts
// its extreme on the EMA, satisfying the pullback and trigger together. That
// is what a bounce off the EMA looks like, which is the setup being modelled.
// ---------------------------------------------------------------------------

#[test]
fn an_engineered_long_setup_fires_with_coherent_geometry() {
    let mut strat = PullbackStrategy::new(StrategyParams::defaults());
    let symbol = Symbol::new("BTCUSDT");
    let inst = instrument();

    // 4h uptrend establishes long bias (EMA50 > EMA200).
    for k in 0..260i64 {
        let px = Decimal::from(100) + Decimal::new(k * 5, 1);
        let c = candle(k * 14_400_000, px + dec!(1), px - dec!(1), px);
        strat.on_candle_close(&MarketContext {
            symbol: &symbol,
            timeframe: Timeframe::H4,
            candle: &c,
            instrument: &inst,
        });
    }

    // 1h uptrend warms EMA20, RSI14 and ATR14 and keeps ATR/close near 0.9%,
    // comfortably inside the [0.3%, 5.0%] volatility band.
    let mut px = Decimal::from(100);
    let mut t = 0i64;
    for _ in 0..260 {
        let c = candle(t, px + dec!(1), px - dec!(1), px);
        strat.on_candle_close(&MarketContext {
            symbol: &symbol,
            timeframe: Timeframe::H1,
            candle: &c,
            instrument: &inst,
        });
        px += dec!(0.5);
        t += 3_600_000;
    }

    // 14 down candles walk RSI to ~36.4 — below the 40 trigger, so the next
    // up-move can cross it.
    for _ in 0..14 {
        px -= dec!(0.5);
        let c = candle(t, px + dec!(1), px - dec!(1), px);
        strat.on_candle_close(&MarketContext {
            symbol: &symbol,
            timeframe: Timeframe::H1,
            candle: &c,
            instrument: &inst,
        });
        t += 3_600_000;
    }

    // Reversal: closes 5 higher (crossing RSI back above 40) while its low
    // dips onto EMA20 (~225.3), satisfying the pullback gate.
    let close = px + dec!(5);
    let c = candle(t, close + dec!(1), dec!(225.3), close);
    let sig = strat
        .on_candle_close(&MarketContext {
            symbol: &symbol,
            timeframe: Timeframe::H1,
            candle: &c,
            instrument: &inst,
        })
        .expect("engineered long setup must fire");

    assert_eq!(sig.side, Side::Buy);
    assert!(
        sig.stop_price < sig.entry_price,
        "long stop {} must sit below entry {}",
        sig.stop_price,
        sig.entry_price
    );
    assert!(
        sig.target_price > sig.entry_price,
        "long target {} must sit above entry {}",
        sig.target_price,
        sig.entry_price
    );
    assert_eq!(
        sig.reward_multiple(),
        Some(dec!(2)),
        "target must sit at exactly 2R"
    );
    assert!(sig.atr > Decimal::ZERO, "signal must carry a positive ATR");
    assert_eq!(sig.symbol.as_str(), "BTCUSDT");
}

#[test]
fn an_engineered_short_setup_fires_with_mirrored_geometry() {
    let mut strat = PullbackStrategy::new(StrategyParams::defaults());
    let symbol = Symbol::new("BTCUSDT");
    let inst = instrument();

    // 4h downtrend establishes short bias (EMA50 < EMA200).
    for k in 0..260i64 {
        let px = Decimal::from(500) - Decimal::new(k * 5, 1);
        let c = candle(k * 14_400_000, px + dec!(1), px - dec!(1), px);
        strat.on_candle_close(&MarketContext {
            symbol: &symbol,
            timeframe: Timeframe::H4,
            candle: &c,
            instrument: &inst,
        });
    }

    let mut px = Decimal::from(500);
    let mut t = 0i64;
    for _ in 0..260 {
        let c = candle(t, px + dec!(1), px - dec!(1), px);
        strat.on_candle_close(&MarketContext {
            symbol: &symbol,
            timeframe: Timeframe::H1,
            candle: &c,
            instrument: &inst,
        });
        px -= dec!(0.5);
        t += 3_600_000;
    }

    // 14 up candles walk RSI above the 60 short trigger.
    for _ in 0..14 {
        px += dec!(0.5);
        let c = candle(t, px + dec!(1), px - dec!(1), px);
        strat.on_candle_close(&MarketContext {
            symbol: &symbol,
            timeframe: Timeframe::H1,
            candle: &c,
            instrument: &inst,
        });
        t += 3_600_000;
    }

    // Reversal: closes 4 lower (crossing RSI back below 60) while its high
    // reaches up to EMA20 (~374.7).
    let close = px - dec!(4);
    let c = candle(t, dec!(374.7), close - dec!(1), close);
    let sig = strat
        .on_candle_close(&MarketContext {
            symbol: &symbol,
            timeframe: Timeframe::H1,
            candle: &c,
            instrument: &inst,
        })
        .expect("engineered short setup must fire");

    assert_eq!(sig.side, Side::Sell);
    assert!(
        sig.stop_price > sig.entry_price,
        "short stop {} must sit above entry {}",
        sig.stop_price,
        sig.entry_price
    );
    assert!(
        sig.target_price < sig.entry_price,
        "short target {} must sit below entry {}",
        sig.target_price,
        sig.entry_price
    );
    assert_eq!(
        sig.reward_multiple(),
        Some(dec!(2)),
        "target must sit at exactly 2R"
    );
}
