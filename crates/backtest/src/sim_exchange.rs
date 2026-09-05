//! `SimulatedExchange`: an `ExchangeClient` that settles orders against
//! historical candles instead of a live venue.
//!
//! This crate does not reimplement trading rules. The daily-entry cap, the
//! max-concurrent-position cap, sizing, and R:R all live in `RiskManager` and
//! are replayed through the same `EngineLoop` that trades live. What lives
//! here is only what an *exchange* does: rest orders, fill them against the
//! trade-through model, track positions and balance.

use std::collections::HashMap;
use std::sync::Mutex;

use async_trait::async_trait;
use botcore::{
    Balance, Candle, Instrument, LimitEntry, LimitLeg, OpenOrder, OrderAck, OrderState,
    OrderStatus, Position, Side, Symbol, Timeframe,
};
use exchange::ExchangeClient;
use exchange::bybit::transport::ExchangeError;
use exchange::bybit::wire::{FundingRate, Ticker};
use rust_decimal::Decimal;

use crate::costs::{CostModel, funding_charge};
use crate::fills::{ExitOutcome, exit_was_ambiguous, limit_fill, resolve_exit};

/// Why a position closed. `ClosedTrade` carries this rather than callers
/// re-deriving it from prices, since the pessimistic-stop rule already makes
/// that derivation non-obvious.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitReason {
    Stop,
    Target,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClosedTrade {
    pub symbol: Symbol,
    pub side: Side,
    pub qty: Decimal,
    pub entry_price: Decimal,
    pub exit_price: Decimal,
    pub entry_ms: i64,
    pub exit_ms: i64,
    pub gross_pnl: Decimal,
    /// Entry maker fee plus exit maker fee, never netted against `funding` or
    /// `gross_pnl` — costs must stay visible as their own line item.
    pub fees: Decimal,
    /// Positive means this trade PAID funding, matching `funding_charge`'s
    /// sign convention.
    pub funding: Decimal,
    pub net_pnl: Decimal,
    pub exit_reason: ExitReason,
    pub was_ambiguous: bool,
}

/// Order the candidates by which one price reaches FIRST: for a buy the
/// highest limit, for a sell the lowest. Ties break on `order_link_id`.
///
/// Pure and public so the rule can be tested on a plain slice. Testing it only
/// through `advance` meant testing it through a `HashMap`, whose iteration
/// order is random per process — the original bug produced 255 trades on one
/// run and 257 on the next, and a test driven through that map catches it only
/// about two times in three.
pub fn best_fillable<'a>(candidates: &mut Vec<&'a LimitEntry>) -> Option<&'a LimitEntry> {
    candidates.sort_by(|a, b| {
        let by_price = match a.side {
            Side::Buy => b.price.cmp(&a.price),
            Side::Sell => a.price.cmp(&b.price),
        };
        by_price.then_with(|| a.order_link_id.cmp(&b.order_link_id))
    });
    candidates.first().copied()
}

/// An open position plus the exit levels carried on the `LimitEntry` that
/// opened it. `LimitEntry` already has `stop_limit_price` and `take_profit`,
/// so no separate bookkeeping is needed to know where a position's exits sit.
#[derive(Debug, Clone)]
struct OpenPosition {
    side: Side,
    qty: Decimal,
    entry_price: Decimal,
    entry_ms: i64,
    stop_limit_price: Decimal,
    take_profit: Decimal,
    /// Funding charged so far, accrued candle by candle at the mark that
    /// applied when each period fell due rather than re-priced at exit.
    accrued_funding: Decimal,
    /// High-water mark of funding already settled. Each `advance` charges the
    /// half-open span `(last_funded_ms, candle.open_time_ms]`, and because
    /// candles are contiguous those spans tile the hold exactly — every
    /// funding timestamp falls in one of them, and none falls in two.
    last_funded_ms: i64,
    /// Original risk per unit, entry to stop-limit. Fixed at fill, so moving
    /// the stop later cannot change what 1R meant.
    initial_risk: Decimal,
    /// Pull the stop to entry once price has travelled this many R in favour,
    /// taken from the `LimitEntry` that opened the position and therefore from
    /// the strategy's own `Signal`. `None` leaves the stop where it was placed.
    ///
    /// Per position rather than per simulator so the backtester cannot hold an
    /// opinion the live engine does not share: the strategy is the only thing
    /// that says when a stop moves.
    ///
    /// The tradeoff is real and measured rather than assumed: it removes the
    /// loss on trades that go your way and then reverse, but converts what
    /// would have been winners into scratches whenever price retraces to entry
    /// before continuing.
    breakeven_at_r: Option<Decimal>,
    /// Whether the stop has already been pulled to breakeven.
    moved_to_breakeven: bool,
}

