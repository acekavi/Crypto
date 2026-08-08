use botcore::{Candle, Timeframe};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use strategy::ict::{
    Direction, Fvg, IctParams, detect_sweep, find_fvg, fvg_entry_price, in_ny_session, is_mss,
    is_swing_high, is_swing_low,
};

/// `hl` builds a candle from high/low; open and close are given explicitly
/// where a rule depends on them.
fn c(high: Decimal, low: Decimal, close: Decimal) -> Candle {
    Candle {
        open_time_ms: 0,
        open: close,
        high,
        low,
        close,
        volume: dec!(1),
        turnover: dec!(1),
    }
}

fn hl(high: i64, low: i64) -> Candle {
    c(Decimal::from(high), Decimal::from(low), Decimal::from(low))
}

// ------------------------------------------------------------- swings ----

#[test]
fn a_swing_high_needs_lower_highs_on_both_sides() {
    // index 2 is the peak: 10, 11, 15, 12, 9
    let v = vec![hl(10, 1), hl(11, 1), hl(15, 1), hl(12, 1), hl(9, 1)];
    assert!(is_swing_high(&v, 2, 2));
    assert!(!is_swing_high(&v, 1, 2), "not the highest in its window");
}

#[test]
fn an_edge_candle_can_never_be_a_swing() {
    // Both sides are required, so the newest candles cannot be swings yet.
    // That is the cost of a CONFIRMED structure rather than a guessed one.
    let v = vec![hl(10, 1), hl(11, 1), hl(15, 1), hl(12, 1), hl(9, 1)];
    assert!(!is_swing_high(&v, 4, 2), "no candles to its right");
    assert!(!is_swing_high(&v, 0, 2), "no candles to its left");
}

#[test]
fn a_swing_low_is_the_mirror() {
    let v = vec![hl(20, 10), hl(20, 9), hl(20, 5), hl(20, 8), hl(20, 11)];
    assert!(is_swing_low(&v, 2, 2));
    assert!(!is_swing_high(&v, 2, 2), "flat highs are not a swing high");
}

#[test]
fn an_equal_high_does_not_qualify() {
    // Strict inequality: a double top is not a single swing point.
    let v = vec![hl(10, 1), hl(15, 1), hl(15, 1), hl(12, 1), hl(9, 1)];
    assert!(!is_swing_high(&v, 2, 2));
}

// ------------------------------------------------------------- sweeps ----

#[test]
fn a_sweep_of_lows_that_closes_back_above_is_bullish() {
    // Price traded through the swing low at 100 and recovered to close above
    // it — liquidity taken, then rejected.
    let candle = c(dec!(108), dec!(95), dec!(105));
    assert_eq!(
        detect_sweep(&candle, None, Some(dec!(100))),
        Some(Direction::Bullish)
    );
}

#[test]
fn closing_below_the_level_is_a_break_not_a_sweep() {
    // THE DISTINCTION THE RULE EXISTS FOR. Same low, but the close stays
    // under the level: price broke it rather than being rejected by it, which
    // is the opposite event and must not arm a long.
    let candle = c(dec!(101), dec!(95), dec!(97));
    assert_eq!(detect_sweep(&candle, None, Some(dec!(100))), None);
}

#[test]
fn a_sweep_of_highs_that_closes_back_below_is_bearish() {
    let candle = c(dec!(115), dec!(99), dec!(103));
    assert_eq!(
        detect_sweep(&candle, Some(dec!(110)), None),
        Some(Direction::Bearish)
    );
}

#[test]
fn merely_touching_the_level_is_not_a_sweep() {
    // Low equals the swing low exactly: nothing was taken.
    let candle = c(dec!(108), dec!(100), dec!(105));
    assert_eq!(detect_sweep(&candle, None, Some(dec!(100))), None);
}

#[test]
fn with_no_prior_swing_there_is_nothing_to_sweep() {
    let candle = c(dec!(108), dec!(95), dec!(105));
    assert_eq!(detect_sweep(&candle, None, None), None);
}

// ---------------------------------------------------------------- mss ----

#[test]
fn a_bullish_shift_needs_a_close_above_the_recent_swing_high() {
    assert!(is_mss(Direction::Bullish, dec!(111), Some(dec!(110)), None));
    assert!(
        !is_mss(Direction::Bullish, dec!(109), Some(dec!(110)), None),
        "a close below the level is not a shift"
    );
}

#[test]
fn a_bearish_shift_is_the_mirror() {
    assert!(is_mss(Direction::Bearish, dec!(89), None, Some(dec!(90))));
    assert!(!is_mss(Direction::Bearish, dec!(91), None, Some(dec!(90))));
}

#[test]
fn without_an_opposing_swing_no_shift_can_be_confirmed() {
    assert!(!is_mss(Direction::Bullish, dec!(999), None, None));
}

// ---------------------------------------------------------------- fvg ----

