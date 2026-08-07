//! The trade-through fill model and pessimistic exit resolution.
//!
//! Pure functions, no I/O: these two rules are the foundation every other
//! backtest number rests on, so they are tested in isolation with
//! hand-computed expected values (see `tests/fills.rs`).

use botcore::{Candle, Side};
use rust_decimal::Decimal;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FillOutcome {
    NoFill,
    Filled { price: Decimal },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitOutcome {
    StillOpen,
    Stopped { price: Decimal },
    TargetHit { price: Decimal },
}

/// A resting limit fills only when price has traded *strictly* past it —
/// never on touch. The live bot posts at this price and sits behind the
/// queue at its own level; on a mere touch, the orders already resting there
/// absorb the volume and a latecomer does not fill. Fill price is always the
/// limit price, never better — the order does not fill at the candle's
/// extreme just because price reached further.
pub fn limit_fill(side: Side, limit_price: Decimal, candle: &Candle) -> FillOutcome {
    let traded_through = match side {
        Side::Buy => candle.low < limit_price,
        Side::Sell => candle.high > limit_price,
    };
    if traded_through {
        FillOutcome::Filled { price: limit_price }
    } else {
        FillOutcome::NoFill
    }
}

/// Whether a candle traded through the stop and/or the target for a position
/// on `position_side`. A long closes by selling (stop below, target above);
/// a short closes by buying (stop above, target below) — getting this
/// backwards silently inverts every short trade.
fn hits(
    position_side: Side,
    stop_limit: Decimal,
    target: Decimal,
    candle: &Candle,
) -> (bool, bool) {
    match position_side {
        Side::Buy => (candle.low < stop_limit, candle.high > target),
        Side::Sell => (candle.high > stop_limit, candle.low < target),
    }
}

/// Resolves how a candle affects an open position's stop/target. The stop is
/// checked first: OHLC data gives no intra-candle path, so when both the
/// stop and the target were reachable in the same candle, the true order is
/// unknowable and this always resolves as stopped. Any other choice inflates
/// results, and the inflation is largest on exactly the volatile candles
/// that dominate returns.
pub fn resolve_exit(
    position_side: Side,
    stop_limit: Decimal,
    target: Decimal,
    candle: &Candle,
) -> ExitOutcome {
    let (stop_hit, target_hit) = hits(position_side, stop_limit, target, candle);
    if stop_hit {
        ExitOutcome::Stopped { price: stop_limit }
    } else if target_hit {
        ExitOutcome::TargetHit { price: target }
    } else {
        ExitOutcome::StillOpen
    }
}

/// True when a single candle traded through both the stop and the target, so
/// the pessimistic resolution in `resolve_exit` stood in for an ordering the
/// data cannot actually determine. Reported so a run that leans heavily on
/// the assumption says so, rather than presenting it as certain.
pub fn exit_was_ambiguous(
    position_side: Side,
    stop_limit: Decimal,
    target: Decimal,
    candle: &Candle,
) -> bool {
    let (stop_hit, target_hit) = hits(position_side, stop_limit, target, candle);
    stop_hit && target_hit
}