#[derive(Debug, Default)]
struct State {
    equity: Decimal,
    /// Resting entries keyed by `order_link_id` — the exchange's own
    /// idempotency key, and the natural key for `cancel_order`.
    resting: HashMap<String, LimitEntry>,
    /// Open positions keyed by symbol. Bybit's one-way position mode (see
    /// `amend_stop`'s `positionIdx: 0` on the live client) allows only one
    /// net position per symbol, which this mirrors directly.
    positions: HashMap<Symbol, OpenPosition>,
    closed: Vec<ClosedTrade>,
    /// Pair legs placed via `place_limit_leg`, keyed by `order_link_id` so
    /// `order_by_link_id` can answer without a separate position model — this
    /// crate does not track pair positions, only the fill each leg produced.
    legs: HashMap<String, OrderStatus>,
    /// The most recent candle `advance` has settled for each symbol.
    ///
    /// `place_limit_leg` checks a leg's fill against this rather than against
    /// nothing: by the time the replay driver could have decided to place a
    /// leg for a candle, `advance` has already been called with that same
    /// candle (see `replay.rs`, where `sim.advance` runs before
    /// `engine.on_candle_closed` for every tick), so this is already-observed
    /// data, not a future candle the placement call is peeking at.
    last_candle: HashMap<Symbol, Candle>,
}

/// An `ExchangeClient` over historical candles.
///
/// `ExchangeClient` methods take `&self`, so state must be reachable through
/// a shared reference exactly as `engine::mock::MockExchange` holds its
/// state: a single `Mutex` guarding everything that changes.
pub struct SimulatedExchange {
    instruments: Vec<Instrument>,
    costs: CostModel,
    state: Mutex<State>,
}

impl SimulatedExchange {
    pub fn new(starting_equity: Decimal, instruments: Vec<Instrument>, costs: CostModel) -> Self {
        SimulatedExchange {
            instruments,
            costs,
            state: Mutex::new(State {
                equity: starting_equity,
                resting: HashMap::new(),
                positions: HashMap::new(),
                closed: Vec::new(),
                legs: HashMap::new(),
                last_candle: HashMap::new(),
            }),
        }
    }

    /// The full history of trades closed so far, across every symbol —
    /// the same records `advance` has been returning per call, retained here
    /// for a caller (a run summary, say) that wants the whole run at once.
    pub fn closed_trades(&self) -> Vec<ClosedTrade> {
        self.state.lock().expect("sim exchange lock").closed.clone()
    }