#[test]
fn a_bullish_gap_is_candle_ones_high_below_candle_threes_low() {
    let c1 = c(dec!(100), dec!(95), dec!(99));
    let c3 = c(dec!(115), dec!(105), dec!(112));
    assert_eq!(
        find_fvg(&c1, &c3, Direction::Bullish),
        Some(Fvg {
            low: dec!(100),
            high: dec!(105)
        })
    );
}

#[test]
fn overlapping_candles_leave_no_gap() {
    // Candle 3's low sits inside candle 1's range: price traded through it,
    // so there is no imbalance to return to.
    let c1 = c(dec!(100), dec!(95), dec!(99));
    let c3 = c(dec!(110), dec!(98), dec!(108));
    assert_eq!(find_fvg(&c1, &c3, Direction::Bullish), None);
}

#[test]
fn a_gap_facing_the_wrong_way_does_not_count() {
    // A bearish setup enters a gap ABOVE price. A bullish-shaped gap must not
    // satisfy it, or the strategy would trade into the move it is fading.
    let c1 = c(dec!(100), dec!(95), dec!(99));
    let c3 = c(dec!(115), dec!(105), dec!(112));
    assert_eq!(find_fvg(&c1, &c3, Direction::Bearish), None);
}

#[test]
fn the_entry_sits_at_the_declared_depth_from_the_edge_price_reaches_first() {
    // Gap 100..105. A bullish setup falls INTO it from above, so depth is
    // measured down from 105: the midpoint is 102.5, and 0.75 is deeper.
    let fvg = Fvg {
        low: dec!(100),
        high: dec!(105),
    };
    assert_eq!(
        fvg_entry_price(&fvg, Direction::Bullish, dec!(0.5)),
        dec!(102.5)
    );
    assert_eq!(
        fvg_entry_price(&fvg, Direction::Bullish, dec!(0.75)),
        dec!(101.25)
    );
    // A bearish setup rises into it from below, so depth is measured up.
    assert_eq!(
        fvg_entry_price(&fvg, Direction::Bearish, dec!(0.5)),
        dec!(102.5)
    );
    assert_eq!(
        fvg_entry_price(&fvg, Direction::Bearish, dec!(0.75)),
        dec!(103.75)
    );
}

// ------------------------------------------------------------ session ----

#[test]
fn the_new_york_window_admits_only_its_own_hours() {
    let p = IctParams::variant_a();
    let at = |h: i64, m: i64| (h * 3_600_000) + (m * 60_000);

    assert!(!in_ny_session(at(13, 29), p.ny_open_ms, p.ny_close_ms));
    assert!(
        in_ny_session(at(13, 30), p.ny_open_ms, p.ny_close_ms),
        "open is inclusive"
    );
    assert!(in_ny_session(at(17, 0), p.ny_open_ms, p.ny_close_ms));
    assert!(
        !in_ny_session(at(20, 0), p.ny_open_ms, p.ny_close_ms),
        "close is exclusive"
    );
    assert!(!in_ny_session(at(2, 0), p.ny_open_ms, p.ny_close_ms));
}

#[test]
fn the_session_check_works_on_any_day_not_just_the_epoch() {
    // Uses ms-into-day, so a real 2024 timestamp must behave identically.
    let p = IctParams::variant_a();
    let day = 1_700_000_000_000i64 / 86_400_000 * 86_400_000;
    assert!(in_ny_session(
        day + 14 * 3_600_000,
        p.ny_open_ms,
        p.ny_close_ms
    ));
    assert!(!in_ny_session(
        day + 3 * 3_600_000,
        p.ny_open_ms,
        p.ny_close_ms
    ));
}

// ----------------------------------------------------------- variants ----

#[test]
fn the_declared_variants_are_exactly_the_six_in_the_spec() {
    // A tripwire, like the gate's threshold test: the spec fixes these before
    // any run, and each changes exactly ONE field from A so a difference is
    // attributable.
    let v = IctParams::declared_variants();
    assert_eq!(v.len(), 6);
    assert_eq!(
        v.iter().map(|(n, _)| *n).collect::<Vec<_>>(),
        vec!["A", "B", "C", "D", "E", "F"]
    );

    let a = IctParams::variant_a();
    assert_eq!(v[0].1, a, "A is the primary, unmodified");

    let differs = |p: &IctParams| {
        let mut n = 0;
        if p.swing_lookback != a.swing_lookback {
            n += 1
        }
        if p.mss_window != a.mss_window {
            n += 1
        }
        if p.fvg_entry_fraction != a.fvg_entry_fraction {
            n += 1
        }
        if p.stop_buffer_atr != a.stop_buffer_atr {
            n += 1
        }
        n
    };
    for (name, p) in v.iter().skip(1) {
        assert_eq!(
            differs(p),
            1,
            "variant {name} must change exactly one field"
        );
    }
}

