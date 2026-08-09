use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use botcore::{Candle, ErrorClass, Instrument, OrderState, Side, Symbol, Timeframe};
use engine::{
    Acceptance, CandleStore, EscalationAction, EscalationLadder, Executor, JournalFacts,
    OrderTracker, RestingOrder, TrackerAction, TriggeredStop, assemble_account_state,
    next_escalation, utc_day_start_ms,
};
use exchange::ExchangeClient;
use exchange::bybit::transport::ExchangeError;
use exchange::bybit::ws_private::AccountEvent;
use persistence::{Journal, JournalError, ProtectionRecord, TradeEvent, TradeEventKind};
use risk::{Decision, Refusal, RiskManager};
use rust_decimal::Decimal;
use strategy::{MarketContext, Strategy};
use tracing::{error, info, warn};

/// The stop this engine remembers placing for an open position.
///
/// `Position`, as reported by the exchange, does not carry the stop it was
/// opened with — only size, entry price and liquidation price. This is
/// recorded locally the moment `place_entry` succeeds in `on_candle_closed`
/// and is the only source of truth `drive_stop_escalation` and
/// `drive_breakeven_stops` have for a position's trigger, side, ATR and
/// breakeven threshold.
#[derive(Debug, Clone, PartialEq, Eq)]
struct StopProtection {
    /// The entry order this position was opened by. Carried so a journal row,
    /// a trade event and an `orders` row can be correlated into one audit
    /// trail; the exchange never hands it back on a `Position`.
    order_link_id: String,
    side: Side,
    /// The stop's trigger price. Rewritten to the entry price once
    /// `drive_breakeven_stops` has actually moved the stop, so the escalation
    /// ladder measures from where the stop now sits rather than from where it
    /// was first placed.
    trigger: Decimal,
    atr: Decimal,
    /// Price the entry was placed at. The breakeven amend's trigger is this
    /// exactly — entry, not entry plus a tick: the point is to stop losing on
    /// the trade, not to claim a profit the fill would not capture.
    entry_price: Decimal,
    /// Entry to stop-limit distance, fixed when the entry was placed. 1R means
    /// the real worst case (see `RiskManager::evaluate`), and moving the stop
    /// later must not change what 1R meant.
    initial_risk: Decimal,
    /// How far BEYOND the trigger the stop-limit sits, taken from the prices
    /// risk already computed. A long's stop-limit is below its trigger, a
    /// short's above; getting that backwards is the sizing bug fixed in
    /// `2d19d2d`, so the breakeven amend reuses the same offset rather than
    /// recomputing it.
    stop_limit_offset: Decimal,
    /// Pull the stop to entry once price has travelled this many R in favour.
    /// Carried verbatim from `Signal` through `OrderIntent`; `None` leaves the
    /// stop where it was placed. The strategy is the only thing that decides
    /// when a stop moves.
    breakeven_at_r: Option<Decimal>,
    /// Whether the stop has already been pulled to entry. Set only after the
    /// exchange has accepted the amend, so a failure retries next tick.
    moved_to_breakeven: bool,
}

impl StopProtection {
    /// The durable form of this record. Every field the engine needs to keep
    /// managing the position travels, because a restart rebuilds the whole
    /// protection from the row and nothing else can supply the parts
    /// `Position` does not carry.
    fn to_record(&self, symbol: &Symbol, updated_at_ms: i64) -> ProtectionRecord {
        ProtectionRecord {
            symbol: symbol.clone(),
            order_link_id: self.order_link_id.clone(),
            side: self.side,
            trigger: self.trigger,
            atr: self.atr,
            entry_price: self.entry_price,
            initial_risk: self.initial_risk,
            stop_limit_offset: self.stop_limit_offset,
            breakeven_at_r: self.breakeven_at_r,
            moved_to_breakeven: self.moved_to_breakeven,
            updated_at_ms,
        }
    }

    fn from_record(r: &ProtectionRecord) -> Self {
        StopProtection {
            order_link_id: r.order_link_id.clone(),
            side: r.side,
            trigger: r.trigger,
            atr: r.atr,
            entry_price: r.entry_price,
            initial_risk: r.initial_risk,
            stop_limit_offset: r.stop_limit_offset,
            breakeven_at_r: r.breakeven_at_r,
            moved_to_breakeven: r.moved_to_breakeven,
        }
    }
}

/// Why a candle produced no order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkipReason {
    /// Duplicate, out-of-order, or a gap — the stream did not advance.
    NotAccepted,
    NotWarm,
    Stale,
    NoSignal,
    /// No instrument metadata, so no valid order could be formed.
    UnknownInstrument,
}

/// What one closed candle led to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CandleOutcome {
    Skipped(SkipReason),
    Refused(Refusal),
    Placed { link_id: String },
}