    /// Settle one candle for `symbol`: resolve an open position's exit, then
    /// resolve any resting entry's fill, and return whatever closed on this
    /// bar.
    ///
    /// Exits are settled BEFORE entries. An entry that fills on this candle
    /// has not yet had a chance to trade further within the same bar — OHLC
    /// data gives no intra-candle path, so letting a same-bar entry also
    /// close would assume a path (fill, then reverse, then stop) the data
    /// cannot support. Checking the pre-existing position first, before any
    /// new position can be opened, is what keeps that assumption out.
    ///
    /// Funding is accrued incrementally rather than settled in one lump at
    /// exit: each call charges the timestamps falling in
    /// `(last_funded_ms, candle.open_time_ms]` at THIS candle's close, which
    /// is what the spec asks for and needs no price history. Pricing a whole
    /// hold at the exit mark instead would bias every trade the same
    /// direction — see the accrual block for why that is worse than noise.
    pub fn advance(
        &self,
        symbol: &Symbol,
        candle: &Candle,
        funding: &[FundingRate],
    ) -> Vec<ClosedTrade> {
        let mut state = self.state.lock().expect("sim exchange lock");
        let mut newly_closed = Vec::new();

        // Recorded unconditionally, before anything else, so `place_limit_leg`
        // always sees the latest candle this symbol has settled.
        state.last_candle.insert(symbol.clone(), candle.clone());

        // Accrue funding BEFORE resolving the exit, and at THIS candle's
        // close. Charging a whole hold at the exit price instead would bias
        // systematically rather than randomly: a winning long exits higher
        // than it entered, so every one of its funding periods would be
        // marked up, and the error grows with hold time — precisely the
        // regime a swing strategy holding for days operates in.
        if let Some(pos) = state.positions.get_mut(symbol) {
            let charge: Decimal = funding
                .iter()
                .filter(|r| {
                    pos.last_funded_ms < r.funding_time_ms
                        && r.funding_time_ms <= candle.open_time_ms
                })
                .map(|r| funding_charge(pos.side, pos.qty, candle.close, r.rate))
                .sum();
            pos.accrued_funding += charge;
            pos.last_funded_ms = candle.open_time_ms;
        }

        // Breakeven is applied only AFTER the exit is resolved for this
        // candle, never before. A candle that reaches 1R may also have traded
        // through the original stop, and OHLC cannot say which came first —
        // moving the stop up first would quietly convert a real loss into a
        // scratch, which is the same optimism the pessimistic exit rule
        // exists to refuse.
        if let Some(pos) = state.positions.get(symbol).cloned() {
            let outcome = resolve_exit(pos.side, pos.stop_limit_price, pos.take_profit, candle);
            let exit = match outcome {
                ExitOutcome::StillOpen => None,
                ExitOutcome::Stopped { price } => Some((price, ExitReason::Stop)),
                ExitOutcome::TargetHit { price } => Some((price, ExitReason::Target)),
            };

            if let Some((exit_price, exit_reason)) = exit {
                let was_ambiguous =
                    exit_was_ambiguous(pos.side, pos.stop_limit_price, pos.take_profit, candle);
                let gross_pnl = match pos.side {
                    Side::Buy => pos.qty * (exit_price - pos.entry_price),
                    Side::Sell => pos.qty * (pos.entry_price - exit_price),
                };
                let entry_fee = self.costs.maker_fee(pos.qty, pos.entry_price);
                let exit_fee = self.costs.maker_fee(pos.qty, exit_price);
                let fees = entry_fee + exit_fee;
                let exit_ms = candle.open_time_ms;
                // Already settled candle by candle above; nothing is re-priced
                // at exit.
                let funding_total = pos.accrued_funding;
                let net_pnl = gross_pnl - fees - funding_total;

                // The entry fee was already debited from equity when the
                // position opened (below), so only the remainder of net_pnl
                // is applied here — otherwise it would be double-charged.
                state.equity += gross_pnl - exit_fee - funding_total;
                state.positions.remove(symbol);

                let trade = ClosedTrade {
                    symbol: symbol.clone(),
                    side: pos.side,
                    qty: pos.qty,
                    entry_price: pos.entry_price,
                    exit_price,
                    entry_ms: pos.entry_ms,
                    exit_ms,
                    gross_pnl,
                    fees,
                    funding: funding_total,
                    net_pnl,
                    exit_reason,
                    was_ambiguous,
                };
                state.closed.push(trade.clone());
                newly_closed.push(trade);
            }
        }

        // Still open after this candle: consider pulling the stop to entry.
        if let Some(pos) = state.positions.get_mut(symbol)
            && let Some(threshold) = pos.breakeven_at_r
            && !pos.moved_to_breakeven
            && pos.initial_risk > Decimal::ZERO
        {
            let travelled = match pos.side {
                Side::Buy => candle.high - pos.entry_price,
                Side::Sell => pos.entry_price - candle.low,
            };
            if travelled >= pos.initial_risk * threshold {
                // Entry exactly, not entry-plus-a-tick: the point is to stop
                // losing on this trade, and claiming a profit that the fill
                // would not actually capture is how a backtest flatters
                // itself.
                pos.stop_limit_price = pos.entry_price;
                pos.moved_to_breakeven = true;
            }
        }

        // Only fill a resting entry for this symbol if no position is open
        // for it: one-way mode has room for exactly one net position per
        // symbol (see the `positions` field comment above), and averaging a
        // second fill into an existing position is not something this task's
        // interface asks for.
        if !state.positions.contains_key(symbol) {
            // PRICE PRIORITY, and deterministic. `resting` is a HashMap, so
            // iterating it to pick a fill made the choice depend on hash order
            // — with two resting orders on one symbol the same backtest
            // produced 255 trades on one run and 257 on the next. Every
            // comparison the gate makes assumes reproducibility, so that had
            // to go.
            //
            // Among the orders price actually traded through, the one it
            // reached FIRST fills: for a buy that is the highest limit, for a
            // sell the lowest. Ties break on `order_link_id`, which is
            // deterministic by construction, so the result never depends on
            // map ordering.
            let mut fillable: Vec<&LimitEntry> = state
                .resting
                .values()
                .filter(|entry| &entry.symbol == symbol)
                .filter(|entry| {
                    matches!(
                        limit_fill(entry.side, entry.price, candle),
                        crate::fills::FillOutcome::Filled { .. }
                    )
                })
                .collect();
            let fillable_link_id = best_fillable(&mut fillable).map(|e| e.order_link_id.clone());

            if let Some(link_id) = fillable_link_id {
                let entry = state.resting.remove(&link_id).expect("just matched");
                let entry_fee = self.costs.maker_fee(entry.qty, entry.price);
                state.equity -= entry_fee;
                state.positions.insert(
                    symbol.clone(),
                    OpenPosition {
                        side: entry.side,
                        qty: entry.qty,
                        entry_price: entry.price,
                        entry_ms: candle.open_time_ms,
                        stop_limit_price: entry.stop_limit_price,
                        take_profit: entry.take_profit,
                        initial_risk: (entry.price - entry.stop_limit_price).abs(),
                        breakeven_at_r: entry.breakeven_at_r,
                        moved_to_breakeven: false,
                        accrued_funding: Decimal::ZERO,
                        // Seeded at the entry candle's open so a funding
                        // timestamp at or before entry is never charged to a
                        // position that was not yet open for it.
                        last_funded_ms: candle.open_time_ms,
                    },
                );
            }
        }

        newly_closed
    }
}

