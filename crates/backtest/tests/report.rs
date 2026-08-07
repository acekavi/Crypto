use backtest::{BacktestResult, ExitReason, RunSummary, summarise};
use botcore::{Side, Symbol};
use rust_decimal_macros::dec;

fn trade(
    net_pnl: rust_decimal::Decimal,
    gross_pnl: rust_decimal::Decimal,
    fees: rust_decimal::Decimal,
    funding: rust_decimal::Decimal,
    was_ambiguous: bool,
) -> backtest::ClosedTrade {
    backtest::ClosedTrade {
        symbol: Symbol::new("BTCUSDT"),
        side: Side::Buy,
        qty: dec!(1),
        entry_price: dec!(100),
        exit_price: dec!(100),
        entry_ms: 0,
        exit_ms: 1,
        gross_pnl,
        fees,
        funding,
        net_pnl,
        exit_reason: ExitReason::Target,
        was_ambiguous,
    }
}

#[test]
fn wins_and_losses_are_counted_by_net_pnl_sign() {
    let result = BacktestResult {
        trades: vec![
            // A win.
            trade(dec!(100), dec!(105), dec!(3), dec!(2), false),
            // A loss.
            trade(dec!(-50), dec!(-45), dec!(3), dec!(2), false),
            // Exactly breakeven counts as a loss, not a third bucket.
            trade(dec!(0), dec!(5), dec!(3), dec!(2), false),
        ],
        final_equity: dec!(10050),
        ambiguous_exits: 0,
        candles_replayed: 10,
        halt_events: 0,
    };

    let summary = summarise(&result);
    assert_eq!(summary.trades, 3);
    assert_eq!(summary.wins, 1);
    assert_eq!(summary.losses, 2);
}

#[test]
fn fees_and_funding_are_never_netted_away_from_net_pnl() {
    // A strategy that wins on paper but pays its entire edge away in costs
    // must still show that plainly: fees and funding as their own totals,
    // not folded silently into net_pnl.
    let result = BacktestResult {
        trades: vec![
            trade(dec!(10), dec!(20), dec!(7), dec!(3), false),
            trade(dec!(4), dec!(10), dec!(4), dec!(2), true),
        ],
        final_equity: dec!(10014),
        ambiguous_exits: 1,
        candles_replayed: 4,
        halt_events: 0,
    };

    let summary = summarise(&result);
    assert_eq!(
        summary,
        RunSummary {
            trades: 2,
            wins: 2,
            losses: 0,
            gross_pnl: dec!(30),
            total_fees: dec!(11),
            total_funding: dec!(5),
            net_pnl: dec!(14),
            ambiguous_exits: 1,
        }
    );
    // The identity the report exists to make visible, asserted exactly.
    assert_eq!(
        summary.gross_pnl - summary.total_fees - summary.total_funding,
        summary.net_pnl
    );
}

#[test]
fn ambiguous_exits_is_carried_through_from_the_result_not_recomputed() {
    // `ambiguous_exits` belongs to the whole run (from `run_backtest`), not
    // to any one trade summarise() can re-derive it from — carrying it
    // through, rather than dropping it, is the only correct behaviour.
    let result = BacktestResult {
        trades: vec![trade(dec!(1), dec!(1), dec!(0), dec!(0), false)],
        final_equity: dec!(10001),
        ambiguous_exits: 7,
        candles_replayed: 1,
        halt_events: 0,
    };

    assert_eq!(summarise(&result).ambiguous_exits, 7);
}

#[test]
fn an_empty_result_summarises_to_all_zeros() {
    let result = BacktestResult {
        trades: vec![],
        final_equity: dec!(10000),
        ambiguous_exits: 0,
        candles_replayed: 0,
        halt_events: 0,
    };

    assert_eq!(
        summarise(&result),
        RunSummary {
            trades: 0,
            wins: 0,
            losses: 0,
            gross_pnl: dec!(0),
            total_fees: dec!(0),
            total_funding: dec!(0),
            net_pnl: dec!(0),
            ambiguous_exits: 0,
        }
    );
}
