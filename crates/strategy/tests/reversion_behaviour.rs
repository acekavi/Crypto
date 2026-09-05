use botcore::{Candle, Instrument, Side, Symbol, Timeframe};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use strategy::reversion::{ReversionParams, ReversionStrategy};
use strategy::{MarketContext, Signal, Strategy};

const H1: i64 = 3_600_000;

fn instrument(sym: &Symbol) -> Instrument {
    Instrument {
        symbol: sym.clone(),
        tick_size: dec!(0.0001),
        qty_step: dec!(0.000001),
        min_order_qty: dec!(0.000001),
        min_notional: dec!(5),
        launch_time_ms: 0,
    }
}

fn candle(open_time_ms: i64, close: Decimal, range: Decimal) -> Candle {
    Candle {
        open_time_ms,
        open: close,
        high: close + range,
        low: close - range,
        close,
        volume: dec!(1),
        turnover: dec!(1),
    }
}

/// Feed `n` H4 candles at a fixed price so both H4 EMAs converge to it, which
/// makes trend strength ~0 and leaves the regime gate open.
fn warm_flat_h4(
    s: &mut ReversionStrategy,
    sym: &Symbol,
    inst: &Instrument,
    price: Decimal,
    n: i64,
) {
    for i in 0..n {
        let c = candle(i * H1 * 4, price, dec!(1));
        s.on_candle_close(&MarketContext {
            symbol: sym,
            timeframe: Timeframe::H4,
            candle: &c,
            instrument: inst,
        });
    }
}

fn feed_h1(
    s: &mut ReversionStrategy,
    sym: &Symbol,
    inst: &Instrument,
    close: Decimal,
    range: Decimal,
    t: i64,
) -> Option<Signal> {
    let c = candle(t, close, range);
    s.on_candle_close(&MarketContext {
        symbol: sym,
        timeframe: Timeframe::H1,
        candle: &c,
        instrument: inst,
    })
}

#[test]
fn every_declared_variant_targets_before_the_mean() {
    // THE CONSTRAINT THAT MAKES A REVERSION TARGET COHERENT. A 2R target
    // sitting past the mean would need price to overshoot the very level it is
    // reverting to, which is not the hypothesis under test. The spec fixes six
    // variants; this proves each one is valid by construction, so an invalid
    // variant cannot be quietly introduced later.
    let offset = dec!(0.3); // the live stop-limit offset, in ATRs
    for (name, p) in ReversionParams::declared_variants() {
        assert!(
            p.target_lands_before_mean(offset),
            "variant {name}: 2 x ({} + {offset}) exceeds a stretch of {}",
            p.stop_atr,
            p.stretch_atr
        );
    }
}

#[test]
fn the_declared_variants_are_exactly_the_six_in_the_spec() {
    // A tripwire, like the gate's threshold test. The spec fixes these six
    // before any run; silently adding a seventh would be a different study.
    let v = ReversionParams::declared_variants();
    assert_eq!(v.len(), 6);
    let names: Vec<&str> = v.iter().map(|(n, _)| *n).collect();
    assert_eq!(names, vec!["A", "B", "C", "D", "E", "F"]);

    let (primary_name, primary) = &v[0];
    assert_eq!(*primary_name, "A");
    assert_eq!(primary.stretch_atr, dec!(3.0));
    assert_eq!(primary.stop_atr, dec!(1.0));
    assert_eq!(primary, &ReversionParams::variant_a());
}

#[test]
fn a_deep_stretch_below_the_mean_signals_long() {
    let sym = Symbol::new("BTCUSDT");
    let inst = instrument(&sym);
    let mut s = ReversionStrategy::new(ReversionParams::variant_a());
    warm_flat_h4(&mut s, &sym, &inst, dec!(100), 300);

    // Warm H1 at a flat 100 so EMA20 ~ 100 and ATR ~ 2.
    let mut t = 0;
    for _ in 0..100 {
        feed_h1(&mut s, &sym, &inst, dec!(100), dec!(1), t);
        t += H1;
    }

    // Drop far below the mean in one candle. The big candle also inflates
    // ATR, which damps the ratio, so the displacement has to be genuinely
    // large: close 90 works out to -3.42 ATRs against the 3.0 threshold.
    let sig = feed_h1(&mut s, &sym, &inst, dec!(90), dec!(1), t)
        .expect("a deep downward stretch must signal");
    assert_eq!(sig.side, Side::Buy, "fading a drop means buying");
    // Entry is placed FURTHER down, not at the close.
    assert!(
        sig.entry_price < dec!(90),
        "entry {} must sit below the close, deeper into the stretch",
        sig.entry_price
    );
    assert!(
        sig.stop_price < sig.entry_price,
        "a long's stop must sit below its entry"
    );
}