#[async_trait]
impl ExchangeClient for SimulatedExchange {
    async fn instruments(&self) -> Result<Vec<Instrument>, ExchangeError> {
        Ok(self.instruments.clone())
    }

    // Neither is consulted by the chronological replay driver: candles and
    // funding come from `HistoryDb`, and price discovery happens through
    // `advance`, not a ticker poll. Returning empty rather than
    // `unimplemented!()` means an incidental call here logs a no-op instead
    // of panicking a whole backtest run.
    async fn tickers(&self) -> Result<Vec<Ticker>, ExchangeError> {
        Ok(Vec::new())
    }

    /// Errors rather than inventing a book. The simulator has no top of book
    /// to serve — its whole price model is the candle passed to `advance` —
    /// and returning a fabricated bid/ask would let a backtest fill against
    /// prices that never existed.
    async fn ticker(&self, symbol: &Symbol) -> Result<Ticker, ExchangeError> {
        Err(ExchangeError::Decode(format!(
            "the simulated exchange has no top of book for {symbol}"
        )))
    }

    async fn klines(
        &self,
        _symbol: &Symbol,
        _tf: Timeframe,
        _limit: u16,
    ) -> Result<Vec<Candle>, ExchangeError> {
        Ok(Vec::new())
    }

    async fn place_limit_entry(&self, req: LimitEntry) -> Result<OrderAck, ExchangeError> {
        let mut state = self.state.lock().expect("sim exchange lock");
        let ack = OrderAck {
            order_id: format!("sim-{}", req.order_link_id),
            order_link_id: req.order_link_id.clone(),
        };
        // Records a resting order only. Fills happen exclusively in
        // `advance`, when a candle arrives — an order that filled here, on
        // the same call that created it, would be look-ahead bias: it would
        // see and react to a candle it was placed inside of, rather than one
        // that arrived after.
        state.resting.insert(req.order_link_id.clone(), req);
        Ok(ack)
    }

