use std::time::Duration;

use botcore::{Instrument, LimitLeg, OrderStatus, Side, Symbol};
use exchange::ExchangeClient;
use exchange::bybit::transport::ExchangeError;
use rust_decimal::Decimal;
use tracing::{debug, error, info, warn};

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
    /// Whether a two-leg entry that only partially fills gets flattened.
    ///
    /// `true` (the default, and the only behavior this module had before this
    /// field existed) is the safety property documented on [`settle`]: any
    /// fill short of both legs full gets unwound back to flat, because a
    /// lopsided pair runs directional risk a market-neutral strategy has no
    /// edge in.
    ///
    /// `false` disables that *only* for the case both legs actually observed
    /// — both sides partially filled, neither cancelled. The still-unfilled
    /// remainder on each leg is left resting (limit-only, GTC, already
    /// placed) rather than cancelled, and the position is opened at whatever
    /// is filled now; a later bar may see either leg's resting remainder fill
    /// further, but nothing here tops the journalled qty/avg-price up when it
    /// does — the exit path re-reads the true live position size when it
    /// closes regardless, so the position is still fully closed even if the
    /// journal is stale, but a stale qty in the meantime is a known
    /// imprecision, not a bug. A true one-sided fill (the other leg touched
    /// nothing at all) is a strictly worse case this flag does not affect:
    /// that is still unwound unconditionally, because riding a fully naked
    /// leg is a different and larger risk than riding an unequal pair.
    pub unwind_on_partial_fill: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum ExecutionError {
    #[error("exchange error: {0}")]
    Exchange(#[from] ExchangeError),
    #[error("journal error: {0}")]
    Journal(#[from] persistence::JournalError),

    /// Exposure could not be flattened within the ladder.
    ///
    /// The caller must halt this pair and alert a human. It must **not**
    /// retry blindly and must **not** fall back to a market order: the
    /// limit-only rule holds on the unwind path exactly as it does everywhere
    /// else, and the agreed mitigation for a limit that cannot fill is
    /// escalation plus a person.
    #[error("could not flatten {} stranded leg(s); halting", legs.len())]
    UnwindExhausted { legs: Vec<LegReport> },

    /// One leg's true state could not be established.
    ///
    /// Deliberately distinct from a bare [`ExecutionError::Exchange`], which
    /// names nothing: an operator woken by this needs to know which leg, which
    /// symbol and how much was at stake without going to read the code. Like
    /// `UnwindExhausted` it means halt-and-reconcile, never retry-and-forget —
    /// the leg may be filled, resting, or absent, and only the venue knows.
    #[error("leg {leg:?} ({symbol}) could not be resolved after requesting {qty}: {source}")]
    LegUnresolved {
        leg: Leg,
        symbol: Symbol,
        qty: Decimal,
        #[source]
        source: ExchangeError,
    },
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

/// Bybit's hard cap on `orderLinkId`.
const MAX_LINK_ID_LEN: usize = 36;

/// The widest suffix either id builder can append to a slug: `-`, a
/// two-character tag, a one-character leg, a two-digit rung, `-`, and a
/// 13-digit millisecond bar timestamp.
///
/// The rung is budgeted at two digits rather than at the ladder's current three
/// rungs, so lengthening `unwind_ladder` can never silently push an id over the
/// cap.
const MAX_LINK_ID_SUFFIX_LEN: usize = 1 + 2 + 1 + 2 + 1 + 13;

/// Longest pair slug that may appear in an `orderLinkId`; longer slugs are
/// truncated to it.
///
/// Truncation is not cosmetic. `1000FLOKIUSDT`/`1000BONKUSDT` — all live Bybit
/// linear perps — produce an 18-character slug, and an untruncated unwind id
/// built from it is 37 characters. The entry legs would place fine and then
/// *every* unwind rung would be rejected for an over-long id, disabling the
/// ladder at exactly the moment it is needed. Bounding the slug is what makes
/// id construction total.
///
/// Two obligations follow from truncating, and neither belongs in this module:
///
/// 1. **Task 9 must reject at config load** any portfolio whose pairs share a
///    slug once truncated. Two pairs colliding on a truncated prefix would
///    reintroduce exactly the cross-leg id collision [`unwind_link_id`] exists
///    to prevent.
/// 2. **`scripts/render_pairs_dashboard.py` must apply the same truncation.**
///    It attributes orders with `orderLinkId.startswith(f'{slug}-')` computed
///    from the untruncated config symbols, so a truncated pair's orders would
///    silently vanish from the dashboard. Nothing in the live portfolio is long
///    enough to be affected today — `aave_eth`, `ena_xrp` and `bnb_xaut` are
///    eight characters or fewer — which is why this is a follow-up rather than
///    a blocker.
const MAX_SLUG_LEN: usize = MAX_LINK_ID_LEN - MAX_LINK_ID_SUFFIX_LEN;

/// The pair's slug, clamped to what an `orderLinkId` can carry.
///
/// A no-op for every pair short enough to fit, which is every pair the live
/// portfolio trades; see [`MAX_SLUG_LEN`] for what truncation obliges.
fn bounded_slug(params: &PairParams) -> String {
    // Bybit symbols are ASCII, but truncating by character rather than by byte
    // index means a non-ASCII slug could never panic here.
    params.slug().chars().take(MAX_SLUG_LEN).collect()
}

/// Deterministic, bar-derived order id.
///
/// Derived from the bar rather than the wall clock so a retry within the same
/// bar reuses the id and Bybit deduplicates it. The Python used
/// `int(time.time())`, which minted a fresh id per retry and made a duplicate
/// position possible. The readable `slug` prefix is kept because the dashboard
/// attributes orders by it.
fn leg_link_id(params: &PairParams, tag: &str, bar_ms: i64) -> String {
    format!("{}-{tag}-{bar_ms}", bounded_slug(params))
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
    //
    // When `unwind_on_partial_fill` is off, neither leg is cancelled here —
    // `resolve_leg_no_cancel` only polls. Whether either remainder actually
    // gets cancelled is decided after `settle` below, once both legs' joint
    // outcome is known (a lone exposed leg must still be unwound; two
    // partially-filled legs are what gets ridden).
    let (a_resolved, b_resolved) = if cfg.unwind_on_partial_fill {
        tokio::join!(
            resolve_leg(client, cfg, Leg::A, &a_leg),
            resolve_leg(client, cfg, Leg::B, &b_leg)
        )
    } else {
        tokio::join!(
            resolve_leg_no_cancel(client, cfg, Leg::A, &a_leg),
            resolve_leg_no_cancel(client, cfg, Leg::B, &b_leg)
        )
    };

    // `join!` ran both resolutions to completion, so when one leg errors the
    // *other* leg's report is already in hand. Propagating the error without
    // looking at it would strand a filled sibling with nothing recording it —
    // the same naked-leg failure this module exists to prevent, reached from a
    // different direction.
    let (a_report, b_report) = match (a_resolved, b_resolved) {
        (Ok(a), Ok(b)) => (a, b),
        (Err(e), Ok(sibling)) => {
            warn!(
                pair = %params.display_pair(),
                leg = "a",
                error = %e,
                "leg A could not be resolved; rescuing leg B before halting"
            );
            unwind_sibling(client, cfg, params, quotes, bar_ms, sibling).await?;
            return Err(name_leg(Leg::A, &a_leg, e));
        }
        (Ok(sibling), Err(e)) => {
            warn!(
                pair = %params.display_pair(),
                leg = "b",
                error = %e,
                "leg B could not be resolved; rescuing leg A before halting"
            );
            unwind_sibling(client, cfg, params, quotes, bar_ms, sibling).await?;
            return Err(name_leg(Leg::B, &b_leg, e));
        }
        (Err(a_err), Err(b_err)) => {
            // Neither leg's state is known, so there is nothing that can safely
            // be unwound: an unwind needs a symbol and a size, and both are
            // precisely what is missing. Halt with both errors on the record.
            error!(
                pair = %params.display_pair(),
                leg_a_error = %a_err,
                leg_b_error = %b_err,
                "neither leg could be resolved; halting for reconciliation"
            );
            return Err(name_leg(Leg::A, &a_leg, a_err));
        }
    };

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
            if !cfg.unwind_on_partial_fill {
                // Nothing filled on either side; no cancel happened in
                // resolve_leg_no_cancel. Nothing to ride either, so clean up
                // exactly as the default path would have.
                cancel_leg_best_effort(client, &a_leg.symbol, &a_leg.order_link_id).await;
                cancel_leg_best_effort(client, &b_leg.symbol, &b_leg.order_link_id).await;
            }
            info!(pair = %params.display_pair(), "pair entry did not fill; account is flat");
            Ok(None)
        }
        Settlement::Unwind(legs) if !cfg.unwind_on_partial_fill && legs.len() == 2 => {
            // Both legs carry real exposure. Ride it: their remainder orders
            // are already resting (resolve_leg_no_cancel never cancelled
            // them), so leave them working and open the position at whatever
            // is filled right now.
            let a = legs.iter().find(|l| l.leg == Leg::A).cloned().expect("leg a present");
            let b = legs.iter().find(|l| l.leg == Leg::B).cloned().expect("leg b present");
            warn!(
                pair = %params.display_pair(),
                a_filled = %a.executed_qty, a_requested = %a.requested_qty,
                b_filled = %b.executed_qty, b_requested = %b.requested_qty,
                "pair entry partially filled on both legs; unwind_on_partial_fill is off — \
                 riding the partial size, remainder orders left resting on the book"
            );
            Ok(Some(OpenedPair { side, a, b, opened_at_ms: bar_ms }))
        }
        Settlement::Unwind(legs) => {
            // Either the feature is on, or exactly one leg carries exposure
            // (a true naked fill) — a strictly worse case the flag above does
            // not cover. Preserve the original safety behavior: make sure
            // nothing is still resting, then flatten whatever filled.
            if !cfg.unwind_on_partial_fill {
                cancel_leg_best_effort(client, &a_leg.symbol, &a_leg.order_link_id).await;
                cancel_leg_best_effort(client, &b_leg.symbol, &b_leg.order_link_id).await;
            }
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

/// Attach the leg, symbol and quantity to an otherwise anonymous exchange
/// error, so the alert an operator wakes up to says what is at stake.
fn name_leg(leg: Leg, order: &LimitLeg, err: ExecutionError) -> ExecutionError {
    match err {
        ExecutionError::Exchange(source) => ExecutionError::LegUnresolved {
            leg,
            symbol: order.symbol.clone(),
            qty: order.qty,
            source,
        },
        // Already named and actionable; re-wrapping would only bury it.
        other => other,
    }
}

/// Flatten a leg whose sibling could not be resolved.
///
/// If the sibling executed anything it must be flattened before the error is
/// returned: the alternative is halting with a filled leg on the book and
/// nothing recording it. A sibling that executed nothing needs no order, which
/// is why this is not simply an unconditional `unwind_legs` call.
async fn unwind_sibling(
    client: &dyn ExchangeClient,
    cfg: &ExecutorConfig,
    params: &PairParams,
    quotes: (&LegQuote, &LegQuote),
    bar_ms: i64,
    sibling: LegReport,
) -> Result<(), ExecutionError> {
    if !sibling.has_exposure() {
        return Ok(());
    }
    warn!(
        symbol = %sibling.symbol,
        qty = %sibling.executed_qty,
        "the sibling leg carries exposure; unwinding it before halting"
    );
    unwind_legs(client, cfg, params, &[sibling], bar_ms, quotes, "uw").await
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
        None => {
            // The exchange has no record of this id. That is *usually* a
            // placement that never landed — but it is also exactly what a
            // placement looks like in the window between the venue accepting it
            // and the order becoming visible to a query. Trusting "absent"
            // would leave a live crossing GTC limit resting on the book with
            // nothing tracking it, so a cancel goes out regardless.
            //
            // Its error is deliberately ignored: cancelling an id the exchange
            // genuinely never saw is *expected* to fail, and that failure is
            // the confirmation rather than a problem.
            if let Err(e) = client
                .cancel_order(&order.symbol, &order.order_link_id)
                .await
            {
                debug!(
                    symbol = %order.symbol,
                    link_id = %order.order_link_id,
                    error = %e,
                    "best-effort cancel of an order the exchange has no record of"
                );
            }
            None
        }
    };

    Ok(build_report(leg, order, status.as_ref()))
}

/// Like [`resolve_leg`], but never cancels — used when `unwind_on_partial_fill`
/// is off, so the caller can decide to cancel (or not) only once both legs'
/// joint outcome is known. Whatever the poll last saw, terminal or not, is
/// reported as-is; the order itself is untouched either way.
async fn resolve_leg_no_cancel(
    client: &dyn ExchangeClient,
    cfg: &ExecutorConfig,
    leg: Leg,
    order: &LimitLeg,
) -> Result<LegReport, ExecutionError> {
    let status = poll_to_terminal(client, cfg, &order.symbol, &order.order_link_id).await?;
    Ok(build_report(leg, order, status.as_ref()))
}

/// Cancel an order, logging rather than failing on error.
///
/// Mirrors the ignored-error cancel already used in [`resolve_leg`]'s `None`
/// branch: a cancel against an id the exchange settled or never saw is
/// *expected* to fail, and that failure is confirmation, not a problem.
async fn cancel_leg_best_effort(client: &dyn ExchangeClient, symbol: &Symbol, link_id: &str) {
    if let Err(e) = client.cancel_order(symbol, link_id).await {
        debug!(%symbol, link_id, error = %e, "best-effort cancel");
    }
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

/// The signed size of the account's open position on `symbol`; zero when flat.
///
/// Positive is long, negative short. Shared rather than left to each caller to
/// filter `positions()` itself, so there is exactly one definition of "how much
/// of this symbol do we actually hold" for the unwind ladder and for Task 7's
/// close path.
///
/// Reading the venue rather than accumulating local fill reports is the same
/// discipline rule 1 applies to placements. Local bookkeeping misses fills that
/// land in the cancel/re-read race, and it is blind to a manual close or a
/// liquidation; only the exchange knows what is actually held.
pub async fn open_position_size(
    client: &dyn ExchangeClient,
    symbol: &Symbol,
) -> Result<Decimal, ExchangeError> {
    Ok(client
        .positions()
        .await?
        .iter()
        .filter(|p| &p.symbol == symbol)
        .map(|p| match p.side {
            Side::Buy => p.size,
            Side::Sell => -p.size,
        })
        .sum())
}

/// Flatten every listed leg with reduce-only limits, escalating the price
/// through the book on each attempt.
///
/// **There is deliberately no market-order fallback.** The limit-only rule
/// holds here exactly as everywhere else; when the ladder is exhausted this
/// returns [`ExecutionError::UnwindExhausted`] so a human is brought in.
///
/// Every rung is sized from a fresh [`open_position_size`] read rather than from
/// the leg report that got us here. A rung that partially fills leaves a
/// *residual*, and a ladder that keeps re-requesting the original quantity can
/// flatten a leg across two rungs and still report it stranded — the number
/// handed to a human has to be what is genuinely still on the book.
///
/// Nothing in the per-leg loop uses `?`. A transient failure on one leg must
/// never stop the other leg from being attempted, and an early return here
/// would also discard the stranded legs already collected.
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
        // Pessimistic until a rung is *observed* to have flattened the leg.
        let mut flattened = false;
        let mut residual = leg.executed_qty;

        for (attempt, ticks) in cfg.unwind_ladder.iter().enumerate() {
            // What the account actually holds, read fresh every rung. This also
            // catches a leg closed by hand or liquidated between two rungs,
            // which no amount of local accounting would notice.
            let signed = match open_position_size(client, &leg.symbol).await {
                Ok(size) => size,
                Err(e) => {
                    // Without a trustworthy size there is nothing safe to size
                    // an order from, and falling back to the leg report is
                    // exactly the local bookkeeping this read exists to avoid.
                    // Skip the rung rather than guess.
                    warn!(
                        symbol = %leg.symbol,
                        attempt,
                        error = %e,
                        "could not read the open position; skipping this rung"
                    );
                    continue;
                }
            };
            if signed.is_zero() {
                info!(
                    symbol = %leg.symbol,
                    attempt,
                    "leg is already flat; no further rungs needed"
                );
                flattened = true;
                residual = Decimal::ZERO;
                break;
            }
            residual = signed.abs();
            // The direction comes from the live position, not from the leg
            // report: the position is what has to be flattened, and the report
            // is only a claim about one order that may since have been overtaken.
            let side = if signed > Decimal::ZERO {
                Side::Sell
            } else {
                Side::Buy
            };

            // Re-quote each attempt: a rung that failed did so because the
            // book moved, and pricing the next rung off a stale quote is how a
            // ladder walks in the wrong direction.
            let t = match client.ticker(&leg.symbol).await {
                Ok(t) => t,
                Err(e) => {
                    warn!(
                        symbol = %leg.symbol,
                        attempt,
                        error = %e,
                        "could not re-quote the book; skipping this rung"
                    );
                    continue;
                }
            };
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
                qty = %residual,
                "unwinding stranded leg with a reduce-only limit"
            );

            let order = LimitLeg {
                symbol: leg.symbol.clone(),
                side,
                qty: residual,
                price,
                order_link_id: link_id,
                reduce_only: true,
            };
            if let Err(e) = client.place_limit_leg(order.clone()).await {
                // Not fatal on its own: the query below establishes what
                // actually happened, and the next rung may work. Logged rather
                // than dropped because an id the venue *refuses* — too long, or
                // a duplicate — looks identical to a rung that simply did not
                // fill, and only this line tells the two apart.
                warn!(
                    symbol = %order.symbol,
                    link_id = %order.order_link_id,
                    attempt,
                    error = %e,
                    "unwind placement returned an error; querying for the truth"
                );
            }
            let report = match resolve_leg(client, cfg, leg.leg, &order).await {
                Ok(r) => r,
                Err(e) => {
                    warn!(
                        symbol = %leg.symbol,
                        attempt,
                        error = %e,
                        "could not establish what the unwind order did; trying the next rung"
                    );
                    continue;
                }
            };
            if report.is_fully_filled() {
                flattened = true;
                residual = Decimal::ZERO;
                break;
            }
        }

        if !flattened {
            // One last read. The final rung's fill, full or partial, is not in
            // `residual` — that was taken before the order went out — and the
            // quantity a human is handed has to be what is still on the book.
            match open_position_size(client, &leg.symbol).await {
                Ok(size) if size.is_zero() => flattened = true,
                Ok(size) => residual = size.abs(),
                Err(e) => warn!(
                    symbol = %leg.symbol,
                    error = %e,
                    "could not confirm the residual; reporting the last size seen"
                ),
            }
        }

        if flattened {
            info!(symbol = %leg.symbol, "stranded leg flattened");
        } else {
            error!(
                symbol = %leg.symbol,
                qty = %residual,
                "unwind ladder exhausted; leg is still open and needs a human"
            );
            // The residual, not the original: reporting a quantity that is no
            // longer on the book sends a human looking for the wrong position.
            stranded.push(LegReport {
                executed_qty: residual,
                ..leg.clone()
            });
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
    fn no_id_the_scheme_can_emit_exceeds_bybits_thirty_six_character_limit() {
        // The worst case the *scheme* allows, not a case that happens to fit.
        // The previous version of this test picked a 17-character slug, landed
        // on exactly 36, and certified a guarantee the code did not have:
        // `1000FLOKI`/`1000BONK` — all live Bybit linear perps — produced a
        // 37-character id, so every unwind rung would have been rejected while
        // the entry legs placed fine.
        let p = params("1000FLOKIUSDT", "1000BONKUSDT");
        assert!(p.slug().len() > MAX_SLUG_LEN, "the pair must exercise truncation");

        for tag in ["uw", "cl"] {
            // Two-digit rungs are budgeted for, so lengthening `unwind_ladder`
            // beyond ten rungs cannot silently overflow the cap.
            for attempt in [0usize, 9, 99] {
                for leg in [Leg::A, Leg::B] {
                    let id = unwind_link_id(&p, tag, leg, attempt, 1_700_000_000_000);
                    assert!(id.len() <= MAX_LINK_ID_LEN, "{id} is {} chars", id.len());
                }
            }
        }
        for tag in ["a", "b"] {
            let id = leg_link_id(&p, tag, 1_700_000_000_000);
            assert!(id.len() <= MAX_LINK_ID_LEN, "{id} is {} chars", id.len());
        }
    }

    #[test]
    fn truncation_never_touches_a_pair_the_live_portfolio_trades() {
        // The dashboard attributes orders with a `startswith(f"{slug}-")` filter
        // built from the untruncated config symbols, so truncation must stay
        // invisible to every pair actually running. See `MAX_SLUG_LEN` for the
        // obligation this carries into Task 9 and the dashboard port.
        for (a, b, slug) in [
            ("AAVEUSDT", "ETHUSDT", "aave_eth"),
            ("ENAUSDT", "XRPUSDT", "ena_xrp"),
            ("BNBUSDT", "XAUTUSDT", "bnb_xaut"),
            ("DOGEUSDT", "XRPUSDT", "doge_xrp"),
            ("LINKUSDT", "XRPUSDT", "link_xrp"),
        ] {
            assert_eq!(bounded_slug(&params(a, b)), slug);
        }
    }

    #[test]
    fn a_named_leg_error_says_which_leg_which_symbol_and_how_much() {
        // An anonymous `Exchange` error tells an operator woken at 03:00
        // nothing about what to go and look for.
        let p = params("AAVEUSDT", "ETHUSDT");
        let order = LimitLeg {
            symbol: p.leg_a.clone(),
            side: Side::Buy,
            qty: dec!(10),
            price: dec!(100.15),
            order_link_id: leg_link_id(&p, "a", 1_700_000_000_000),
            reduce_only: false,
        };
        let err = name_leg(
            Leg::A,
            &order,
            ExecutionError::Exchange(ExchangeError::Decode("timeout".into())),
        );
        let rendered = err.to_string();
        assert!(rendered.contains("AAVEUSDT"), "{rendered}");
        assert!(rendered.contains("10"), "{rendered}");
        assert!(rendered.contains('A'), "{rendered}");
        assert!(matches!(err, ExecutionError::LegUnresolved { .. }));
    }

    #[test]
    fn an_already_named_error_is_not_re_wrapped() {
        // `UnwindExhausted` already carries the stranded legs; burying it
        // inside a `LegUnresolved` would lose the payload a human needs.
        let p = params("AAVEUSDT", "ETHUSDT");
        let order = LimitLeg {
            symbol: p.leg_a.clone(),
            side: Side::Buy,
            qty: dec!(10),
            price: dec!(100.15),
            order_link_id: leg_link_id(&p, "a", 1_700_000_000_000),
            reduce_only: false,
        };
        let err = name_leg(
            Leg::A,
            &order,
            ExecutionError::UnwindExhausted { legs: Vec::new() },
        );
        assert!(matches!(err, ExecutionError::UnwindExhausted { .. }));
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