/// Wires candle -> strategy -> risk -> executor.
///
/// Every gate is applied in order and each one is nameable, so a candle that
/// produces no trade always says why rather than disappearing silently.
pub struct EngineLoop {
    strategy: Box<dyn Strategy>,
    risk: RiskManager,
    client: Arc<dyn ExchangeClient>,
    journal: Arc<Journal>,
    /// SHA-256 of the effective config, stamped on every trade event so the
    /// audit trail says which ruleset produced it. `Executor` keeps its own
    /// copy for the same reason on `orders`.
    config_hash: String,
    executor: Executor,
    tracker: OrderTracker,
    store: CandleStore,
    instruments: HashMap<String, Instrument>,
    high_water_mark: Decimal,
    day_start_equity: Decimal,
    /// The UTC day `day_start_equity` was captured for. When the clock
    /// crosses into a new day the baseline is recaptured, otherwise the
    /// "daily" drawdown halt would keep measuring against first-startup
    /// equity and silently become a permanent-since-launch check.
    day_start_ms: i64,
    /// The stop recorded for each open position, keyed by symbol. Read by
    /// `drive_stop_escalation` to know a position's trigger since `Position`
    /// itself does not carry it.
    protections: HashMap<Symbol, StopProtection>,
    /// Stops observed past their trigger and not yet filled, keyed by
    /// symbol. Absence means either the stop has not triggered or it filled
    /// and closed the position.
    triggered_stops: HashMap<Symbol, TriggeredStop>,
    /// Symbols for which an exhausted-ladder halt has already been
    /// persisted, so `drive_stop_escalation` writes it once rather than on
    /// every tick the ladder stays exhausted.
    halted_for_exhaustion: HashSet<Symbol>,
    ladder: EscalationLadder,
    /// The most recent timestamp any input carried: a closed candle's open
    /// time, an escalation tick, an order update, or the startup clock.
    ///
    /// `AccountEvent::PositionClosed` carries no timestamp of its own, and the
    /// journal rows it writes must still be stamped with something meaningful.
    /// Reading the wall clock here instead would be wrong for the same reason
    /// `crates/backtest`'s `no_wall_clock` test forbids it there: "now" is
    /// whatever the data says it is. Advanced monotonically, so an out-of-order
    /// message cannot rewind the log.
    last_observed_ms: i64,
    /// Drawdown breaches observed. Under `HaltPolicy::Enforce` each of these
    /// also refused the entry; under `RecordOnly` (backtests) they were only
    /// counted, so a run can report how often its safety net would have
    /// engaged without the halt truncating the sample.
    halt_events: usize,
}