    /// Decides the leg synchronously, unlike `place_limit_entry`'s resting
    /// order — but only fills it if the market actually traded through its
    /// price. `place_limit_entry` defers its trade-through check to a later
    /// `advance` call because filling on the same call that created the
    /// order would be look-ahead; a leg has no such risk to guard against
    /// because it is checked against `last_candle`, the candle `advance` most
    /// recently settled for this symbol — already-observed data, not one
    /// still ahead of the caller (see `last_candle`'s field comment).
    ///
    /// Reuses `limit_fill`, the exact rule a resting `LimitEntry` is filled
    /// against in `advance`, rather than a second copy of trade-through
    /// logic: a buy leg fills only if the candle traded strictly below its
    /// price, a sell leg only if it traded strictly above, and never at a
    /// price better than the leg's own. A leg the market never reached — or
    /// one placed before any candle has been seen for its symbol at all — is
    /// left resting as `OrderState::New` instead of being marked Filled; this
    /// simulator does not re-check a resting leg against later candles, so
    /// such a leg simply stays New for the rest of the run.
    ///
    /// A filled leg pays `maker_fee`, the only per-fill cost this crate's
    /// `CostModel` knows how to compute. That is an approximation, not a
    /// statement that a leg's cost is genuinely the same as an entry's: a
    /// leg is priced *through* the book by design, which is a taker fill on
    /// a real exchange, and Bybit's taker rate is higher than its maker rate.
    /// Charging `maker_fee` here understates the true cost of every leg.
    ///
    /// This does not touch `positions` — that map's shape (stop, target,
    /// breakeven) belongs to directional `LimitEntry` trades. A pair's own
    /// position/PnL bookkeeping is the executor's job, built on top of the
    /// `OrderStatus` this returns, not this exchange's.
    async fn place_limit_leg(&self, req: LimitLeg) -> Result<OrderAck, ExchangeError> {
        let mut state = self.state.lock().expect("sim exchange lock");
        let ack = OrderAck {
            order_id: format!("sim-{}", req.order_link_id),
            order_link_id: req.order_link_id.clone(),
        };

        let filled = state.last_candle.get(&req.symbol).is_some_and(|candle| {
            matches!(
                limit_fill(req.side, req.price, candle),
                crate::fills::FillOutcome::Filled { .. }
            )
        });

        let status = if filled {
            let fee = self.costs.maker_fee(req.qty, req.price);
            state.equity -= fee;
            OrderStatus {
                symbol: req.symbol,
                order_id: ack.order_id.clone(),
                order_link_id: req.order_link_id.clone(),
                side: req.side,
                state: OrderState::Filled,
                qty: req.qty,
                cum_exec_qty: req.qty,
                avg_price: req.price,
                // place_limit_leg takes no timestamp and this crate never
                // reads the wall clock (see `open_orders`'s created_time_ms),
                // so order age genuinely isn't known here.
                updated_time_ms: 0,
            }
        } else {
            OrderStatus {
                symbol: req.symbol,
                order_id: ack.order_id.clone(),
                order_link_id: req.order_link_id.clone(),
                side: req.side,
                state: OrderState::New,
                qty: req.qty,
                cum_exec_qty: Decimal::ZERO,
                avg_price: Decimal::ZERO,
                updated_time_ms: 0,
            }
        };
        state.legs.insert(req.order_link_id, status);
        Ok(ack)
    }

    /// Answers from the record `place_limit_leg` wrote: `Filled` if the
    /// market had already traded through the leg's price when it was placed,
    /// `New` if it had not (and, in this simulator, will never be re-checked
    /// against a later candle — see `place_limit_leg`'s doc comment), or
    /// `None` if this link id was never placed at all.
    async fn order_by_link_id(
        &self,
        _symbol: &Symbol,
        link_id: &str,
    ) -> Result<Option<OrderStatus>, ExchangeError> {
        let state = self.state.lock().expect("sim exchange lock");
        Ok(state.legs.get(link_id).cloned())
    }

