//! Proves the engine's stop protections are durable: written through to the
//! journal at every mutation, rebuilt at startup for positions the exchange
//! still reports, and dropped for those it does not.
//!
//! `EngineLoop::protections` used to live only in memory. A restart mid-trade
//! therefore orphaned an open position: no breakeven management and no stop
//! escalation, because `Position` as the exchange reports it carries neither
//! the trigger, nor the 1R the position was sized against, nor whether the
//! stop has already moved to entry. The strategy's 1:5 target can take days to
//! reach, so a restart mid-trade is expected, not hypothetical.
//!
//! The two rules that outrank everything else here:
//!
//! * **The exchange is authoritative about which positions exist.** The
//!   journal supplies only what the exchange does not report. A journal row
//!   with no matching open position is stale and is deleted, never adopted —
//!   otherwise a restart would have the engine amending the stop of a position
//!   that closed while it was down.
//! * **A journal write failure must never stop the engine managing a
//!   position.** Losing an audit row is bad; refusing to move a stop to
//!   breakeven because a write failed is worse.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use bot::engine_loop::{CandleOutcome, EngineLoop};
use botcore::{
    Balance, Candle, Instrument, OpenOrder, OrderState, Position, Side, Symbol, Timeframe,
};
use engine::mock::MockExchange;
use exchange::bybit::transport::ExchangeError;
use exchange::bybit::wire::Ticker;
use exchange::bybit::ws_private::AccountEvent;
use persistence::{Journal, ProtectionRecord, TradeEventKind};
use risk::{RiskManager, RiskParams};
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use strategy::{PullbackStrategy, pullback::StrategyParams};

const H1: i64 = 3_600_000;
const T0: i64 = 1_000_000;

/// Entry 100 with the stop-limit at 90 makes 1R exactly 10, so 2R is a candle
/// high of 120 for a long. The trigger sits one unit inside the stop-limit, so
/// the breakeven amend's offset is 1. Same geometry as
/// `breakeven_behaviour.rs`, so the two files describe the same trade.
const ENTRY: Decimal = dec!(100);
const LONG_TRIGGER: Decimal = dec!(91);
const LONG_STOP_LIMIT: Decimal = dec!(90);
const ATR: Decimal = dec!(5);

/// `PullbackStrategy` declares H1 and H4, so H1 is its finest — the timeframe
/// the breakeven check rides.
const EXECUTION_TF: Timeframe = Timeframe::H1;

fn btc() -> Symbol {
    Symbol::new("BTCUSDT")
}

fn instrument() -> Instrument {
    Instrument {
        symbol: btc(),
        tick_size: dec!(0.1),
        qty_step: dec!(0.001),
        min_order_qty: dec!(0.001),
        min_notional: dec!(5),
        launch_time_ms: 0,
    }
}

fn position() -> Position {
    Position {
        symbol: btc(),
        side: Side::Buy,
        size: dec!(1),
        entry_price: ENTRY,
        liq_price: None,
        unrealized_pnl: Decimal::ZERO,
    }
}

