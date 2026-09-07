use bot::pairs_backtest::{run_backtest_on_series, BacktestExperiment};
use pairs::PairParams;
use botcore::{Symbol, Timeframe};
use rust_decimal_macros::dec;

fn params() -> PairParams {
    PairParams {
        leg_a: Symbol::new("ENAUSDT"),
        leg_b: Symbol::new("XRPUSDT"),
        timeframe: Timeframe::H1,
        rolling_window: 20,
        entry_z: 3.0,
        stop_z: 4.0,
        target_z: 0.0,
        max_hold_bars: 20,
        fee_per_leg: dec!(0),
        per_leg_notional_usdt: dec!(100),
        risk_pct_of_equity: dec!(0),
        max_notional_multiple_of_equity: dec!(1),
        enable_breakeven: false,
        breakeven_r_multiple: dec!(2),
    }
}

#[test]
fn trailing_stop_experiment_exits_earlier_than_the_plain_target_path() {
    let p = params();
    let times = vec![1_i64, 2, 3, 4];
    let z = vec![3.2, 1.5, 2.7, 0.0];
    let a = vec![100.0, 95.0, 97.0, 90.0];
    let b = vec![100.0, 100.0, 100.0, 100.0];

    let base = run_backtest_on_series(&p, &times, &z, &a, &b, None).expect("base");
    let trailing = run_backtest_on_series(
        &p,
        &times,
        &z,
        &a,
        &b,
        Some(BacktestExperiment::TrailingStop {
            arm_r: 1.0,
            giveback_r: 1.0,
        }),
    )
    .expect("trailing");

    assert_eq!(base.trades, 1);
    assert_eq!(trailing.trades, 1);
    assert_eq!(base.trades_detail[0].reason, "target");
    assert_eq!(trailing.trades_detail[0].reason, "trailing_stop");
    assert!(trailing.trades_detail[0].exit_ms < base.trades_detail[0].exit_ms);
    assert!(trailing.net < base.net);
    assert!(trailing.net > 0.0);
}

#[test]
fn add_on_experiment_increases_profit_on_a_continuation_winner() {
    let p = params();
    let times = vec![1_i64, 2, 3];
    let z = vec![3.2, 1.8, 0.0];
    let a = vec![100.0, 95.0, 90.0];
    let b = vec![100.0, 100.0, 100.0];

    let base = run_backtest_on_series(&p, &times, &z, &a, &b, None).expect("base");
    let add_on = run_backtest_on_series(
        &p,
        &times,
        &z,
        &a,
        &b,
        Some(BacktestExperiment::AddOn {
            trigger_r: 1.0,
            add_on_fraction: 0.5,
        }),
    )
    .expect("add_on");

    assert_eq!(base.trades, 1);
    assert_eq!(add_on.trades, 1);
    assert_eq!(base.trades_detail[0].reason, "target");
    assert_eq!(add_on.trades_detail[0].reason, "target");
    assert!(add_on.net > base.net);
    assert!(add_on.trades_detail[0].per_leg_notional > base.trades_detail[0].per_leg_notional);
}

#[test]
fn earlier_and_larger_add_ons_increase_pnl_more_than_later_and_smaller_ones() {
    let p = params();
    let times = vec![1_i64, 2, 3, 4];
    let z = vec![3.2, 2.0, 1.4, 0.0];
    let a = vec![100.0, 97.0, 95.0, 90.0];
    let b = vec![100.0, 100.0, 100.0, 100.0];

    let early_small = run_backtest_on_series(
        &p,
        &times,
        &z,
        &a,
        &b,
        Some(BacktestExperiment::AddOn {
            trigger_r: 1.0,
            add_on_fraction: 0.25,
        }),
    )
    .expect("early_small");
    let early_large = run_backtest_on_series(
        &p,
        &times,
        &z,
        &a,
        &b,
        Some(BacktestExperiment::AddOn {
            trigger_r: 1.0,
            add_on_fraction: 0.5,
        }),
    )
    .expect("early_large");
    let late_small = run_backtest_on_series(
        &p,
        &times,
        &z,
        &a,
        &b,
        Some(BacktestExperiment::AddOn {
            trigger_r: 1.5,
            add_on_fraction: 0.25,
        }),
    )
    .expect("late_small");

    assert!(early_large.net > early_small.net);
    assert!(early_small.net > late_small.net);
    assert!(early_large.trades_detail[0].per_leg_notional > early_small.trades_detail[0].per_leg_notional);
}
