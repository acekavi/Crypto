use botcore::{Balance, Position};
use risk::AccountState;
use rust_decimal::Decimal;

const MS_PER_DAY: i64 = 86_400_000;

/// Floor a timestamp to the most recent 00:00 UTC boundary.
///
/// Both the daily entry cap and the daily drawdown baseline are defined
/// against this boundary, so every consumer must agree on exactly where it
/// falls.
pub fn utc_day_start_ms(now_ms: i64) -> i64 {
    now_ms.div_euclid(MS_PER_DAY) * MS_PER_DAY
}

/// The all-time peak equity, which may only rise.
///
/// If this tracked equity downward the total-drawdown halt could never fire —
/// the baseline would keep retreating to meet the loss.
pub fn update_high_water_mark(previous: Decimal, current_equity: Decimal) -> Decimal {
    if current_equity > previous {
        current_equity
    } else {
        previous
    }
}

/// What the journal knows that the exchange does not.
///
/// The exchange reports positions and balance; the journal holds today's fill
/// count, the persisted halt, and the drawdown baselines. Both halves are
/// needed before the risk layer can decide anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalFacts {
    pub entries_filled_today: u32,
    pub halt_reason: Option<String>,
    pub day_start_equity: Decimal,
    pub high_water_mark: Decimal,
}

/// Combine exchange truth and journal memory into the state the risk layer reads.
///
/// The high-water mark is raised to current equity when the journal's recorded
/// mark is behind — otherwise a session that made a new peak before the mark was
/// persisted would understate every drawdown measured afterwards.
pub fn assemble_account_state(
    balance: &Balance,
    positions: Vec<Position>,
    facts: JournalFacts,
) -> AccountState {
    AccountState {
        equity: balance.equity,
        available: balance.available,
        open_positions: positions,
        day_start_equity: facts.day_start_equity,
        high_water_mark: update_high_water_mark(facts.high_water_mark, balance.equity),
        entries_filled_today: facts.entries_filled_today,
        halt_reason: facts.halt_reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use botcore::{Balance, Position, Side, Symbol};
    use rust_decimal_macros::dec;

    const DAY: i64 = 86_400_000;

    fn balance() -> Balance {
        Balance {
            equity: dec!(10000),
            available: dec!(9000),
        }
    }

    fn facts() -> JournalFacts {
        JournalFacts {
            entries_filled_today: 2,
            halt_reason: None,
            day_start_equity: dec!(10500),
            high_water_mark: dec!(12000),
        }
    }

    #[test]
    fn a_timestamp_exactly_on_midnight_is_its_own_day_start() {
        assert_eq!(utc_day_start_ms(5 * DAY), 5 * DAY);
    }

    #[test]
    fn a_timestamp_mid_day_floors_to_the_preceding_midnight() {
        assert_eq!(utc_day_start_ms(5 * DAY + 1), 5 * DAY);
        assert_eq!(utc_day_start_ms(5 * DAY + DAY - 1), 5 * DAY);
    }

    #[test]
    fn day_boundaries_are_contiguous_with_no_gap_or_overlap() {
        let a = utc_day_start_ms(5 * DAY + DAY - 1);
        let b = utc_day_start_ms(6 * DAY);
        assert_eq!(b - a, DAY);
    }

    #[test]
    fn the_high_water_mark_rises_with_a_new_peak() {
        assert_eq!(
            update_high_water_mark(dec!(10000), dec!(11000)),
            dec!(11000)
        );
    }

    #[test]
    fn the_high_water_mark_never_falls() {
        // If it tracked equity downward, the total-drawdown halt could never
        // fire — the baseline would keep retreating to meet the loss.
        assert_eq!(update_high_water_mark(dec!(12000), dec!(9000)), dec!(12000));
        assert_eq!(
            update_high_water_mark(dec!(12000), dec!(12000)),
            dec!(12000)
        );
    }

    #[test]
    fn assembly_carries_every_field_through_unchanged() {
        let positions = vec![Position {
            symbol: Symbol::new("BTCUSDT"),
            side: Side::Buy,
            size: dec!(1),
            entry_price: dec!(100),
            liq_price: None,
            unrealized_pnl: dec!(5),
        }];
        let state = assemble_account_state(&balance(), positions, facts());

        assert_eq!(state.equity, dec!(10000));
        assert_eq!(state.available, dec!(9000));
        assert_eq!(state.open_positions.len(), 1);
        assert_eq!(state.day_start_equity, dec!(10500));
        assert_eq!(state.high_water_mark, dec!(12000));
        assert_eq!(state.entries_filled_today, 2);
        assert_eq!(state.halt_reason, None);
    }

    #[test]
    fn a_persisted_halt_reason_survives_assembly() {
        // The halt must reach the risk layer, or a restart would silently
        // resume trading a halted account.
        let mut f = facts();
        f.halt_reason = Some("daily drawdown".into());
        let state = assemble_account_state(&balance(), vec![], f);
        assert_eq!(state.halt_reason.as_deref(), Some("daily drawdown"));
    }

    #[test]
    fn assembly_raises_a_stale_high_water_mark_to_current_equity() {
        // Equity above the recorded peak means the journal's mark is behind;
        // using it unraised would understate later drawdowns.
        let mut f = facts();
        f.high_water_mark = dec!(9000);
        let state = assemble_account_state(&balance(), vec![], f);
        assert_eq!(state.high_water_mark, dec!(10000));
    }

    use proptest::prelude::*;

    proptest! {
        /// Every timestamp must land in exactly one day, and the boundary must
        /// never sit in the future relative to the timestamp itself. Negative
        /// timestamps (pre-1970) are included deliberately: integer division
        /// truncates toward zero and would place them in the wrong day, which
        /// `div_euclid` avoids.
        #[test]
        fn a_day_start_always_precedes_its_timestamp_and_is_within_one_day(
            now_ms in -1_000_000_000_000i64..4_000_000_000_000i64,
        ) {
            let start = utc_day_start_ms(now_ms);
            prop_assert!(start <= now_ms, "day start {start} was after {now_ms}");
            prop_assert!(
                now_ms - start < MS_PER_DAY,
                "day start {start} was more than a day before {now_ms}"
            );
            prop_assert_eq!(start % MS_PER_DAY, 0, "day start {} was not on a boundary", start);
        }
    }
}