fn ticker(last_price: Decimal) -> Ticker {
    // A one-tick book straddling the last trade: these tests only care about
    // `last_price`, but a `Ticker` with no book would be an impossible state.
    Ticker {
        symbol: btc(),
        turnover_24h: Decimal::ZERO,
        last_price,
        bid1: last_price - dec!(0.5),
        ask1: last_price + dec!(0.5),
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

/// A candle that reaches 2R on a long entered at 100 and closes back below it.
fn candle_at_2r() -> Candle {
    candle(T0, dec!(130), dec!(104), dec!(105))
}

/// A journal on a temp file, following the convention every other test here
/// uses. The `TempDir` must outlive the journal, so it is returned alongside.
async fn temp_journal() -> (Arc<Journal>, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let journal = Arc::new(
        Journal::open_local(dir.path().join("j.db").to_str().expect("utf-8 path"))
            .await
            .expect("journal opens"),
    );
    (journal, dir)
}

/// An engine over `mock`, sharing `journal`. Separate from `temp_journal` so a
/// test can build a SECOND engine over the same journal — which is what a
/// restart is.
fn engine_sharing(mock: Arc<MockExchange>, journal: Arc<Journal>) -> EngineLoop {
    EngineLoop::new(
        Box::new(PullbackStrategy::new(StrategyParams::defaults())),
        RiskManager::new(RiskParams::defaults(), dec!(0.3)),
        mock,
        journal,
        vec![instrument()],
        "cfg".into(),
        3,
        250,
    )
}

/// An engine holding one open long with its stop recorded in memory, exactly
/// as `on_candle_closed` records it after a successful `place_entry`.
async fn engine_with_recorded_stop() -> (
    EngineLoop,
    Arc<MockExchange>,
    Arc<Journal>,
    tempfile::TempDir,
) {
    let mock = Arc::new(
        MockExchange::new()
            .with_positions(vec![position()])
            .with_tickers(vec![ticker(dec!(105))]),
    );
    let (journal, dir) = temp_journal().await;
    let mut engine = engine_sharing(Arc::clone(&mock), Arc::clone(&journal));
    engine.protect_with_breakeven_for_test(
        btc(),
        Side::Buy,
        ENTRY,
        LONG_TRIGGER,
        LONG_STOP_LIMIT,
        ATR,
        Some(dec!(2)),
    );
    (engine, mock, journal, dir)
}

/// A journal row for an open long at 100 whose stop still sits at its original
/// trigger — what the journal holds when the process dies mid-trade.
fn journalled_protection() -> ProtectionRecord {
    ProtectionRecord {
        symbol: btc(),
        order_link_id: "entry-1".into(),
        side: Side::Buy,
        trigger: LONG_TRIGGER,
        atr: ATR,
        entry_price: ENTRY,
        initial_risk: ENTRY - LONG_STOP_LIMIT,
        stop_limit_offset: LONG_TRIGGER - LONG_STOP_LIMIT,
        breakeven_at_r: Some(dec!(2)),
        moved_to_breakeven: false,
        updated_at_ms: T0,
    }
}

async fn kinds_for(journal: &Journal, symbol: &Symbol) -> Vec<TradeEventKind> {
    journal
        .events_for(symbol)
        .await
        .expect("events load")
        .iter()
        .map(|e| e.kind)
        .collect()
}

#[tokio::test]
async fn moving_a_stop_to_breakeven_is_persisted_with_the_rewritten_trigger() {
    // In-memory only, the breakeven flag dies with the process — and the next
    // start would escalate a stop already resting at entry from the trigger it
    // no longer has.
    let (mut engine, mock, journal, _d) = engine_with_recorded_stop().await;

    engine
        .drive_breakeven_stops(&btc(), EXECUTION_TF, &candle_at_2r())
        .await
        .expect("breakeven pass");

    assert_eq!(mock.amended_stops().len(), 1, "setup: the stop must move");
    let saved = journal.load_protections().await.expect("load");
    assert_eq!(saved.len(), 1, "the moved stop must be on record");
    assert!(
        saved[0].moved_to_breakeven,
        "the amend must be durable, not in-memory only"
    );
    assert_eq!(
        saved[0].trigger, ENTRY,
        "the rewritten trigger must persist too, or the ladder measures from the old one"
    );
    assert!(
        kinds_for(&journal, &btc())
            .await
            .contains(&TradeEventKind::StopMovedToBreakeven)
    );
}

#[tokio::test]
async fn a_rejected_amend_records_the_attempt_without_moving_the_journalled_stop() {
    // The in-memory state did not change either, so writing a protection here
    // would put the journal AHEAD of both the engine and the exchange.
    let mock = Arc::new(
        MockExchange::new()
            .with_positions(vec![position()])
            .with_tickers(vec![ticker(dec!(105))])
            .fail_amend_stop_always(ExchangeError::WebSocket("timeout".into())),
    );
    let (journal, _d) = temp_journal().await;
    let mut engine = engine_sharing(Arc::clone(&mock), Arc::clone(&journal));
    engine.protect_with_breakeven_for_test(
        btc(),
        Side::Buy,
        ENTRY,
        LONG_TRIGGER,
        LONG_STOP_LIMIT,
        ATR,
        Some(dec!(2)),
    );

    engine
        .drive_breakeven_stops(&btc(), EXECUTION_TF, &candle_at_2r())
        .await
        .expect("a rejected amend must not abort the pass");

    assert!(
        journal.load_protections().await.expect("load").is_empty(),
        "a rejected amend must not write a protection the engine does not hold"
    );
    assert_eq!(
        kinds_for(&journal, &btc()).await,
        vec![TradeEventKind::StopAmendFailed],
        "the rejection itself belongs in the audit trail"
    );
}

#[tokio::test]
async fn a_restart_rebuilds_protections_for_positions_still_open() {
    // The whole point of the table: a second engine over the same journal
    // picks up the trade the first one was managing.
    let (journal, _d) = temp_journal().await;
    journal
        .upsert_protection(&journalled_protection())
        .await
        .expect("seed the journal as a crashed process would have left it");

    let mock = Arc::new(MockExchange::new().with_positions(vec![position()]));
    let mut restarted = engine_sharing(mock, Arc::clone(&journal));
    let adopted = restarted
        .restore_protections(&[btc()], T0)
        .await
        .expect("restore");

    assert_eq!(adopted, 1);
    assert_eq!(restarted.protection_count_for_test(), 1);
    assert_eq!(
        restarted.recorded_trigger_for_test(&btc()),
        Some(LONG_TRIGGER),
        "the restored protection must carry the trigger the stop actually rests at"
    );
    assert!(
        kinds_for(&journal, &btc())
            .await
            .contains(&TradeEventKind::ProtectionRestored),
        "the restart itself must be visible in the audit trail"
    );
}

#[tokio::test]
async fn a_restored_position_still_moves_to_breakeven() {
    // Restoring the row is only half the goal — the trade must still be
    // MANAGED afterwards. Everything the breakeven pass reads (entry price,
    // 1R, the offset, the threshold) has to survive the round trip, and a
    // single dropped field would leave this silently doing nothing.
    let (journal, _d) = temp_journal().await;
    journal
        .upsert_protection(&journalled_protection())
        .await
        .expect("seed");

    let mock = Arc::new(
        MockExchange::new()
            .with_positions(vec![position()])
            .with_tickers(vec![ticker(dec!(105))]),
    );
    let mut restarted = engine_sharing(Arc::clone(&mock), Arc::clone(&journal));
    restarted
        .restore_protections(&[btc()], T0)
        .await
        .expect("restore");

    restarted
        .drive_breakeven_stops(&btc(), EXECUTION_TF, &candle_at_2r())
        .await
        .expect("breakeven pass");

    let amends = mock.amended_stops();
    assert_eq!(
        amends.len(),
        1,
        "a position adopted from the journal must still have its stop managed"
    );
    assert_eq!(amends[0].1, ENTRY, "the trigger must be the entry price");
    assert_eq!(
        amends[0].2,
        dec!(99),
        "the stop-limit offset must have survived the round trip"
    );
}

#[tokio::test]
async fn a_journal_row_with_no_open_position_is_dropped_not_adopted() {
    // The exchange is authoritative about what is open. Adopting a stale row
    // would have the engine managing — and amending the stop of — a position
    // that does not exist.
    let (journal, _d) = temp_journal().await;
    journal
        .upsert_protection(&journalled_protection())
        .await
        .expect("seed");

    let mock = Arc::new(MockExchange::new().with_positions(vec![]));
    let mut restarted = engine_sharing(mock, Arc::clone(&journal));
    let adopted = restarted
        .restore_protections(&[], T0)
        .await
        .expect("restore");

    assert_eq!(adopted, 0);
    assert_eq!(restarted.protection_count_for_test(), 0);
    assert!(
        journal.load_protections().await.expect("load").is_empty(),
        "the stale row must be deleted, not left to be reconsidered at the next restart"
    );
    assert!(
        kinds_for(&journal, &btc())
            .await
            .contains(&TradeEventKind::PositionClosed)
    );
}

#[tokio::test]
async fn a_closed_position_deletes_its_protection_row() {
    // The private feed's PositionClosed is the first thing to learn the stop
    // filled. Leaving the row behind would have the next restart adopt it.
    let (mut engine, _mock, journal, _d) = engine_with_recorded_stop().await;
    engine
        .drive_breakeven_stops(&btc(), EXECUTION_TF, &candle_at_2r())
        .await
        .expect("breakeven pass writes the row");
    assert_eq!(
        journal.load_protections().await.expect("load").len(),
        1,
        "setup: a row must exist to be deleted"
    );

    engine
        .on_account_event(&AccountEvent::PositionClosed { symbol: btc() })
        .await;

    assert!(
        journal.load_protections().await.expect("load").is_empty(),
        "a closed position must not leave a protection row behind"
    );
    assert!(
        kinds_for(&journal, &btc())
            .await
            .contains(&TradeEventKind::PositionClosed)
    );
}

#[tokio::test]
async fn pruning_a_position_the_exchange_no_longer_reports_deletes_its_row() {
    // The second line of defence: a PositionClosed message can be missed (a
    // dropped private-feed frame), and `drive_stop_escalation` prunes against
    // the exchange's own current truth. The journal must be pruned with it.
    let (journal, _d) = temp_journal().await;
    journal
        .upsert_protection(&journalled_protection())
        .await
        .expect("seed");

    // The exchange reports nothing open, while the engine still holds the
    // protection in memory.
    let mock = Arc::new(MockExchange::new().with_positions(vec![]));
    let mut engine = engine_sharing(mock, Arc::clone(&journal));
    engine.protect_with_breakeven_for_test(
        btc(),
        Side::Buy,
        ENTRY,
        LONG_TRIGGER,
        LONG_STOP_LIMIT,
        ATR,
        Some(dec!(2)),
    );

    engine.drive_stop_escalation(T0).await.expect("ladder tick");

    assert_eq!(engine.protection_count_for_test(), 0);
    assert!(
        journal.load_protections().await.expect("load").is_empty(),
        "pruning the in-memory map must prune the journal too"
    );
}

#[tokio::test]
async fn a_journal_write_failure_does_not_stop_the_engine_managing_the_position() {
    // Losing an audit row is bad; refusing to move a stop to breakeven because
    // a write failed is worse. Nothing on this path may reach the caller's
    // error type.
    let (mut engine, mock, journal, _d) = engine_with_recorded_stop().await;
    journal
        .fail_writes_for_test()
        .await
        .expect("break the journal's writes");

    engine
        .drive_breakeven_stops(&btc(), EXECUTION_TF, &candle_at_2r())
        .await
        .expect("a journal failure must not propagate");

    let amends = mock.amended_stops();
    assert_eq!(amends.len(), 1, "the stop still moved");
    assert_eq!(amends[0].1, ENTRY);
    assert_eq!(
        engine.recorded_trigger_for_test(&btc()),
        Some(ENTRY),
        "the in-memory state must still reflect the move the exchange accepted"
    );
}

#[tokio::test]
async fn a_journal_write_failure_does_not_stop_a_position_being_closed() {
    // The same rule on the delete path: a failed delete must not stop the
    // engine forgetting a position that is gone.
    let (mut engine, _mock, journal, _d) = engine_with_recorded_stop().await;
    journal
        .fail_writes_for_test()
        .await
        .expect("break the journal's writes");

    engine
        .on_account_event(&AccountEvent::PositionClosed { symbol: btc() })
        .await;

    assert_eq!(
        engine.protection_count_for_test(),
        0,
        "the in-memory map must still forget the closed position"
    );
}

#[tokio::test]
async fn the_trade_event_log_records_the_whole_lifecycle() {
    // A drawn-out trade over one restart: placed, filled, breakeven, closed.
    // Each step is written where it happens, so the log reads as the trade's
    // history rather than a snapshot of its end state.
    let (journal, _d) = temp_journal().await;
    journal
        .upsert_protection(&journalled_protection())
        .await
        .expect("seed");

    let mock = Arc::new(
        MockExchange::new()
            .with_positions(vec![position()])
            .with_tickers(vec![ticker(dec!(105))]),
    );
    let mut engine = engine_sharing(Arc::clone(&mock), Arc::clone(&journal));
    engine
        .restore_protections(&[btc()], T0)
        .await
        .expect("restore");
    engine
        .on_account_event(&AccountEvent::OrderUpdate(OpenOrder {
            symbol: btc(),
            order_id: "oid-1".into(),
            order_link_id: "entry-1".into(),
            side: Side::Buy,
            price: ENTRY,
            qty: dec!(1),
            cum_exec_qty: dec!(1),
            state: OrderState::Filled,
            created_time_ms: T0,
            updated_time_ms: T0,
        }))
        .await;
    engine
        .drive_breakeven_stops(&btc(), EXECUTION_TF, &candle_at_2r())
        .await
        .expect("breakeven pass");
    engine
        .on_account_event(&AccountEvent::PositionClosed { symbol: btc() })
        .await;

    let kinds = kinds_for(&journal, &btc()).await;
    assert_eq!(
        kinds,
        vec![
            TradeEventKind::ProtectionRestored,
            TradeEventKind::EntryFilled,
            TradeEventKind::StopMovedToBreakeven,
            TradeEventKind::PositionClosed,
        ],
        "the log must read chronologically, one row per thing that happened"
    );
}

/// The engineered long setup from `engine_loop_behaviour.rs`'s
/// `a_zero_equity_account_refuses_rather_than_placing`, driven against a
/// FUNDED account so it reaches `place_entry` instead of a refusal: a rising
/// 4h bias, a 1h pullback into EMA20, then an RSI cross back through the long
/// trigger. Nothing shorter reaches the production insert site, and that site
/// is the one the whole feature hangs off.
async fn drive_an_engineered_long(engine: &mut EngineLoop) -> Option<String> {
    let sym = btc();
    let seed: Vec<Candle> = (-249..=0)
        .map(|i| candle(i * H1, dec!(101), dec!(99), dec!(100)))
        .collect();
    engine.warm(&sym, Timeframe::H1, seed.clone());
    engine.warm(&sym, Timeframe::H4, seed);

    const RAMP: i64 = 1040;
    const DIP: i64 = 14;
    let mut px = Decimal::from(100);
    let mut px_h4 = Decimal::from(100);
    let mut placed = None;

    for i in 1..=(RAMP + DIP + 1) {
        let t = i * H1;
        let h1 = if i <= RAMP {
            let c = candle(t, px + dec!(1), px - dec!(1), px);
            px += dec!(0.5);
            c
        } else if i <= RAMP + DIP {
            px -= dec!(0.5);
            candle(t, px + dec!(1), px - dec!(1), px)
        } else {
            let close = px + dec!(5);
            candle(t, close + dec!(1), close - dec!(2.5), close)
        };

        if let CandleOutcome::Placed { link_id } = engine
            .on_candle_closed(&sym, Timeframe::H1, &h1)
            .await
            .expect("handled")
        {
            placed = Some(link_id);
        }

        if i % 4 == 0 {
            let h4 = candle(t, px_h4 + dec!(1), px_h4 - dec!(1), px_h4);
            px_h4 += dec!(0.5);
            engine
                .on_candle_closed(&sym, Timeframe::H4, &h4)
                .await
                .expect("handled");
        }
    }
    placed
}

#[tokio::test]
async fn a_placed_entry_writes_its_protection_and_its_events_to_the_journal() {
    // The production insert site, reached the only way it can be: through the
    // strategy and the risk layer to a real `place_entry`. Seeding a
    // protection with a test helper instead would prove nothing about whether
    // the engine writes one when it actually opens a trade.
    //
    // `available` deliberately exceeds `equity`: `RiskManager` refuses when the
    // whole NOTIONAL exceeds available margin (1x, leverage being the
    // exchange's own margin-efficiency setting rather than something this layer
    // models), and a 1% risk over this setup's stop distance puts the notional
    // at roughly 1.5x equity. A tighter fixture would have this test observe a
    // margin refusal instead of the write-through it exists to check.
    let mock = Arc::new(MockExchange::new().with_balance(Balance {
        equity: dec!(10_000),
        available: dec!(60_000),
    }));
    let (journal, _d) = temp_journal().await;
    let mut engine = engine_sharing(Arc::clone(&mock), Arc::clone(&journal));

    let link_id = drive_an_engineered_long(&mut engine)
        .await
        .expect("the engineered setup must place an entry");

    let saved = journal.load_protections().await.expect("load");
    assert_eq!(saved.len(), 1, "a placed entry must record its protection");
    assert_eq!(saved[0].symbol, btc());
    assert_eq!(
        saved[0].order_link_id, link_id,
        "the row must name the entry order, or nothing correlates it with the audit trail"
    );
    assert!(
        !saved[0].moved_to_breakeven,
        "a stop that has not moved must not be recorded as moved"
    );
    assert!(
        saved[0].initial_risk > Decimal::ZERO,
        "1R must be recorded, or a restored position can never arm its breakeven"
    );

    let kinds = kinds_for(&journal, &btc()).await;
    assert!(
        kinds.contains(&TradeEventKind::EntryPlaced),
        "got {kinds:?}"
    );
    assert!(kinds.contains(&TradeEventKind::StopPlaced), "got {kinds:?}");
}

fn main_rs() -> PathBuf {
    // CARGO_MANIFEST_DIR for the `bot` crate is the workspace root's `bot/`.
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src/main.rs")
}

#[test]
fn startup_restores_protections_after_reconciling_and_before_the_loop() {
    // Guards the class of bug this codebase has shipped three times — a
    // feature implemented, unit-tested, and never actually invoked (299bf40,
    // 3a725d5, 4ce2f44). A source-level check because the ordering is the
    // contract: `reconcile` must run first so the exchange decides what is
    // open, and both must run before the event loop starts, or the first
    // candle would be processed with an empty protection map.
    //
    // Line comments are stripped, so commenting the call out fails this rather
    // than silently satisfying it.
    let text = std::fs::read_to_string(main_rs()).expect("bot/src/main.rs reads");
    let code: Vec<(usize, &str)> = text
        .lines()
        .enumerate()
        .filter(|(_, line)| !line.trim_start().starts_with("//"))
        .collect();
    let line_of = |needle: &str| {
        code.iter()
            .find(|(_, line)| line.contains(needle))
            .map(|(i, _)| *i)
    };

    let reconcile = line_of("reconcile(").expect("main.rs must reconcile against the exchange");
    let restore = line_of("restore_protections(")
        .expect("main.rs must restore stop protections at startup, or a restart orphans the trade");
    // The FIRST `loop {` in the file is not necessarily the event loop —
    // helper functions above `main` may have their own (the startup retry
    // does). Anchor on the one that follows reconciliation.
    let event_loop = code
        .iter()
        .find(|(i, line)| *i > reconcile && line.contains("loop {"))
        .map(|(i, _)| *i)
        .expect("main.rs must run an event loop after reconciling");

    assert!(
        restore > reconcile,
        "restore_protections must run AFTER reconcile: the exchange decides which positions exist"
    );
    assert!(
        restore < event_loop,
        "restore_protections must run BEFORE the event loop, or the first candle sees no protections"
    );
}
