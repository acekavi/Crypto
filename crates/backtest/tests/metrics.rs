use backtest::metrics::{compute, equity_curve};
use backtest::{ClosedTrade, ExitReason};
use botcore::{Side, Symbol};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// A closed trade carrying only what the metrics read. Prices are filler.
fn trade(exit_ms: i64, net_pnl: Decimal) -> ClosedTrade {
    ClosedTrade {
        symbol: Symbol::new("BTCUSDT"),
        side: Side::Buy,
        qty: dec!(1),
        entry_price: dec!(100),
        exit_price: dec!(100),
        entry_ms: exit_ms - 1,
        exit_ms,
        gross_pnl: net_pnl,
        fees: dec!(0),
        funding: dec!(0),
        net_pnl,
        exit_reason: if net_pnl >= Decimal::ZERO {
            ExitReason::Target
        } else {
            ExitReason::Stop
        },
        was_ambiguous: false,
    }
}

#[test]
fn expectancy_is_mean_net_pnl_per_trade() {
    // (+100 -50 +30) / 3 = 26.666...
    let m = compute(
        &[trade(1, dec!(100)), trade(2, dec!(-50)), trade(3, dec!(30))],
        dec!(10000),
    );
    assert_eq!(m.trade_count, 3);
    assert_eq!(m.net_pnl, dec!(80));
    assert_eq!(m.expectancy, dec!(80) / dec!(3));
}

#[test]
fn profit_factor_is_gross_profit_over_gross_loss() {
    // wins 100 + 30 = 130; losses |-50| = 50; 130 / 50 = 2.6
    let m = compute(
        &[trade(1, dec!(100)), trade(2, dec!(-50)), trade(3, dec!(30))],
        dec!(10000),
    );
    assert_eq!(m.gross_profit, dec!(130));
    assert_eq!(m.gross_loss, dec!(50));
    assert_eq!(m.profit_factor, Some(dec!(2.6)));
}

#[test]
fn profit_factor_is_none_when_nothing_lost() {
    // Not infinity and not a sentinel: a sample with no losers has not proven
    // a profit factor, it has proven the sample is too small. The gate must
    // confront that rather than see a flattering number.
    let m = compute(&[trade(1, dec!(100)), trade(2, dec!(30))], dec!(10000));
    assert_eq!(m.profit_factor, None);
}

#[test]
fn win_rate_counts_only_strictly_positive_trades() {
    // A scratch trade is not a win. Counting it as one inflates the rate on
    // exactly the strategies that scratch most.
    let m = compute(
        &[trade(1, dec!(100)), trade(2, dec!(0)), trade(3, dec!(-10))],
        dec!(10000),
    );
    assert_eq!(m.wins, 1);
    assert_eq!(m.losses, 1);
    assert_eq!(m.trade_count, 3);
}

#[test]
fn max_drawdown_is_measured_from_the_running_peak_not_starting_equity() {
    // 10000 -> 20000 (peak) -> 17000. The decline is 3000 from a peak of
    // 20000 = 15%. Measured against starting equity it would look like a 30%
    // GAIN and no drawdown at all — the error that hides real risk after a
    // strategy has run up.
    let m = compute(&[trade(1, dec!(10000)), trade(2, dec!(-3000))], dec!(10000));
    assert_eq!(m.max_drawdown_pct, dec!(15));
}

#[test]
fn max_drawdown_takes_the_deepest_of_several_declines() {
    // +1000 (11000), -500 (10500), +2000 (12500 peak), -2500 (10000).
    // First decline: 500/11000 = 4.545%. Second: 2500/12500 = 20%.
    let m = compute(
        &[
            trade(1, dec!(1000)),
            trade(2, dec!(-500)),
            trade(3, dec!(2000)),
            trade(4, dec!(-2500)),
        ],
        dec!(10000),
    );
    assert_eq!(m.max_drawdown_pct, dec!(20));
}

#[test]
fn a_run_that_only_gains_has_no_drawdown() {
    let m = compute(&[trade(1, dec!(100)), trade(2, dec!(200))], dec!(10000));
    assert_eq!(m.max_drawdown_pct, dec!(0));
}

#[test]
fn no_trades_produces_zeroed_metrics_rather_than_a_divide_by_zero() {
    let m = compute(&[], dec!(10000));
    assert_eq!(m.trade_count, 0);
    assert_eq!(m.expectancy, dec!(0));
    assert_eq!(m.profit_factor, None);
    assert_eq!(m.max_drawdown_pct, dec!(0));
}

#[test]
fn the_equity_curve_is_ordered_by_exit_time_not_input_order() {
    // A trade affects equity when it CLOSES. Trades arriving out of order
    // (they can, across symbols) must not produce a curve that dips and
    // recovers in an order that never happened — that would invent drawdowns.
    let curve = equity_curve(&[trade(3, dec!(100)), trade(1, dec!(50))], dec!(10000));
    assert_eq!(curve, vec![(1, dec!(10050)), (3, dec!(10150))]);
}

#[test]
fn fees_and_funding_are_reported_separately_not_folded_into_pnl() {
    // A strategy paying its entire edge away in costs must be visibly doing
    // so, so these stay their own line items.
    let mut t = trade(1, dec!(10));
    t.gross_pnl = dec!(15);
    t.fees = dec!(3);
    t.funding = dec!(2);
    let m = compute(&[t], dec!(10000));
    assert_eq!(m.total_fees, dec!(3));
    assert_eq!(m.total_funding, dec!(2));
    assert_eq!(m.net_pnl, dec!(10));
}