    async fn amend_stop(
        &self,
        symbol: &Symbol,
        _trigger: Decimal,
        limit_price: Decimal,
    ) -> Result<(), ExchangeError> {
        // Mirrors the live client: `limit_price` is the actual stop-limit
        // level (`slLimitPrice` on the wire), which is what `resolve_exit`
        // compares candles against. A symbol with no open position has
        // nothing to amend; that is not an error here, matching the live
        // exchange's tolerance for a stale escalation tick.
        let mut state = self.state.lock().expect("sim exchange lock");
        if let Some(pos) = state.positions.get_mut(symbol) {
            pos.stop_limit_price = limit_price;
        }
        Ok(())
    }

    async fn cancel_order(&self, _symbol: &Symbol, link_id: &str) -> Result<(), ExchangeError> {
        let mut state = self.state.lock().expect("sim exchange lock");
        state.resting.remove(link_id);
        Ok(())
    }

    /// Ordered by symbol.
    ///
    /// Built from a `HashMap`, whose iteration order is random per process. A
    /// simulator that returns an arbitrary order is the same latent defect as
    /// the resting-order pick that made this backtester non-reproducible — a
    /// caller that ever becomes order-sensitive would inherit it silently, and
    /// the sort costs nothing at these sizes.
    async fn positions(&self) -> Result<Vec<Position>, ExchangeError> {
        let state = self.state.lock().expect("sim exchange lock");
        let mut out: Vec<Position> = state
            .positions
            .iter()
            .map(|(symbol, pos)| Position {
                symbol: symbol.clone(),
                side: pos.side,
                size: pos.qty,
                entry_price: pos.entry_price,
                // Liquidation is not modeled: the pessimistic exit rule
                // already resolves every candle's worst case through
                // `resolve_exit`, so there is no separate liquidation path
                // for a backtest to simulate.
                liq_price: None,
                // Not marked to market between candles — no ticker feed
                // drives replay. Realized PnL appears on `ClosedTrade` at
                // settle time instead.
                unrealized_pnl: Decimal::ZERO,
            })
            .collect();
        out.sort_by(|a, b| a.symbol.as_str().cmp(b.symbol.as_str()));
        Ok(out)
    }

    /// Ordered by `order_link_id`, for the same reason as `positions`.
    async fn open_orders(&self) -> Result<Vec<OpenOrder>, ExchangeError> {
        let state = self.state.lock().expect("sim exchange lock");
        let mut out: Vec<OpenOrder> = state
            .resting
            .values()
            .map(|entry| OpenOrder {
                symbol: entry.symbol.clone(),
                order_id: format!("sim-{}", entry.order_link_id),
                order_link_id: entry.order_link_id.clone(),
                side: entry.side,
                price: entry.price,
                qty: entry.qty,
                cum_exec_qty: Decimal::ZERO,
                state: OrderState::New,
                // place_limit_entry takes no timestamp, and this crate never
                // reads the wall clock, so order age genuinely isn't known
                // here. The replay driver drives time through `advance`'s
                // candle, not by polling open orders.
                created_time_ms: 0,
                updated_time_ms: 0,
            })
            .collect();
        out.sort_by(|a, b| a.order_link_id.cmp(&b.order_link_id));
        Ok(out)
    }

    async fn set_leverage(
        &self,
        _symbol: &Symbol,
        _leverage: Decimal,
    ) -> Result<(), ExchangeError> {
        // Leverage affects margin and liquidation, neither of which this
        // simulator models (see the `liq_price: None` comment above).
        Ok(())
    }

    async fn balance(&self) -> Result<Balance, ExchangeError> {
        let state = self.state.lock().expect("sim exchange lock");
        Ok(Balance {
            equity: state.equity,
            // Margin reservation against `available` is `RiskManager`'s
            // concern upstream of order placement; this simulator only
            // tracks realized equity movement, so `available` mirrors it.
            available: state.equity,
        })
    }
}
