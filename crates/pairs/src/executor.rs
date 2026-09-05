use std::time::Duration;

use botcore::{Instrument, LimitLeg, OrderStatus, Side, Symbol};
use exchange::ExchangeClient;
use exchange::bybit::transport::ExchangeError;
use rust_decimal::Decimal;
use tracing::{error, info, warn};

use crate::pricing::{aggressive_limit_price, sized_qty};
use crate::settle::{Leg, LegReport, Settlement, settle};
use crate::signal::{PairParams, PairSide};

/// Top of book plus the trading constraints for one leg.
///
/// Passed in rather than fetched inside the executor so the supervisor can
/// fetch once per bar and share across pairs, and so tests can drive the
/// executor without a clock.
#[derive(Debug, Clone)]
pub struct LegQuote {
    pub instrument: Instrument,
    pub bid: Decimal,
    pub ask: Decimal,
    pub last: Decimal,
}

#[derive(Debug, Clone)]
pub struct ExecutorConfig {
    /// How far through the book an entry or exit leg is priced.
    pub ticks_through: Decimal,
    /// How long to wait for a leg to reach a terminal state before cancelling.
    pub fill_timeout: Duration,
    pub poll_interval: Duration,
    /// Ticks-through for each successive unwind attempt. Its length is the
    /// attempt limit; an empty ladder disables unwinding and is a config error.
    pub unwind_ladder: Vec<Decimal>,
}

