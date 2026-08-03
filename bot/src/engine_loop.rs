use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use botcore::{Candle, Instrument, Symbol, Timeframe};
use engine::{
    Acceptance, CandleStore, Executor, JournalFacts, OrderTracker, RestingOrder, TrackerAction,
    assemble_account_state, utc_day_start_ms,
};
use exchange::ExchangeClient;
use exchange::bybit::transport::ExchangeError;
use persistence::Journal;
use risk::{Decision, Refusal, RiskManager};
use rust_decimal::Decimal;
use strategy::{MarketContext, Strategy};
use tracing::{info, warn};

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
    executor: Executor,
    tracker: OrderTracker,
    store: CandleStore,
    instruments: HashMap<String, Instrument>,
    high_water_mark: Decimal,
    day_start_equity: Decimal,
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
        let executor = Executor::new(Arc::clone(&client), Arc::clone(&journal), config_hash);
        EngineLoop {
            strategy,
            risk,
            client,
            journal,
            executor,
            tracker: OrderTracker::new(entry_expiry_candles),
            store: CandleStore::new(warmup_candles),
            instruments: instruments
                .into_iter()
                .map(|i| (i.symbol.as_str().to_string(), i))
                .collect(),
            high_water_mark: Decimal::ZERO,
            day_start_equity: Decimal::ZERO,
        }
    }

    /// Seed the store after a restart or a gap backfill.
    pub fn warm(&mut self, symbol: &Symbol, tf: Timeframe, candles: Vec<Candle>) {
        self.store.warm(symbol, tf, candles);
    }

    /// Adopt a resting order found by reconciliation.
    pub fn track(&mut self, order: RestingOrder) {
        self.tracker.track(order);
    }

    /// Test-only alias so integration tests can seed a resting order.
    pub fn track_for_test(&mut self, order: RestingOrder) {
        self.track(order);
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
                warn!(%link_id, error = %e, "cancelling an expired entry failed");
            }
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

        match self.risk.evaluate(&signal, &state, &instrument, liq) {
            Decision::Refuse(refusal) => {
                info!(%symbol, refusal = %refusal, "entry refused");
                Ok(CandleOutcome::Refused(refusal))
            }
            Decision::Enter(intent) => {
                let resting = self.executor.place_entry(&intent).await?;
                let link_id = resting.link_id.clone();
                self.tracker.track(resting);
                info!(%symbol, %link_id, qty = %intent.qty, "entry placed");
                Ok(CandleOutcome::Placed { link_id })
            }
        }
    }

    /// Assemble the account state the risk layer reads.
    async fn account_state(&mut self, now_ms: i64) -> Result<risk::AccountState, ExchangeError> {
        let balance = self.client.balance().await?;
        let positions = self.client.positions().await?;

        if self.day_start_equity.is_zero() {
            self.day_start_equity = balance.equity;
        }
        self.high_water_mark = engine::update_high_water_mark(self.high_water_mark, balance.equity);

        let day_start = utc_day_start_ms(now_ms);
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