#[test]
fn a_deep_stretch_above_the_mean_signals_short() {
    let sym = Symbol::new("BTCUSDT");
    let inst = instrument(&sym);
    let mut s = ReversionStrategy::new(ReversionParams::variant_a());
    warm_flat_h4(&mut s, &sym, &inst, dec!(100), 300);

    let mut t = 0;
    for _ in 0..100 {
        feed_h1(&mut s, &sym, &inst, dec!(100), dec!(1), t);
        t += H1;
    }

    let sig = feed_h1(&mut s, &sym, &inst, dec!(110), dec!(1), t)
        .expect("a deep upward stretch must signal");
    assert_eq!(sig.side, Side::Sell);
    assert!(
        sig.entry_price > dec!(110),
        "entry must sit above the close"
    );
    assert!(
        sig.stop_price > sig.entry_price,
        "a short's stop sits above"
    );
}

#[test]
fn price_near_the_mean_does_not_signal() {
    let sym = Symbol::new("BTCUSDT");
    let inst = instrument(&sym);
    let mut s = ReversionStrategy::new(ReversionParams::variant_a());
    warm_flat_h4(&mut s, &sym, &inst, dec!(100), 300);

    let mut t = 0;
    for _ in 0..100 {
        feed_h1(&mut s, &sym, &inst, dec!(100), dec!(1), t);
        t += H1;
    }

    // About 1 ATR of displacement — a long way short of the 3.0 required.
    assert!(feed_h1(&mut s, &sym, &inst, dec!(98), dec!(1), t).is_none());
}

#[test]
fn a_trending_regime_refuses_even_a_deep_stretch() {
    // THE CLAUSE THAT SEPARATES THIS FROM "buy anything that fell". Fading
    // displacement while a trend is working is the falling-knife loss, so the
    // gate must refuse regardless of how stretched price is.
    let sym = Symbol::new("BTCUSDT");
    let inst = instrument(&sym);
    let mut s = ReversionStrategy::new(ReversionParams::variant_a());

    // A steadily rising H4 series drives EMA50 well above EMA200, so trend
    // strength climbs past the 0.02 gate.
    let mut price = dec!(100);
    for i in 0..400 {
        let c = candle(i * H1 * 4, price, dec!(1));
        s.on_candle_close(&MarketContext {
            symbol: &sym,
            timeframe: Timeframe::H4,
            candle: &c,
            instrument: &inst,
        });
        price += dec!(1);
    }

    let mut t = 0;
    for _ in 0..100 {
        feed_h1(&mut s, &sym, &inst, dec!(500), dec!(1), t);
        t += H1;
    }

    // The displacement must be large enough to clear the stretch threshold on
    // its own, or this test passes for the wrong reason: a smaller drop is
    // refused by the stretch check and never reaches the regime gate at all.
    // Close 490 against a flat-500 mean is -3.42 ATRs, the same magnitude that
    // DOES signal in a ranging market above.
    assert!(
        feed_h1(&mut s, &sym, &inst, dec!(490), dec!(1), t).is_none(),
        "a stretched price in a strong trend must be refused, not faded"
    );
}

#[test]
fn nothing_signals_before_the_indicators_are_warm() {
    let sym = Symbol::new("BTCUSDT");
    let inst = instrument(&sym);
    let mut s = ReversionStrategy::new(ReversionParams::variant_a());

    // No H4 history at all, so trend strength is unknown. A stretch must not
    // signal on an unknown regime — that would trade the gate blind.
    let mut t = 0;
    for _ in 0..100 {
        feed_h1(&mut s, &sym, &inst, dec!(100), dec!(1), t);
        t += H1;
    }
    assert!(feed_h1(&mut s, &sym, &inst, dec!(94), dec!(1), t).is_none());
}

#[test]
fn a_wider_stretch_threshold_signals_less_often() {
    // Sanity on the parameter's direction: variant E (4.0) must be strictly
    // harder to trigger than variant B (2.5) on identical data.
    let sym = Symbol::new("BTCUSDT");
    let inst = instrument(&sym);

    let count = |p: ReversionParams| {
        let mut s = ReversionStrategy::new(p);
        warm_flat_h4(&mut s, &sym, &inst, dec!(100), 300);
        let mut t = 0;
        for _ in 0..100 {
            feed_h1(&mut s, &sym, &inst, dec!(100), dec!(1), t);
            t += H1;
        }
        // A 3.42-ATR displacement: clears 2.5, does not clear 4.0.
        feed_h1(&mut s, &sym, &inst, dec!(90), dec!(1), t).is_some()
    };

    let variants = ReversionParams::declared_variants();
    let b = variants.iter().find(|(n, _)| *n == "B").unwrap().1.clone();
    let e = variants.iter().find(|(n, _)| *n == "E").unwrap().1.clone();
    assert!(
        count(b),
        "a 2.5 ATR threshold must trigger on a 3 ATR stretch"
    );
    assert!(!count(e), "a 4.0 ATR threshold must not");
}
