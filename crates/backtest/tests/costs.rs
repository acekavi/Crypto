use backtest::{CostModel, funding_charge, funding_timestamps_in};
use botcore::Side;
use botcore::Symbol;
use exchange::bybit::wire::FundingRate;
use rust_decimal_macros::dec;

#[test]
fn a_maker_fee_is_the_rate_times_notional() {
    let m = CostModel {
        maker_fee_rate: dec!(0.0002),
    };
    // 50 units at 100 = 5000 notional; 5000 * 0.0002 = 1.0
    assert_eq!(m.maker_fee(dec!(50), dec!(100)), dec!(1.0));
    // 50 units at 104 = 5200 notional; 5200 * 0.0002 = 1.04
    assert_eq!(m.maker_fee(dec!(50), dec!(104)), dec!(1.04));
}

#[test]
fn a_long_pays_when_the_funding_rate_is_positive() {
    // 50 units at 100 = 5000 notional; 5000 * 0.0001 = 0.5 PAID.
    assert_eq!(
        funding_charge(Side::Buy, dec!(50), dec!(100), dec!(0.0001)),
        dec!(0.5)
    );
}

#[test]
fn a_short_receives_when_the_funding_rate_is_positive() {
    // Same rate, opposite side: the short is PAID 0.5, so the charge is
    // negative. Getting this backwards inverts the cost of every short.
    assert_eq!(
        funding_charge(Side::Sell, dec!(50), dec!(100), dec!(0.0001)),
        dec!(-0.5)
    );
}

#[test]
fn a_negative_rate_flips_both_sides() {
    // Negative rate: shorts pay longs.
    assert_eq!(
        funding_charge(Side::Buy, dec!(50), dec!(100), dec!(-0.0001)),
        dec!(-0.5)
    );
    assert_eq!(
        funding_charge(Side::Sell, dec!(50), dec!(100), dec!(-0.0001)),
        dec!(0.5)
    );
}

#[test]
fn only_funding_timestamps_strictly_inside_the_hold_are_charged() {
    let sym = Symbol::new("BTCUSDT");
    let r = |t: i64| FundingRate {
        symbol: sym.clone(),
        funding_time_ms: t,
        rate: dec!(0.0001),
    };
    let rates = vec![r(1000), r(2000), r(3000), r(4000)];

    // Held from 1000 to 3000. The boundaries are NOT charged: a position
    // opened exactly at a funding timestamp has not held through it, and one
    // closed exactly at one is already out.
    let charged = funding_timestamps_in(1000, 3000, &rates);
    let times: Vec<i64> = charged.iter().map(|f| f.funding_time_ms).collect();
    assert_eq!(times, vec![2000]);
}

#[test]
fn a_position_held_across_no_funding_timestamp_is_charged_nothing() {
    let sym = Symbol::new("BTCUSDT");
    let rates = vec![FundingRate {
        symbol: sym,
        funding_time_ms: 5000,
        rate: dec!(0.0001),
    }];
    assert!(funding_timestamps_in(1000, 3000, &rates).is_empty());
}

#[test]
fn a_full_round_trip_nets_out_to_the_hand_computed_figure() {
    // THE ORACLE. Long 50 units, entry 100, target 104, one funding period
    // held at 0.0001 with mark 100, maker fee 0.0002 both sides.
    //   gross pnl   = 50 * (104 - 100)        = 200
    //   entry fee   = 50 * 100   * 0.0002     =   1.00
    //   exit  fee   = 50 * 104   * 0.0002     =   1.04
    //   funding     = 50 * 100   * 0.0001     =   0.50  (long pays)
    //   net         = 200 - 1.00 - 1.04 - 0.50 = 197.46
    let m = CostModel {
        maker_fee_rate: dec!(0.0002),
    };
    let gross = dec!(50) * (dec!(104) - dec!(100));
    let entry_fee = m.maker_fee(dec!(50), dec!(100));
    let exit_fee = m.maker_fee(dec!(50), dec!(104));
    let funding = funding_charge(Side::Buy, dec!(50), dec!(100), dec!(0.0001));

    assert_eq!(gross - entry_fee - exit_fee - funding, dec!(197.46));
}
