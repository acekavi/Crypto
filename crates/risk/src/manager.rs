use botcore::money::{round_down_to_step, round_price_away_from_market};
use botcore::{Instrument, Side, Symbol};
use rust_decimal::Decimal;
use strategy::Signal;

use crate::limits::{AccountState, Refusal, check_entry_allowed, drawdown_breach};
use crate::sizing::{RiskParams, liquidation_is_safe, position_size};

/// A fully-sized order, ready for the executor to turn into a `LimitEntry`.
///
/// The executor adds only the deterministic `orderLinkId`, derived from
/// `symbol`, `signal_candle_open_ms` and `side`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderIntent {
    pub symbol: Symbol,
    pub side: Side,
    pub qty: Decimal,
    pub entry_price: Decimal,
    pub stop_price: Decimal,
    /// Limit price for the stop, placed BEYOND the trigger so it fills into
    /// the move rather than at its edge.
    pub stop_limit_price: Decimal,
    pub target_price: Decimal,
    pub atr: Decimal,
    pub signal_candle_open_ms: i64,
}

/// The outcome of evaluating one signal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Enter(OrderIntent),
    Refuse(Refusal),
}

/// The only component that computes a position size.
///
/// Every hard limit the owner set is enforced here, and the strategy cannot
/// reach around it — strategies emit prices, never quantities.
pub struct RiskManager {
    params: RiskParams,
    /// Fraction of ATR the stop-limit sits beyond its trigger.
    stop_limit_offset_atr: Decimal,
}

impl RiskManager {
    pub fn new(params: RiskParams, stop_limit_offset_atr: Decimal) -> Self {
        RiskManager {
            params,
            stop_limit_offset_atr,
        }
    }

    pub fn params(&self) -> &RiskParams {
        &self.params
    }

    /// Turn a signal into a sized intent, or a named refusal.
    ///
    /// `liq_price_estimate` is the liquidation price the position would have.
    /// `None` means the exchange reports no liquidation risk.
    ///
    /// Checks run cheapest-and-most-certain first: the persisted halt and the
    /// counting limits before any arithmetic, then drawdown, then sizing.
    pub fn evaluate(
        &self,
        signal: &Signal,
        state: &AccountState,
        instrument: &Instrument,
        liq_price_estimate: Option<Decimal>,
    ) -> Decision {
        if let Err(refusal) = check_entry_allowed(state, &self.params, &signal.symbol) {
            return Decision::Refuse(refusal);
        }

        // Measure drawdown directly rather than waiting for the engine to have
        // persisted a halt flag — the flag is written after this fires.
        if let Some(breach) = drawdown_breach(state, &self.params) {
            return Decision::Refuse(breach);
        }

        // Round prices to the instrument's tick before anything derives from
        // them, so the size matches the price actually transmitted.
        let entry =
            round_price_away_from_market(signal.entry_price, instrument.tick_size, signal.side);
        let stop = round_stop_to_tick(signal.stop_price, instrument.tick_size, signal.side);

        let stop_distance = (entry - stop).abs();
        if stop_distance <= Decimal::ZERO {
            return Decision::Refuse(Refusal::SizeTooSmall);
        }

        if !liquidation_is_safe(
            entry,
            stop,
            liq_price_estimate,
            self.params.liq_buffer_multiple,
        ) {
            return Decision::Refuse(Refusal::LiquidationTooClose {
                multiple: self.params.liq_buffer_multiple,
            });
        }

        let Some(qty) = position_size(
            state.equity,
            self.params.risk_pct,
            stop_distance,
            instrument.qty_step,
        ) else {
            return Decision::Refuse(Refusal::SizeTooSmall);
        };

        if !instrument.qty_is_valid(qty) {
            return Decision::Refuse(Refusal::BelowMinimumQty {
                notional: qty,
                minimum: instrument.min_order_qty,
            });
        }

        let notional = qty * entry;
        if notional > state.available {
            return Decision::Refuse(Refusal::InsufficientMargin {
                notional,
                available: state.available,
            });
        }

        // Target at the strategy's own R multiple, measured from the ROUNDED
        // entry so the realized reward matches what was sized.
        let reward = stop_distance * reward_multiple_from(signal);
        let target = match signal.side {
            Side::Buy => entry + reward,
            Side::Sell => entry - reward,
        };

        let offset = signal.atr * self.stop_limit_offset_atr;
        let stop_limit_price = match signal.side {
            Side::Buy => stop - offset,
            Side::Sell => stop + offset,
        };

        Decision::Enter(OrderIntent {
            symbol: signal.symbol.clone(),
            side: signal.side,
            qty,
            entry_price: entry,
            stop_price: stop,
            stop_limit_price: round_stop_to_tick(
                stop_limit_price,
                instrument.tick_size,
                signal.side,
            ),
            target_price: round_stop_to_tick(target, instrument.tick_size, signal.side.opposite()),
            atr: signal.atr,
            signal_candle_open_ms: signal.signal_candle_open_ms,
        })
    }
}

/// Recover the strategy's intended R multiple from the signal's own prices, so
/// a strategy configured for something other than 2R is honoured rather than
/// silently overridden.
fn reward_multiple_from(signal: &Signal) -> Decimal {
    signal.reward_multiple().unwrap_or(Decimal::from(2))
}

/// Round a protective price to a valid tick, conservatively for the side.
///
/// A long's stop rounds DOWN (further from entry, giving the trade more room);
/// a short's rounds UP. Rounding a stop toward entry would tighten it below
/// what was sized, so realized loss would exceed the budget.
fn round_stop_to_tick(price: Decimal, tick: Decimal, side: Side) -> Decimal {
    if tick.is_zero() {
        return price;
    }
    match side {
        Side::Buy => round_down_to_step(price, tick),
        Side::Sell => {
            let down = round_down_to_step(price, tick);
            if down == price { price } else { down + tick }
        }
    }
}