#[derive(Debug, thiserror::Error)]
pub enum ExecutionError {
    #[error("exchange error: {0}")]
    Exchange(#[from] ExchangeError),

    /// Exposure could not be flattened within the ladder.
    ///
    /// The caller must halt this pair and alert a human. It must **not**
    /// retry blindly and must **not** fall back to a market order: the
    /// limit-only rule holds on the unwind path exactly as it does everywhere
    /// else, and the agreed mitigation for a limit that cannot fill is
    /// escalation plus a person.
    #[error("could not flatten {} stranded leg(s); halting", legs.len())]
    UnwindExhausted { legs: Vec<LegReport> },
}

/// A pair position that actually exists on the exchange.
#[derive(Debug, Clone)]
pub struct OpenedPair {
    pub side: PairSide,
    pub a: LegReport,
    pub b: LegReport,
    pub opened_at_ms: i64,
}

/// Which way each leg trades for a given spread side.
fn leg_sides(side: PairSide) -> (Side, Side) {
    match side {
        PairSide::LongSpread => (Side::Buy, Side::Sell),
        PairSide::ShortSpread => (Side::Sell, Side::Buy),
    }
}

/// Deterministic, bar-derived order id.
///
/// Derived from the bar rather than the wall clock so a retry within the same
/// bar reuses the id and Bybit deduplicates it. The Python used
/// `int(time.time())`, which minted a fresh id per retry and made a duplicate
/// position possible. The readable `slug` prefix is kept because the dashboard
/// attributes orders by it.
fn leg_link_id(params: &PairParams, tag: &str, bar_ms: i64) -> String {
    format!("{}-{tag}-{bar_ms}", params.slug())
}

/// The id for one rung of one leg's unwind ladder.
///
/// The leg **must** be part of the id. Bybit scopes `orderLinkId` uniqueness to
/// the account, not the symbol, so an id built from the tag and rung alone
/// collides between leg A and leg B whenever both are being unwound — the
/// lopsided-fill case. The exchange then rejects the second leg's order as a
/// duplicate, `order_by_link_id` answers with the *first* leg's filled order,
/// and the ladder concludes it flattened a leg it never touched. That is a
/// silent success with live exposure still on the book, which is the exact
/// failure this module exists to make impossible.
fn unwind_link_id(params: &PairParams, tag: &str, leg: Leg, attempt: usize, bar_ms: i64) -> String {
    let leg_tag = match leg {
        Leg::A => "a",
        Leg::B => "b",
    };
    leg_link_id(params, &format!("{tag}{leg_tag}{attempt}"), bar_ms)
}

/// Open both legs, or leave the account flat.
///
/// Returns `Ok(Some(_))` only when both legs filled in full. Every other path
/// either provably flattens whatever executed or returns
/// [`ExecutionError::UnwindExhausted`]. There is no third outcome, and that
/// totality is the point of this function.
pub async fn open_pair(
    client: &dyn ExchangeClient,
    cfg: &ExecutorConfig,
    params: &PairParams,
    quotes: (&LegQuote, &LegQuote),
    side: PairSide,
    notional: Decimal,
    bar_ms: i64,
) -> Result<Option<OpenedPair>, ExecutionError> {
    let (aq, bq) = quotes;
    let (a_side, b_side) = leg_sides(side);

    let a_price = aggressive_limit_price(
        a_side,
        aq.bid,
        aq.ask,
        aq.instrument.tick_size,
        cfg.ticks_through,
    );
    let b_price = aggressive_limit_price(
        b_side,
        bq.bid,
        bq.ask,
        bq.instrument.tick_size,
        cfg.ticks_through,
    );
    let a_qty = sized_qty(notional, aq.last, &aq.instrument);
    let b_qty = sized_qty(notional, bq.last, &bq.instrument);

    let a_leg = LimitLeg {
        symbol: params.leg_a.clone(),
        side: a_side,
        qty: a_qty,
        price: a_price,
        order_link_id: leg_link_id(params, "a", bar_ms),
        reduce_only: false,
    };
    let b_leg = LimitLeg {
        symbol: params.leg_b.clone(),
        side: b_side,
        qty: b_qty,
        price: b_price,
        order_link_id: leg_link_id(params, "b", bar_ms),
        reduce_only: false,
    };

    info!(
        pair = %params.display_pair(),
        side = side.as_str(),
        notional = %notional,
        a_qty = %a_qty,
        b_qty = %b_qty,
        "placing pair entry"
    );

    // Both legs go out together: sequencing them widens the window in which
    // the spread can move between fills, and it would not remove the unwind
    // path anyway (a second leg can be rejected regardless of ordering).
    // `join!` rather than `try_join!` because a failure on one leg must not
    // abandon the other — the whole problem is knowing what happened to both.
    let (a_res, b_res) = tokio::join!(
        client.place_limit_leg(a_leg.clone()),
        client.place_limit_leg(b_leg.clone())
    );
    if let Err(e) = &a_res {
        warn!(leg = "a", error = %e, "leg placement returned an error; querying for the truth");
    }
    if let Err(e) = &b_res {
        warn!(leg = "b", error = %e, "leg placement returned an error; querying for the truth");
    }

    // The placement result is never trusted. A call that returned `Err` may
    // still have reached the exchange, so the exchange is asked what exists.
    let (a_report, b_report) = tokio::join!(
        resolve_leg(client, cfg, Leg::A, &a_leg),
        resolve_leg(client, cfg, Leg::B, &b_leg)
    );
    let a_report = a_report?;
    let b_report = b_report?;

    match settle(a_report, b_report) {
        Settlement::Opened { a, b } => {
            info!(
                pair = %params.display_pair(),
                a_price = %a.avg_price,
                b_price = %b.avg_price,
                "pair entry filled"
            );
            Ok(Some(OpenedPair {
                side,
                a,
                b,
                opened_at_ms: bar_ms,
            }))
        }
        Settlement::Flat => {
            info!(pair = %params.display_pair(), "pair entry did not fill; account is flat");
            Ok(None)
        }
        Settlement::Unwind(legs) => {
            warn!(
                pair = %params.display_pair(),
                legs = legs.len(),
                "pair entry left exposure; unwinding"
            );
            unwind_legs(client, cfg, params, &legs, bar_ms, quotes, "uw").await?;
            Ok(None)
        }
    }
}

/// Poll one leg to a terminal state, cancelling it if it overruns, and report
/// what it actually executed.
async fn resolve_leg(
    client: &dyn ExchangeClient,
    cfg: &ExecutorConfig,
    leg: Leg,
    order: &LimitLeg,
) -> Result<LegReport, ExecutionError> {
    let status = poll_to_terminal(client, cfg, &order.symbol, &order.order_link_id).await?;

    let status = match status {
        Some(s) if s.state.is_terminal() => Some(s),
        Some(_) => {
            // Still working past the deadline. Cancel, then re-read: the
            // cancel races the fill, and only the re-read knows which won.
            client
                .cancel_order(&order.symbol, &order.order_link_id)
                .await?;
            client
                .order_by_link_id(&order.symbol, &order.order_link_id)
                .await?
        }
        None => None,
    };

    Ok(build_report(leg, order, status.as_ref()))
}

fn build_report(leg: Leg, order: &LimitLeg, status: Option<&OrderStatus>) -> LegReport {
    LegReport {
        leg,
        symbol: order.symbol.clone(),
        side: order.side,
        requested_qty: order.qty,
        executed_qty: status.map(|s| s.cum_exec_qty).unwrap_or(Decimal::ZERO),
        avg_price: status.map(|s| s.avg_price).unwrap_or(Decimal::ZERO),
        order_id: status.map(|s| s.order_id.clone()).unwrap_or_default(),
        state: status.map(|s| s.state),
    }
}

/// Poll until the order reaches a terminal state or `fill_timeout` elapses.
///
/// Returns the last status seen, terminal or not, so the caller can tell
/// "still working" from "never existed".
pub async fn poll_to_terminal(
    client: &dyn ExchangeClient,
    cfg: &ExecutorConfig,
    symbol: &Symbol,
    link_id: &str,
) -> Result<Option<OrderStatus>, ExecutionError> {
    let deadline = tokio::time::Instant::now() + cfg.fill_timeout;
    loop {
        let status = client.order_by_link_id(symbol, link_id).await?;
        if let Some(s) = &status
            && s.state.is_terminal()
        {
            return Ok(status);
        }
        if tokio::time::Instant::now() >= deadline {
            return Ok(status);
        }
        tokio::time::sleep(cfg.poll_interval).await;
    }
}

/// Flatten every listed leg with reduce-only limits, escalating the price
/// through the book on each attempt.
///
/// **There is deliberately no market-order fallback.** The limit-only rule
/// holds here exactly as everywhere else; when the ladder is exhausted this
/// returns [`ExecutionError::UnwindExhausted`] so a human is brought in.
///
/// `tag` distinguishes an entry unwind (`"uw"`) from a close (`"cl"`) in the
/// `orderLinkId`, so the two are attributable apart in Bybit's UI.
pub async fn unwind_legs(
    client: &dyn ExchangeClient,
    cfg: &ExecutorConfig,
    params: &PairParams,
    legs: &[LegReport],
    bar_ms: i64,
    quotes: (&LegQuote, &LegQuote),
    tag: &str,
) -> Result<(), ExecutionError> {
    let mut stranded = Vec::new();

    for leg in legs {
        let mut flattened = false;
        for (attempt, ticks) in cfg.unwind_ladder.iter().enumerate() {
            // Re-quote each attempt: a rung that failed did so because the
            // book moved, and pricing the next rung off a stale quote is how a
            // ladder walks in the wrong direction.
            let t = client.ticker(&leg.symbol).await?;
            let side = leg.side.opposite();
            // Tick size comes from the quote already in hand. Re-reading
            // `instruments()` here would pull all 800-odd linear symbols on a
            // path that is already going badly.
            let (aq, bq) = quotes;
            let tick = if leg.symbol == aq.instrument.symbol {
                aq.instrument.tick_size
            } else {
                bq.instrument.tick_size
            };
            let price = aggressive_limit_price(side, t.bid1, t.ask1, tick, *ticks);
            let link_id = unwind_link_id(params, tag, leg.leg, attempt, bar_ms);

            warn!(
                symbol = %leg.symbol,
                attempt,
                ticks_through = %ticks,
                price = %price,
                qty = %leg.executed_qty,
                "unwinding stranded leg with a reduce-only limit"
            );

            let order = LimitLeg {
                symbol: leg.symbol.clone(),
                side,
                qty: leg.executed_qty,
                price,
                order_link_id: link_id,
                reduce_only: true,
            };
            // An error here is not fatal on its own — the next rung may work,
            // and the query below establishes what actually happened.
            let _ = client.place_limit_leg(order.clone()).await;
            let report = resolve_leg(client, cfg, leg.leg, &order).await?;
            if report.is_fully_filled() {
                flattened = true;
                break;
            }
        }
        if !flattened {
            error!(
                symbol = %leg.symbol,
                qty = %leg.executed_qty,
                "unwind ladder exhausted; leg is still open and needs a human"
            );
            stranded.push(leg.clone());
        }
    }

    if stranded.is_empty() {
        Ok(())
    } else {
        Err(ExecutionError::UnwindExhausted { legs: stranded })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use botcore::Timeframe;
    use rust_decimal_macros::dec;

    fn params(leg_a: &str, leg_b: &str) -> PairParams {
        PairParams {
            leg_a: Symbol::new(leg_a),
            leg_b: Symbol::new(leg_b),
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

    #[test]
    fn a_long_spread_buys_leg_a_and_sells_leg_b_and_a_short_spread_is_its_mirror() {
        assert_eq!(leg_sides(PairSide::LongSpread), (Side::Buy, Side::Sell));
        assert_eq!(leg_sides(PairSide::ShortSpread), (Side::Sell, Side::Buy));
    }

    #[test]
    fn two_legs_unwinding_on_the_same_rung_never_share_an_order_link_id() {
        // Bybit scopes orderLinkId uniqueness to the account rather than the
        // symbol. Without the leg in the id, the lopsided-fill case sends both
        // legs' first rung under one id: the second is refused as a duplicate,
        // the follow-up query returns the *first* leg's filled order, and the
        // ladder reports a leg flattened that is still open.
        let p = params("AAVEUSDT", "ETHUSDT");
        let a = unwind_link_id(&p, "uw", Leg::A, 0, 1_700_000_000_000);
        let b = unwind_link_id(&p, "uw", Leg::B, 0, 1_700_000_000_000);
        assert_ne!(a, b);
        assert_eq!(a, "aave_eth-uwa0-1700000000000");
        assert_eq!(b, "aave_eth-uwb0-1700000000000");
    }

    #[test]
    fn each_rung_of_one_legs_ladder_gets_its_own_id() {
        // A rung that did not fill leaves a cancelled order behind under its
        // own id. Reusing that id for the next rung would have Bybit dedupe
        // the escalation away — the ladder would place one order and then
        // three no-ops.
        let p = params("AAVEUSDT", "ETHUSDT");
        let ids: Vec<String> = (0..3)
            .map(|n| unwind_link_id(&p, "uw", Leg::A, n, 1_700_000_000_000))
            .collect();
        assert_eq!(ids[0], "aave_eth-uwa0-1700000000000");
        assert_eq!(ids[1], "aave_eth-uwa1-1700000000000");
        assert_eq!(ids[2], "aave_eth-uwa2-1700000000000");
    }

    #[test]
    fn an_unwind_id_fits_bybits_thirty_six_character_limit_on_the_longest_real_pair() {
        // `1000PEPE`/`1000BONK` is the widest slug the live universe produces
        // and lands exactly on the limit; anything longer would be silently
        // rejected at placement time, on the unwind path of all places.
        let p = params("1000PEPEUSDT", "1000BONKUSDT");
        let id = unwind_link_id(&p, "uw", Leg::B, 2, 1_700_000_000_000);
        assert_eq!(id, "1000pepe_1000bonk-uwb2-1700000000000");
        assert_eq!(id.len(), 36);
    }

    #[test]
    fn an_entry_id_can_never_collide_with_an_unwind_id_for_the_same_bar() {
        // Same pair, same bar, same account: the entry and its own unwind must
        // be two distinguishable orders in Bybit's UI and in the dedupe table.
        let p = params("AAVEUSDT", "ETHUSDT");
        let entry = leg_link_id(&p, "a", 1_700_000_000_000);
        let unwind = unwind_link_id(&p, "uw", Leg::A, 0, 1_700_000_000_000);
        assert_ne!(entry, unwind);
    }
}