#[test]
fn the_configured_roles_drive_which_timeframes_are_subscribed() {
    // A REGRESSION GUARD. `timeframes()` was hardcoded to [M15, H1, H4, D1]
    // while the params claimed to configure the roles, so a variant executing
    // on M5 never received a single M5 candle and could not fire at all. The
    // funnel showed it as execCandles=0 and I nearly reported that as "the
    // setup is rare" rather than "the wiring is broken".
    use strategy::Strategy;
    use strategy::ict::IctStrategy;

    let registered = IctStrategy::new(IctParams::variant_a());
    assert_eq!(
        registered.timeframes(),
        &[Timeframe::M15, Timeframe::H1, Timeframe::H4, Timeframe::D1]
    );

    let requested = IctStrategy::new(IctParams::h4_sweep_m5_entry());
    assert_eq!(
        requested.timeframes(),
        &[Timeframe::M5, Timeframe::H4, Timeframe::D1],
        "H4 serves as both bias and structure, so it appears once, and M5 must be present"
    );
}

#[test]
fn sessions_are_fixed_utc_windows_and_the_first_match_wins() {
    use strategy::ict::session_for;
    let at = |h: i64| h * 3_600_000;

    assert_eq!(session_for(at(2)), Some("asia"));
    // London (07-16) and New York (13-21) overlap for three hours. The first
    // match wins, so 14:00 is London — stated in the code and pinned here,
    // because a silent overlap rule would change which extremes get recorded.
    assert_eq!(session_for(at(14)), Some("london"));
    assert_eq!(session_for(at(18)), Some("newyork"));
    assert_eq!(
        session_for(at(22)),
        None,
        "22:00 falls outside every window"
    );

    // Works on a real timestamp, not just the epoch.
    let day = 1_700_000_000_000i64 / 86_400_000 * 86_400_000;
    assert_eq!(session_for(day + at(2)), Some("asia"));
}

#[test]
fn an_order_block_is_the_last_opposing_candles_body() {
    use strategy::ict::find_order_block;
    // Bullish setup: the last DOWN candle before price turned up. Body only —
    // the wick is where price already rejected; the body is where the
    // unfilled interest sits.
    let down = Candle {
        open_time_ms: 0,
        open: dec!(104),
        high: dec!(105),
        low: dec!(98),
        close: dec!(100),
        volume: dec!(1),
        turnover: dec!(1),
    };
    let later_up = Candle {
        open_time_ms: 1,
        open: dec!(100),
        high: dec!(110),
        low: dec!(100),
        close: dec!(109),
        volume: dec!(1),
        turnover: dec!(1),
    };
    let seq = vec![down.clone(), later_up];
    assert_eq!(
        find_order_block(&seq, Direction::Bullish),
        Some(Fvg {
            low: dec!(100),
            high: dec!(104)
        }),
        "body of the down candle, not its 98..105 range"
    );
}

#[test]
fn the_most_recent_opposing_candle_wins() {
    use strategy::ict::find_order_block;
    let mk = |o: i64, c: i64| Candle {
        open_time_ms: 0,
        open: Decimal::from(o),
        high: Decimal::from(o.max(c) + 2),
        low: Decimal::from(o.min(c) - 2),
        close: Decimal::from(c),
        volume: dec!(1),
        turnover: dec!(1),
    };
    // Two down candles; the LATER one is the order block.
    let seq = vec![mk(120, 110), mk(108, 100), mk(100, 115)];
    assert_eq!(
        find_order_block(&seq, Direction::Bullish),
        Some(Fvg {
            low: dec!(100),
            high: dec!(108)
        })
    );
}

#[test]
fn a_doji_has_no_body_and_so_no_order_block() {
    use strategy::ict::find_order_block;
    let doji = Candle {
        open_time_ms: 0,
        open: dec!(100),
        high: dec!(105),
        low: dec!(95),
        close: dec!(100),
        volume: dec!(1),
        turnover: dec!(1),
    };
    assert_eq!(find_order_block(&[doji], Direction::Bullish), None);
}

#[test]
fn a_bearish_order_block_is_the_last_up_candle() {
    use strategy::ict::find_order_block;
    let up = Candle {
        open_time_ms: 0,
        open: dec!(100),
        high: dec!(110),
        low: dec!(99),
        close: dec!(106),
        volume: dec!(1),
        turnover: dec!(1),
    };
    assert_eq!(
        find_order_block(&[up], Direction::Bearish),
        Some(Fvg {
            low: dec!(100),
            high: dec!(106)
        })
    );
}

#[test]
fn the_consolidated_strategy_carries_exactly_what_survived_measurement() {
    // A tripwire. Each field below was decided by measuring one change at a
    // time; silently flipping one would change the strategy without changing
    // the evidence that justified it.
    let p = IctParams::liquidity_sweep_v1();
    assert_eq!(p.structure_tf, Timeframe::H4, "H1 sweeps lost money");
    assert_eq!(p.execution_tf, Timeframe::M15);
    assert!(!p.require_mss, "only 3.2% of sweeps ever confirmed one");
    assert!(p.use_pdh_pdl, "tripled trades and raised profit factor");
    assert!(!p.use_session_levels, "PF 1.16 and 27% drawdown");
    assert!(p.use_order_block, "119 -> 200 trades");
    assert_eq!(p.ob_lookback, 5);
    assert_eq!(p.reward_multiple, dec!(3));
    assert_eq!(p.stop_buffer_atr, dec!(0), "stop sits at the swept level");
}
