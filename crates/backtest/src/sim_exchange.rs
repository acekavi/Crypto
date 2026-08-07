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
    Balance, Candle, Instrument, LimitEntry, OpenOrder, OrderAck, OrderState, Position, Side,
    Symbol, Timeframe,
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

        // Only fill a resting entry for this symbol if no position is open
        // for it: one-way mode has room for exactly one net position per
        // symbol (see the `positions` field comment above), and averaging a
        // second fill into an existing position is not something this task's
        // interface asks for.
        if !state.positions.contains_key(symbol) {
            let fillable_link_id = state
                .resting
                .values()
                .find(|entry| &entry.symbol == symbol)
                .filter(|entry| {
                    matches!(
                        limit_fill(entry.side, entry.price, candle),
                        crate::fills::FillOutcome::Filled { .. }
                    )
                })
                .map(|entry| entry.order_link_id.clone());

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

    async fn positions(&self) -> Result<Vec<Position>, ExchangeError> {
        let state = self.state.lock().expect("sim exchange lock");
        Ok(state
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
            .collect())
    }

    async fn open_orders(&self) -> Result<Vec<OpenOrder>, ExchangeError> {
        let state = self.state.lock().expect("sim exchange lock");
        Ok(state
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
            .collect())
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
