mod support;

use botcore::{Symbol, Timeframe};
use pairs::{PairParams, Reconciliation, reconcile_pair};
use persistence::PairPositionRecord;
use rust_decimal_macros::dec;
use support::FaultExchange;

fn params() -> PairParams {
    PairParams {
        leg_a: Symbol::new("AAVEUSDT"),
        leg_b: Symbol::new("ETHUSDT"),
        timeframe: Timeframe::H1,
        rolling_window: 180,
        entry_z: 3.0,
        stop_z: 4.0,
        target_z: 0.0,
        max_hold_bars: 48,
        fee_per_leg: dec!(0.0002),
        per_leg_notional_usdt: dec!(25),
        risk_pct_of_equity: dec!(0.03),
        max_notional_multiple_of_equity: dec!(1),
        enable_breakeven: false,
        breakeven_r_multiple: dec!(2),
    }
}

fn sample_record() -> PairPositionRecord {
    PairPositionRecord {
        bot_id: "aave_eth".into(),
        side: "long_spread".into(),
        opened_at_ms: 1_700_000_000_000,
        entry_z: dec!(-3.21),
        a_symbol: Symbol::new("AAVEUSDT"),
        a_qty: dec!(1.5),
        a_entry: dec!(300.25),
        a_order_id: "oid-a".into(),
        b_symbol: Symbol::new("ETHUSDT"),
        b_qty: dec!(0.4),
        b_entry: dec!(3000.5),
        b_order_id: "oid-b".into(),
        breakeven_armed: false,
        per_leg_notional: dec!(450),
        capped_by: Some("available_equity".into()),
    }
}

#[tokio::test]
async fn a_flat_journal_and_a_flat_exchange_agree() {
    let ex = FaultExchange::new();
    let got = reconcile_pair(&ex, None, &params()).await.unwrap();
    assert_eq!(got, Reconciliation::Flat);
}

#[tokio::test]
async fn a_journalled_position_matching_the_exchange_is_adopted() {
    let ex = FaultExchange::new();
    ex.position("AAVEUSDT", dec!(1.5));
    ex.position("ETHUSDT", dec!(-0.4));
    let record = sample_record();
    let got = reconcile_pair(&ex, Some(&record), &params()).await.unwrap();
    assert_eq!(got, Reconciliation::Holding(Box::new(record)));
}

#[tokio::test]
async fn exchange_positions_with_an_empty_journal_halt_for_human_review() {
    let ex = FaultExchange::new();
    ex.position("AAVEUSDT", dec!(1.5));
    match reconcile_pair(&ex, None, &params()).await.unwrap() {
        Reconciliation::Halt { reason } => {
            assert!(reason.contains("AAVEUSDT"), "reason was {reason}");
        }
        other => panic!("expected Halt, got {other:?}"),
    }
}

#[tokio::test]
async fn a_journalled_position_the_exchange_does_not_have_halts_too() {
    let ex = FaultExchange::new();
    match reconcile_pair(&ex, Some(&sample_record()), &params()).await.unwrap() {
        Reconciliation::Halt { reason } => assert!(reason.contains("journal"), "reason was {reason}"),
        other => panic!("expected Halt, got {other:?}"),
    }
}

#[tokio::test]
async fn only_one_of_the_two_legs_being_open_halts() {
    let ex = FaultExchange::new();
    ex.position("AAVEUSDT", dec!(1.5));
    match reconcile_pair(&ex, Some(&sample_record()), &params()).await.unwrap() {
        Reconciliation::Halt { .. } => {}
        other => panic!("expected Halt, got {other:?}"),
    }
}

#[tokio::test]
async fn positions_on_other_bots_symbols_are_ignored() {
    let ex = FaultExchange::new();
    ex.position("BNBUSDT", dec!(5));
    assert_eq!(reconcile_pair(&ex, None, &params()).await.unwrap(), Reconciliation::Flat);
}