impl EngineLoop {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        strategy: Box<dyn Strategy>,
        risk: RiskManager,
        client: Arc<dyn ExchangeClient>,
        journal: Arc<Journal>,
        instruments: Vec<Instrument>,
        config_hash: String,
        entry_expiry_candles: u32,
        warmup_candles: usize,
    ) -> Self {
        let executor = Executor::new(
            Arc::clone(&client),
            Arc::clone(&journal),
            config_hash.clone(),
        );
        EngineLoop {
            strategy,
            risk,
            client,
            journal,
            config_hash,
            executor,
            tracker: OrderTracker::new(entry_expiry_candles),
            store: CandleStore::new(warmup_candles),
            instruments: instruments
                .into_iter()
                .map(|i| (i.symbol.as_str().to_string(), i))
                .collect(),
            high_water_mark: Decimal::ZERO,
            day_start_equity: Decimal::ZERO,
            day_start_ms: 0,
            protections: HashMap::new(),
            triggered_stops: HashMap::new(),
            halted_for_exhaustion: HashSet::new(),
            ladder: EscalationLadder::defaults(),
            last_observed_ms: 0,
            halt_events: 0,
        }
    }

    /// Advance the clock this engine stamps journal rows with.
    ///
    /// Monotonic: a private-feed message that arrives out of order must not
    /// make the audit trail travel backwards.
    fn observe_ms(&mut self, ms: i64) {
        self.last_observed_ms = self.last_observed_ms.max(ms);
    }

    /// Mirror a symbol's in-memory protection into the journal.
    ///
    /// Called at every site that mutates `protections`, so the table is
    /// authoritative at all times rather than a periodic snapshot.
    ///
    /// A failure is logged at `error!` and swallowed. Losing an audit row is
    /// bad; refusing to manage an open position because a write failed is
    /// worse, so nothing here is allowed to reach the caller's error type.
    async fn persist_protection(&self, symbol: &Symbol, at_ms: i64) {
        let Some(protection) = self.protections.get(symbol) else {
            return;
        };
        if let Err(e) = self
            .journal
            .upsert_protection(&protection.to_record(symbol, at_ms))
            .await
        {
            error!(
                %symbol,
                kind = e.kind(),
                "persisting a stop protection failed; the journal is now behind the engine"
            );
        }
    }

    /// Drop a symbol's protection row and record that its position is gone.
    ///
    /// Same failure contract as `persist_protection`: logged, never
    /// propagated.
    async fn forget_protection(&self, symbol: &Symbol, link_id: Option<&str>, detail: &str) {
        if let Err(e) = self.journal.delete_protection(symbol).await {
            error!(
                %symbol,
                kind = e.kind(),
                "deleting a stop protection failed; a stale row may be adopted at the next restart"
            );
        }
        self.record_event(
            symbol,
            link_id,
            TradeEventKind::PositionClosed,
            detail.to_string(),
        )
        .await;
    }

    /// Append one lifecycle event to the audit log.
    ///
    /// Stamped with `last_observed_ms` rather than the wall clock so a
    /// backtest or a replayed feed logs the time the data describes.
    ///
    /// Same failure contract as `persist_protection`.
    async fn record_event(
        &self,
        symbol: &Symbol,
        link_id: Option<&str>,
        kind: TradeEventKind,
        detail: String,
    ) {
        let event = TradeEvent {
            at_ms: self.last_observed_ms,
            symbol: symbol.clone(),
            order_link_id: link_id.map(str::to_string),
            kind,
            detail,
            config_hash: self.config_hash.clone(),
        };
        if let Err(e) = self.journal.record_event(&event).await {
            error!(
                %symbol,
                event = kind.as_str(),
                kind = e.kind(),
                "recording a trade event failed"
            );
        }
    }

    /// Rebuild the in-memory protection map from the journal, keeping only
    /// symbols the exchange currently reports as open.
    ///
    /// Call once at startup, immediately after `reconcile` and before any
    /// candle is processed. Without it a restart mid-trade orphans the
    /// position: no breakeven management, no escalation ladder, and a 1:5
    /// target can hold for days, so a restart mid-trade is expected rather
    /// than hypothetical.
    ///
    /// `open` is the exchange's own answer — `ReconcileReport::adopted_positions`
    /// is exactly the list `reconcile` just read from it. The exchange is
    /// authoritative about which positions exist and the journal supplies only
    /// what the exchange does not report, so a journal row with no matching
    /// open position is stale: it is deleted and recorded closed, never
    /// adopted. Letting the journal resurrect a position the exchange does not
    /// report would have the engine amending stops for something that is not
    /// there.
    ///
    /// Unlike the write-through paths, a failure to READ is surfaced rather
    /// than swallowed — the same reasoning as `load_baselines`. This runs
    /// before any candle, so there is no in-flight decision a loud failure
    /// could disrupt, and "the protections could not be loaded" is precisely
    /// what an operator must see before trading resumes rather than have
    /// silently treated as "there were none".
    ///
    /// Returns how many protections were adopted.
    pub async fn restore_protections(
        &mut self,
        open: &[Symbol],
        now_ms: i64,
    ) -> Result<usize, JournalError> {
        self.observe_ms(now_ms);
        let saved = self.journal.load_protections().await?;
        let open: HashSet<&str> = open.iter().map(Symbol::as_str).collect();

        let mut adopted = 0;
        for record in saved {
            if !open.contains(record.symbol.as_str()) {
                warn!(
                    symbol = %record.symbol,
                    "a journalled protection has no open position at the exchange; dropping it"
                );
                self.forget_protection(
                    &record.symbol,
                    Some(&record.order_link_id),
                    "no open position at the exchange on restart",
                )
                .await;
                continue;
            }

            info!(
                symbol = %record.symbol,
                trigger = %record.trigger,
                moved_to_breakeven = record.moved_to_breakeven,
                "restored a stop protection from the journal"
            );
            let symbol = record.symbol.clone();
            let link_id = record.order_link_id.clone();
            let detail = format!(
                "trigger {}, breakeven {}",
                record.trigger,
                if record.moved_to_breakeven {
                    "already at entry"
                } else {
                    "pending"
                }
            );
            self.protections
                .insert(symbol.clone(), StopProtection::from_record(&record));
            self.record_event(
                &symbol,
                Some(&link_id),
                TradeEventKind::ProtectionRestored,
                detail,
            )
            .await;
            adopted += 1;
        }
        Ok(adopted)
    }

    /// How many drawdown breaches this run observed.
    pub fn halt_events(&self) -> usize {
        self.halt_events
    }

    /// Seed the store AND the strategy's indicators after a restart or a gap
    /// backfill.
    ///
    /// Feeding the strategy matters as much as filling the store, and for a
    /// long time it did not happen. `Strategy::warmup_candles`'s own contract
    /// says "the engine warms indicators with this much history after a
    /// restart", but only the store was ever seeded. The store then reported
    /// warm immediately, every gate passed, and the strategy was called with
    /// cold EMAs — returning `None` until it had seen its own warm-up worth of
    /// live candles. For a 200-period EMA on 4h that is roughly five weeks of
    /// silence after every restart, logged as a healthy startup.
    ///
    /// Signals produced while warming are discarded: they describe setups that
    /// completed in the past, and acting on them would be trading history.
    pub fn warm(&mut self, symbol: &Symbol, tf: Timeframe, candles: Vec<Candle>) {
        if let Some(instrument) = self.instruments.get(symbol.as_str()).cloned() {
            for candle in &candles {
                let _ = self.strategy.on_candle_close(&MarketContext {
                    symbol,
                    timeframe: tf,
                    candle,
                    instrument: &instrument,
                });
            }
        } else {
            // Without instrument metadata no `MarketContext` can be built. The
            // store is still seeded so staleness and warmth stay correct, and
            // `on_candle_closed` already refuses this symbol with
            // `UnknownInstrument`, so nothing trades on a cold strategy.
            warn!(%symbol, ?tf, "no instrument metadata; strategy indicators not warmed");
        }
        self.store.warm(symbol, tf, candles);
    }

    /// Adopt a resting order found by reconciliation.
    pub fn track(&mut self, order: RestingOrder) {
        self.tracker.track(order);
    }

    /// Direct access to the order tracker.
    ///
    /// `engine::reconcile` takes `&mut OrderTracker` and adopts resting
    /// orders into it directly; exposing the tracker this engine already
    /// owns lets a restart's reconciliation land straight in the engine's
    /// own bookkeeping rather than going through a second tracker whose
    /// contents would then need copying over one by one.
    pub fn tracker_mut(&mut self) -> &mut OrderTracker {
        &mut self.tracker
    }

    /// Test-only alias so integration tests can seed a resting order.
    pub fn track_for_test(&mut self, order: RestingOrder) {
        self.track(order);
    }

    /// Apply an account-state change from the private feed.
    ///
    /// Without this the tracker never learns that an order filled: it would
    /// keep believing the entry is resting and, on expiry, try to cancel an
    /// order the exchange already executed.
    pub async fn on_account_event(&mut self, event: &AccountEvent) {
        match event {
            AccountEvent::OrderUpdate(order) => {
                self.observe_ms(order.updated_time_ms);
                self.tracker.on_order_update(order);
                // Mirror the state change into the journal. A journal failure
                // must never disturb trading, so it is logged, not propagated.
                //
                // `updated_time_ms`, not `created_time_ms`: the daily fill cap
                // counts at fill, and an order placed at 23:50 UTC that fills
                // at 00:05 must count against the day it filled, not the day
                // it was placed.
                let filled_at = matches!(
                    order.state,
                    OrderState::Filled | OrderState::PartiallyFilled
                )
                .then_some(order.updated_time_ms);
                if let Err(e) = self
                    .journal
                    .update_order_state(
                        &order.order_link_id,
                        order.state,
                        order.cum_exec_qty,
                        filled_at,
                    )
                    .await
                {
                    warn!(link_id = %order.order_link_id, error = %e, "journalling an order update failed");
                }
                // The audit log records the lifecycle, not just the current
                // state the row above overwrites: a fill that later closed is
                // otherwise indistinguishable from an entry that never filled.
                let kind = match order.state {
                    OrderState::Filled | OrderState::PartiallyFilled => {
                        Some(TradeEventKind::EntryFilled)
                    }
                    OrderState::Cancelled => Some(TradeEventKind::EntryCancelled),
                    OrderState::New | OrderState::Rejected => None,
                };
                if let Some(kind) = kind {
                    self.record_event(
                        &order.symbol,
                        Some(&order.order_link_id),
                        kind,
                        format!("{} of {} at {}", order.cum_exec_qty, order.qty, order.price),
                    )
                    .await;
                }
            }
            AccountEvent::PositionClosed { symbol } => {
                info!(%symbol, "position closed");
                // The stop filled (or the position closed some other way);
                // the ladder for it is done. Without this, a symbol that
                // trades repeatedly over a multi-month run would leave a
                // stale entry behind every time, growing both maps forever.
                let link_id = self
                    .protections
                    .get(symbol)
                    .map(|p| p.order_link_id.clone());
                self.protections.remove(symbol);
                self.triggered_stops.remove(symbol);
                self.halted_for_exhaustion.remove(symbol);
                // The journal row must go with the in-memory entry, or the
                // next restart would adopt a protection for a position that
                // has already closed.
                self.forget_protection(symbol, link_id.as_deref(), "the exchange reports size 0")
                    .await;
            }
            AccountEvent::PositionUpdate(_) | AccountEvent::WalletUpdate(_) => {}
        }
    }

    /// The timeframe the strategy signals and executes on: the finest it
    /// declares.
    ///
    /// Derived rather than named, exactly as `run_backtest` derives its
    /// `finest` and `main.rs` derives its entry-expiry timeframe. Hard-coding
    /// M15 here would silently stop matching the moment a variant executes on
    /// M5.
    fn execution_timeframe(&self) -> Option<Timeframe> {
        self.strategy
            .timeframes()
            .iter()
            .copied()
            .min_by_key(|tf| tf.duration_ms())
    }

    /// One closed candle, start to finish — the single seam `main.rs`'s candle
    /// arm calls.
    ///
    /// Breakeven runs BEFORE the strategy is evaluated, mirroring
    /// `run_backtest`, where `SimulatedExchange::advance` (which applies the
    /// breakeven rule) is called on the tick ahead of `on_candle_closed`. An
    /// entry placed off this candle must not be able to affect the stop of the
    /// position that was already open when it closed.
    ///
    /// This exists as a seam rather than two calls in `main.rs` so a test can
    /// prove the candle arm actually drives both. Twice now this codebase has
    /// shipped a pass that was implemented, unit-tested and never invoked —
    /// the escalation ladder (`299bf40`) and the drawdown halt (`3a725d5`).
    ///
    /// A breakeven failure must not cost the strategy this candle: dropping it
    /// would leave `CandleStore` seeing a gap at the next one. So anything but
    /// a `Fatal` error is logged and the candle still processed, matching how
    /// `on_candle_closed` itself treats a failed cancel.
    ///
    /// `run_backtest` deliberately does NOT call this — it calls
    /// `on_candle_closed` directly, because `SimulatedExchange::advance`
    /// already applies the identical breakeven rule to its own position record
    /// on the same candle. Driving both would apply it twice, and the
    /// simulator's version rests the stop at entry exactly while a real
    /// stop-limit's fill price sits an offset beyond its trigger.
    pub async fn drive_candle_close(
        &mut self,
        symbol: &Symbol,
        tf: Timeframe,
        candle: &Candle,
    ) -> Result<CandleOutcome, ExchangeError> {
        if let Err(e) = self.drive_breakeven_stops(symbol, tf, candle).await {
            if e.class() == ErrorClass::Fatal {
                return Err(e);
            }
            warn!(%symbol, error = %e, "the breakeven pass failed for this candle");
        }
        self.on_candle_closed(symbol, tf, candle).await
    }

    /// Pull `symbol`'s stop to entry if the candle that just closed travelled
    /// its strategy's breakeven multiple in favour.
    ///
    /// This is the live counterpart of the rule the simulator applies in
    /// `SimulatedExchange::advance`. Without it the live bot trades the
    /// backtested 1:5 target with no breakeven stop — measured at PF 1.312 and
    /// 21.3% max drawdown, which breaches the 20% total-drawdown halt, so the
    /// bot would halt itself.
    ///
    /// Distance travelled is measured against the CLOSED CANDLE's extreme —
    /// `high` for a long, `low` for a short — against `initial_risk` fixed at
    /// placement, which is precisely what the simulator reads. This used to
    /// sample the ticker's last price on the escalation timer, and a wick that
    /// crossed the threshold and retraced between two polls therefore moved
    /// the stop in a backtest and not live. A closed candle records that wick
    /// exactly, so the two now agree by construction.
    ///
    /// Only the execution timeframe drives it, for the same reason
    /// `run_backtest` settles only on `finest`: a structure candle spans price
    /// action the execution candles already covered.
    ///
    /// The amend is a plain amend of the resting stop-limit — never a market
    /// order — and a failure is logged rather than returned, so one symbol's
    /// bad amend cannot abort the candle it arrived on.
    pub async fn drive_breakeven_stops(
        &mut self,
        symbol: &Symbol,
        tf: Timeframe,
        candle: &Candle,
    ) -> Result<(), ExchangeError> {
        if self.execution_timeframe() != Some(tf) {
            return Ok(());
        }
        self.observe_ms(candle.open_time_ms);

        let Some(protection) = self.protections.get(symbol) else {
            return Ok(());
        };
        // No threshold means the strategy did not ask for a breakeven stop,
        // and this pass has no opinion of its own.
        let Some(threshold) = protection.breakeven_at_r else {
            return Ok(());
        };
        if protection.moved_to_breakeven || protection.initial_risk <= Decimal::ZERO {
            return Ok(());
        }

        let side = protection.side;
        let entry_price = protection.entry_price;
        let old_trigger = protection.trigger;
        let link_id = protection.order_link_id.clone();
        let travelled = match side {
            Side::Buy => candle.high - entry_price,
            Side::Sell => entry_price - candle.low,
        };
        if travelled < protection.initial_risk * threshold {
            return Ok(());
        }

        // Beyond the trigger, on the side price is moving through it: a long's
        // stop sells to close on the way down, so its limit sits below; a
        // short's sits above.
        let limit_price = match side {
            Side::Buy => entry_price - protection.stop_limit_offset,
            Side::Sell => entry_price + protection.stop_limit_offset,
        };

        // Last, because it is the only step that costs a round trip: a
        // protection whose position has already closed still sits in the map
        // until the escalation ladder prunes it, and amending its stop would
        // be rejected. The simulator has the same guard implicitly — it
        // applies breakeven only to a position still in `positions` after the
        // candle's exits resolved.
        let positions = self.client.positions().await?;
        if !positions
            .iter()
            .any(|p| p.symbol.as_str() == symbol.as_str())
        {
            return Ok(());
        }

        match self
            .client
            .amend_stop(symbol, entry_price, limit_price)
            .await
        {
            Ok(()) => {
                // Only now. A failed amend leaves the flag clear so the next
                // candle retries rather than silently skipping.
                if let Some(entry) = self.protections.get_mut(symbol) {
                    entry.moved_to_breakeven = true;
                    // The stop now rests at entry, so the escalation ladder
                    // must measure from there.
                    entry.trigger = entry_price;
                }
                info!(
                    %symbol,
                    trigger = %entry_price,
                    %limit_price,
                    "stop moved to entry; the trade can no longer lose"
                );
                // Durable before the next candle: an in-memory-only breakeven
                // flag would be lost by a restart, and the stop already resting
                // at entry would then be escalated from the wrong trigger.
                self.persist_protection(symbol, candle.open_time_ms).await;
                self.record_event(
                    symbol,
                    Some(&link_id),
                    TradeEventKind::StopMovedToBreakeven,
                    format!("stop {old_trigger} -> {entry_price} (entry), limit {limit_price}"),
                )
                .await;
            }
            Err(e) => {
                warn!(
                    %symbol,
                    error = %e,
                    "moving the stop to entry failed; retrying next candle"
                );
                // No protection write: the in-memory state did not change
                // either, so the journal is still correct. Only the attempt is
                // recorded.
                self.record_event(
                    symbol,
                    Some(&link_id),
                    TradeEventKind::StopAmendFailed,
                    format!("moving the stop to {entry_price} was rejected: {e}"),
                )
                .await;
            }
        }

        Ok(())
    }

    /// Advance the stop-escalation ladder for every open position that
    /// carries a recorded protection.
    ///
    /// Detection is by price, not by an exchange "stop triggered" event:
    /// `Position` carries no such signal, so a position still open with the
    /// last traded price through its own recorded trigger IS the definition
    /// of a stop that fired and did not fill — exactly the case the ladder
    /// exists for. A long's stop sells to close, so it triggers on the way
    /// down (`last_price <= trigger`); a short's stop buys to close, so it
    /// triggers on the way up (`last_price >= trigger`).
    ///
    /// One symbol failing (a bad amend, a missing ticker) must never stop
    /// the rest from being evaluated, so nothing here uses `?` inside the
    /// per-position loop — only the two batch fetches at the top can fail
    /// the whole call.
    pub async fn drive_stop_escalation(&mut self, now_ms: i64) -> Result<(), ExchangeError> {
        self.observe_ms(now_ms);
        let positions = self.client.positions().await?;
        let open: HashSet<Symbol> = positions.iter().map(|p| p.symbol.clone()).collect();

        // A review already flagged unbounded map growth in this codebase: a
        // symbol that closes (fill, manual close, liquidation) must not keep
        // its bookkeeping alive for the rest of the process's life. This is
        // a second line of defence alongside `on_account_event`'s
        // `PositionClosed` handler — that event can be missed (a dropped
        // private-feed message), while `positions()` here is the exchange's
        // own current truth.
        //
        // The journal rows go with them: a protection the exchange no longer
        // backs must not survive to be adopted by the next restart.
        let pruned: Vec<(Symbol, String)> = self
            .protections
            .iter()
            .filter(|(symbol, _)| !open.contains(*symbol))
            .map(|(symbol, p)| (symbol.clone(), p.order_link_id.clone()))
            .collect();
        self.protections.retain(|symbol, _| open.contains(symbol));
        self.triggered_stops
            .retain(|symbol, _| open.contains(symbol));
        self.halted_for_exhaustion
            .retain(|symbol| open.contains(symbol));
        for (symbol, link_id) in &pruned {
            self.forget_protection(
                symbol,
                Some(link_id),
                "the exchange no longer reports this position",
            )
            .await;
        }

        // Nothing can escalate without a recorded stop, and the ticker fetch is
        // by far the most expensive call here: `/v5/market/tickers` returns
        // every linear symbol — 812 on testnet, measured at 4.5-10s against a
        // 10s client timeout — while this loop needs prices for at most the
        // open positions. Bailing out when there is nothing to measure turns a
        // large request every ten seconds into none at all, and a timeout on it
        // used to abort the whole escalation pass.
        if !positions
            .iter()
            .any(|p| self.protections.contains_key(&p.symbol))
        {
            return Ok(());
        }

        let tickers = self.client.tickers().await?;
        let last_price: HashMap<&str, Decimal> = tickers
            .iter()
            .map(|t| (t.symbol.as_str(), t.last_price))
            .collect();

        for position in &positions {
            // Copied out rather than held as a borrow: the branches below take
            // `&self` (to write through to the journal) and `&mut` on other
            // fields, which a live borrow of `self.protections` would block.
            let Some((side, trigger, atr, link_id)) = self
                .protections
                .get(&position.symbol)
                .map(|p| (p.side, p.trigger, p.atr, p.order_link_id.clone()))
            else {
                continue;
            };
            let Some(&price) = last_price.get(position.symbol.as_str()) else {
                continue;
            };

            let triggered = match side {
                Side::Buy => price <= trigger,
                Side::Sell => price >= trigger,
            };
            if !triggered {
                continue;
            }

            match self.triggered_stops.get(&position.symbol).cloned() {
                None => {
                    // Rung 0 is already resting at its initial offset and
                    // deserves its own timeout before anything widens it, so
                    // this tick only starts tracking — it must not also act.
                    warn!(
                        symbol = %position.symbol,
                        %trigger,
                        "stop triggered without filling; starting the escalation ladder"
                    );
                    self.triggered_stops.insert(
                        position.symbol.clone(),
                        TriggeredStop {
                            symbol: position.symbol.clone(),
                            side,
                            trigger,
                            atr,
                            rung: 0,
                            rung_started_ms: now_ms,
                        },
                    );
                }
                Some(stop) => match next_escalation(&stop, &self.ladder, now_ms) {
                    EscalationAction::Wait => {}
                    EscalationAction::Widen { rung, limit_price } => {
                        match self
                            .executor
                            .widen_stop(&position.symbol, stop.trigger, limit_price)
                            .await
                        {
                            Ok(()) => {
                                if let Some(entry) = self.triggered_stops.get_mut(&position.symbol)
                                {
                                    entry.rung = rung;
                                    entry.rung_started_ms = now_ms;
                                }
                                // The trigger itself is unchanged — only the
                                // limit widened — but refreshing the row keeps
                                // `updated_at_ms` honest about when the engine
                                // last touched this position.
                                self.persist_protection(&position.symbol, now_ms).await;
                                self.record_event(
                                    &position.symbol,
                                    Some(&link_id),
                                    TradeEventKind::StopEscalated,
                                    format!(
                                        "rung {rung}: limit {limit_price} at trigger {}",
                                        stop.trigger
                                    ),
                                )
                                .await;
                            }
                            // Leave the rung and its start time UNCHANGED.
                            // Advancing here would skip a rung that never
                            // actually reached the exchange; the next tick
                            // retries this same widen.
                            Err(e) => {
                                warn!(
                                    symbol = %position.symbol,
                                    rung,
                                    error = %e,
                                    "widening a triggered stop failed; retrying the same rung next tick"
                                );
                            }
                        }
                    }
                    EscalationAction::Exhausted => {
                        error!(
                            symbol = %position.symbol,
                            "stop escalation ladder exhausted; no market order will be sent, \
                             halting new entries and leaving the position open for a human"
                        );
                        // Persist the halt once per symbol, not on every
                        // tick the ladder stays exhausted — `insert` returns
                        // true only the first time.
                        if self.halted_for_exhaustion.insert(position.symbol.clone()) {
                            let reason =
                                format!("stop escalation ladder exhausted for {}", position.symbol);
                            if let Err(e) = self.journal.set_halt(&reason, now_ms).await {
                                warn!(
                                    symbol = %position.symbol,
                                    kind = e.kind(),
                                    "persisting the escalation-exhausted halt failed"
                                );
                            }
                            self.record_event(
                                &position.symbol,
                                Some(&link_id),
                                TradeEventKind::StopLadderExhausted,
                                reason.clone(),
                            )
                            .await;
                            self.record_event(
                                &position.symbol,
                                None,
                                TradeEventKind::HaltSet,
                                reason,
                            )
                            .await;
                        }
                    }
                },
            }
        }

        Ok(())
    }

    /// Test-only: seed a protection record directly, bypassing the strategy
    /// and risk pipeline that normally produces one via a successful
    /// `place_entry` in `on_candle_closed`. Driving a full engineered signal
    /// through the pullback strategy purely to exercise the escalation
    /// ladder — which only cares that a protection record exists — would be
    /// disproportionate to what these tests check; see
    /// `account_state_for_test`, below, for the same rationale.
    ///
    /// Records no breakeven threshold, so a protection seeded this way is
    /// invisible to `drive_breakeven_stops` — see
    /// `protect_with_breakeven_for_test` for that.
    pub fn protect_for_test(&mut self, symbol: Symbol, side: Side, trigger: Decimal, atr: Decimal) {
        let order_link_id = format!("test-{symbol}");
        self.protections.insert(
            symbol,
            StopProtection {
                order_link_id,
                side,
                trigger,
                atr,
                entry_price: Decimal::ZERO,
                initial_risk: Decimal::ZERO,
                stop_limit_offset: Decimal::ZERO,
                breakeven_at_r: None,
                moved_to_breakeven: false,
            },
        );
    }

    /// Test-only: seed a protection record carrying the breakeven bookkeeping
    /// too, exactly as `on_candle_closed` derives it from a placed
    /// `OrderIntent`. Same rationale as `protect_for_test`: driving a full
    /// engineered signal through a strategy and the risk layer purely to
    /// reach an open position would be disproportionate to what the breakeven
    /// pass actually reads.
    #[allow(clippy::too_many_arguments)]
    pub fn protect_with_breakeven_for_test(
        &mut self,
        symbol: Symbol,
        side: Side,
        entry_price: Decimal,
        trigger: Decimal,
        stop_limit_price: Decimal,
        atr: Decimal,
        breakeven_at_r: Option<Decimal>,
    ) {
        let order_link_id = format!("test-{symbol}");
        self.protections.insert(
            symbol,
            StopProtection {
                order_link_id,
                side,
                trigger,
                atr,
                entry_price,
                initial_risk: (entry_price - stop_limit_price).abs(),
                stop_limit_offset: (trigger - stop_limit_price).abs(),
                breakeven_at_r,
                moved_to_breakeven: false,
            },
        );
    }

    /// Test-only: the trigger currently recorded for a symbol. Proves a
    /// successful breakeven amend rewrites it, so the escalation ladder
    /// measures from where the stop now rests.
    pub fn recorded_trigger_for_test(&self, symbol: &Symbol) -> Option<Decimal> {
        self.protections.get(symbol).map(|p| p.trigger)
    }

    /// Test-only: how many protection records are currently held. Proves
    /// `on_account_event`'s `PositionClosed` handler and
    /// `drive_stop_escalation`'s own pruning do not let this map grow
    /// unboundedly across a multi-month run. `#[cfg(test)]` cannot gate
    /// this — see `track_for_test`, above.
    pub fn protection_count_for_test(&self) -> usize {
        self.protections.len()
    }

    /// Test-only: how many triggered-stop records are currently held. Same
    /// rationale as `protection_count_for_test`.
    pub fn triggered_stop_count_for_test(&self) -> usize {
        self.triggered_stops.len()
    }

    /// Test-only: assemble account state directly, without needing a
    /// strategy signal to reach it. In production `account_state` only runs
    /// once a candle produces a signal; exercising the daily baseline
    /// rollover through the full candle-to-signal path would need an
    /// engineered setup on two different UTC days, which is disproportionate
    /// to what this is testing. `#[cfg(test)]` cannot gate this: an
    /// integration test under `bot/tests/` links against this crate as an
    /// ordinary dependency, not compiled with `--cfg test`, so a
    /// `#[cfg(test)]` item would not exist from its point of view — the same
    /// reason `track_for_test`, above, is a plain `pub fn`.
    pub async fn account_state_for_test(
        &mut self,
        now_ms: i64,
    ) -> Result<risk::AccountState, ExchangeError> {
        self.account_state(now_ms).await
    }

    /// Test-only: the UTC day the equity baseline was captured for, and the
    /// baseline itself.
    pub fn day_baseline_for_test(&self) -> (i64, Decimal) {
        (self.day_start_ms, self.day_start_equity)
    }

    /// Load the drawdown baselines the journal has persisted, so a restart
    /// does not silently reset them to whatever equity exists at the moment
    /// the process happens to come back up.
    ///
    /// Call once at startup, before any candle is processed. Unlike the
    /// per-candle journal reads in `account_state`, a failure here is
    /// surfaced rather than swallowed: this runs before any order has been
    /// considered, so there is no in-flight decision a loud failure could
    /// disrupt, and an unreadable baseline is exactly the kind of thing an
    /// operator must see before trading begins rather than have silently
    /// treated as "no baseline recorded yet".
    pub async fn load_baselines(&mut self, now_ms: i64) -> Result<(), Box<dyn std::error::Error>> {
        let balance = self.client.balance().await?;

        self.high_water_mark = self
            .journal
            .high_water_mark()
            .await?
            .unwrap_or(balance.equity);

        let day_start = utc_day_start_ms(now_ms);
        self.day_start_ms = day_start;
        self.day_start_equity = self
            .journal
            .day_start_equity(day_start)
            .await?
            .unwrap_or(balance.equity);

        Ok(())
    }

    /// Symbols that must not be dropped from the universe: they hold a
    /// position or a resting order, and losing their candles would leave the
    /// engine unable to manage them.
    pub async fn protected_symbols(&self) -> Result<HashSet<Symbol>, ExchangeError> {
        let mut set = self.tracker.resting_symbols();
        for p in self.client.positions().await? {
            set.insert(p.symbol);
        }
        Ok(set)
    }

    /// Drop candle streams for symbols no longer tracked.
    ///
    /// Called on each daily re-rank; without it the store accumulates a
    /// permanent entry for every symbol that ever entered the ranking.
    pub fn retain_symbols(&mut self, keep: &HashSet<Symbol>) -> usize {
        self.store.retain_symbols(keep)
    }

    /// Process one closed candle.
    pub async fn on_candle_closed(
        &mut self,
        symbol: &Symbol,
        tf: Timeframe,
        candle: &Candle,
    ) -> Result<CandleOutcome, ExchangeError> {
        self.observe_ms(candle.open_time_ms);
        let Some(instrument) = self.instruments.get(symbol.as_str()).cloned() else {
            return Ok(CandleOutcome::Skipped(SkipReason::UnknownInstrument));
        };

        // The stream advances only on Accepted. A gap must be backfilled by
        // the feed before the engine sees the next candle, so anything else
        // stops here.
        if self.store.accept(symbol, tf, candle) != Acceptance::Accepted {
            return Ok(CandleOutcome::Skipped(SkipReason::NotAccepted));
        }

        // Expire resting entries whose window has elapsed. A partial fill is
        // kept — only the remainder is cancelled.
        for action in self
            .tracker
            .on_candle_close(symbol, candle.open_time_ms, tf)
        {
            let TrackerAction::Expire {
                link_id,
                symbol: sym,
                filled,
            } = action;
            info!(%sym, %link_id, %filled, "entry expired; cancelling the remainder");
            if let Err(e) = self.executor.cancel(&sym, &link_id).await {
                // A revoked/invalid key (Fatal) means every subsequent call in
                // this process is doomed the same way, so it must halt rather
                // than be logged and skipped like an ordinary cancel failure —
                // mirrors the same class check the caller in main.rs applies
                // to this function's own Err.
                if e.class() == ErrorClass::Fatal {
                    return Err(e);
                }
                warn!(%link_id, error = %e, "cancelling an expired entry failed");
                continue;
            }
            self.record_event(
                &sym,
                Some(&link_id),
                TradeEventKind::EntryExpired,
                format!("cancelled the remainder; {filled} had filled"),
            )
            .await;
        }

        if !self.store.is_warm(symbol, tf) {
            return Ok(CandleOutcome::Skipped(SkipReason::NotWarm));
        }

        // Staleness must be checked across EVERY timeframe the strategy needs,
        // not just the one that arrived. A candle for this stream has, by
        // definition, just arrived, so checking only it can never fire. The
        // real hazard is a different stream going quiet — if the 4h bias feed
        // dies while 1h keeps flowing, the bot would trade on an outdated
        // trend filter and never notice. `to_vec()` sidesteps a simultaneous
        // borrow of `self.strategy` and `self.store` across the loop.
        let required_timeframes: Vec<Timeframe> = self.strategy.timeframes().to_vec();
        for required in required_timeframes {
            if self.store.is_stale(symbol, required, candle.open_time_ms) {
                return Ok(CandleOutcome::Skipped(SkipReason::Stale));
            }
        }

        let ctx = MarketContext {
            symbol,
            timeframe: tf,
            candle,
            instrument: &instrument,
        };
        let Some(signal) = self.strategy.on_candle_close(&ctx) else {
            return Ok(CandleOutcome::Skipped(SkipReason::NoSignal));
        };

        let state = self.account_state(candle.open_time_ms).await?;
        let liq = state
            .open_positions
            .iter()
            .find(|p| p.symbol.as_str() == symbol.as_str())
            .and_then(|p| p.liq_price);

        if self.risk.would_halt(&state).is_some() {
            self.halt_events += 1;
        }

        match self.risk.evaluate(&signal, &state, &instrument, liq) {
            Decision::Refuse(refusal) => {
                info!(%symbol, refusal = %refusal, "entry refused");
                // A drawdown breach must survive a restart, so it is
                // persisted to the journal here rather than living only in
                // the live `drawdown_breach` check that produced it. Every
                // other refusal (a daily cap, a size too small, ...) is a
                // normal, expected outcome and must not halt trading.
                if matches!(
                    refusal,
                    Refusal::DailyDrawdown { .. } | Refusal::TotalDrawdown { .. }
                ) {
                    let reason = refusal.to_string();
                    if let Err(e) = self.journal.set_halt(&reason, candle.open_time_ms).await {
                        warn!(%symbol, kind = e.kind(), "persisting the drawdown halt failed");
                    }
                    self.record_event(symbol, None, TradeEventKind::HaltSet, reason)
                        .await;
                }
                Ok(CandleOutcome::Refused(refusal))
            }
            Decision::Enter(intent) => {
                let resting = self.executor.place_entry(&intent).await?;
                let link_id = resting.link_id.clone();
                self.tracker.track(resting);
                // Record the stop this position was opened with — `?` above
                // means this only runs once the exchange has accepted the
                // order, and `Position` itself never carries the stop back,
                // so this is the only place it can be captured.
                //
                // `breakeven_at_r` rides along here because it is the only
                // record of the strategy's threshold that survives to the
                // open position: Bybit has no order type for "move the stop
                // at 2R", so `LimitEntry` never sends it and nothing comes
                // back from the exchange carrying it.
                self.protections.insert(
                    intent.symbol.clone(),
                    StopProtection {
                        order_link_id: link_id.clone(),
                        side: intent.side,
                        trigger: intent.stop_price,
                        atr: intent.atr,
                        entry_price: intent.entry_price,
                        initial_risk: (intent.entry_price - intent.stop_limit_price).abs(),
                        stop_limit_offset: (intent.stop_price - intent.stop_limit_price).abs(),
                        breakeven_at_r: intent.breakeven_at_r,
                        moved_to_breakeven: false,
                    },
                );
                info!(%symbol, %link_id, qty = %intent.qty, "entry placed");
                // Durable immediately, not at the next fill: the stop rides on
                // the entry order itself, so from this moment a restart that
                // found the position open would otherwise have no record of
                // where its stop sits or when it should move.
                self.persist_protection(&intent.symbol, candle.open_time_ms)
                    .await;
                self.record_event(
                    &intent.symbol,
                    Some(&link_id),
                    TradeEventKind::EntryPlaced,
                    format!("{} at {}", intent.qty, intent.entry_price),
                )
                .await;
                self.record_event(
                    &intent.symbol,
                    Some(&link_id),
                    TradeEventKind::StopPlaced,
                    format!(
                        "trigger {} limit {} target {}",
                        intent.stop_price, intent.stop_limit_price, intent.target_price
                    ),
                )
                .await;
                Ok(CandleOutcome::Placed { link_id })
            }
        }
    }

    /// Assemble the account state the risk layer reads.
    async fn account_state(&mut self, now_ms: i64) -> Result<risk::AccountState, ExchangeError> {
        let balance = self.client.balance().await?;
        let positions = self.client.positions().await?;

        // Record this observation on every evaluation, not just once at
        // startup — otherwise there is almost nothing for `high_water_mark`
        // and `day_start_equity` to read back after a restart. A write
        // failure is logged, never propagated: it must not block a
        // decision on this candle.
        if let Err(e) = self.journal.record_equity(balance.equity, now_ms).await {
            warn!(error = %e, "recording an equity snapshot failed");
        }

        // Recapture the baseline whenever the clock has crossed into a new
        // UTC day, not just once at process start — otherwise the "daily"
        // drawdown halt keeps measuring against whatever equity existed at
        // first startup, and silently degenerates into a permanent
        // since-launch check the longer the process stays up.
        let day_start = utc_day_start_ms(now_ms);
        if day_start != self.day_start_ms {
            self.day_start_ms = day_start;
            self.day_start_equity = balance.equity;
        }
        self.high_water_mark = engine::update_high_water_mark(self.high_water_mark, balance.equity);

        let entries_filled_today = self
            .journal
            .daily_fill_count(day_start)
            .await
            .unwrap_or_else(|e| {
                // Fails OPEN: an entry that never happened is a bounded,
                // reversible cost (at most one extra entry above the daily
                // cap, still governed by every other risk limit), and the
                // cap resets itself at the next UTC day regardless.
                warn!(error = %e, "reading today's fill count failed; treating as zero");
                0
            })
            .max(0) as u32;

        // Fails CLOSED, deliberately the opposite of the fill count above:
        // an unreadable halt flag is not evidence of no halt, and this is
        // the one flag a human — not a transient read error — is meant to
        // clear. Treating the read failure itself as a halt reason routes
        // through the ordinary `Refusal::Halted` path below, so the candle
        // still gets a named, logged outcome rather than an entry placed on
        // missing information. This does not violate "a journal failure
        // must never block an order": that rule guards against a WRITE
        // failure reversing or erroring out an order the exchange already
        // accepted (see `Executor::place_entry`); no order has been decided
        // yet here, so declining to enter is the risk gate working as
        // designed, not a failure blocking a decision already made.
        let halt_reason = match self.journal.halt_reason().await {
            Ok(reason) => reason,
            Err(e) => {
                warn!(error = %e, "reading the halt flag failed; refusing entries this candle");
                Some(format!("halt flag unreadable: {e}"))
            }
        };

        Ok(assemble_account_state(
            &balance,
            positions,
            JournalFacts {
                entries_filled_today,
                halt_reason,
                day_start_equity: self.day_start_equity,
                high_water_mark: self.high_water_mark,
            },
        ))
    }
}
