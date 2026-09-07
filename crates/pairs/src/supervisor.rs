use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use botcore::{Candle, Instrument, Symbol};
use exchange::ExchangeClient;
use exchange::bybit::transport::ExchangeError;
use persistence::{Journal, PairEvent, PairEventKind, PairHeartbeat, PairPositionRecord};
use rust_decimal::Decimal;
use tokio::sync::RwLock;
use tracing::{error, info, warn};

use crate::executor::{ExecutionError, ExecutorConfig, LegQuote, OpenedPair, open_pair, unwind_legs};
use crate::settle::{Leg, LegReport};
use crate::signal::{ExitReason, PairParams, PairSide, SignalEngine, unrealized_pnl_fraction};
use crate::sizing::per_leg_notional;
use crate::spread::{RollingZ, log_spread};
use crate::state::{Reconciliation, reconcile_pair};

#[derive(Debug, Clone)]
pub struct BotSnapshot {
    pub bot_id: String,
    pub display_name: String,
    pub priority: u32,
    pub symbols: HashSet<Symbol>,
    pub has_position: bool,
    pub latest_bar_ms: Option<i64>,
    pub signal: Option<PairSide>,
}

#[derive(Debug, Default)]
pub struct PortfolioGuard {
    snapshots: HashMap<String, BotSnapshot>,
}

impl PortfolioGuard {
    pub fn update(&mut self, snapshot: BotSnapshot) {
        self.snapshots.insert(snapshot.bot_id.clone(), snapshot);
    }

    pub fn defer_reason(
        &self,
        bot_id: &str,
        _signal: PairSide,
        latest_bar_ms: i64,
    ) -> Option<String> {
        let me = self.snapshots.get(bot_id)?;
        for peer in self.snapshots.values() {
            if peer.bot_id == me.bot_id || peer.priority <= me.priority {
                continue;
            }
            if me.symbols.is_disjoint(&peer.symbols) {
                continue;
            }
            if peer.has_position {
                return Some(format!(
                    "deferred to higher-priority {} position on shared symbol",
                    peer.display_name
                ));
            }
            if let (Some(peer_bar), Some(peer_signal)) = (peer.latest_bar_ms, peer.signal)
                && peer_bar == latest_bar_ms
            {
                return Some(format!(
                    "deferred to higher-priority {} same bar signal={}",
                    peer.display_name,
                    peer_signal.as_str()
                ));
            }
        }
        None
    }
}

#[derive(Clone)]
pub struct PairContext {
    pub client: Arc<dyn ExchangeClient>,
    pub journal: Arc<Journal>,
    pub guard: Arc<RwLock<PortfolioGuard>>,
    pub bot_id: String,
    pub display_name: String,
    pub priority: u32,
    pub params: PairParams,
    pub exec: ExecutorConfig,
    pub loop_period: Duration,
    pub kline_margin_bars: u16,
    pub shadow: bool,
    pub instruments: HashMap<String, Instrument>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BarOutcome {
    TooFewCandles { common: usize },
    Unchanged,
    NoSignal,
    Deferred { reason: String },
    Opened { side: PairSide },
    ShadowEntry { side: PairSide },
    Closed { reason: ExitReason },
    Holding,
    Halted { reason: String },
}

#[derive(Debug, Clone)]
struct LatestSnapshot {
    latest_ms: i64,
    latest_z: f64,
    spread_sigma: f64,
    signal: Option<PairSide>,
    a_close: Decimal,
    b_close: Decimal,
}

#[derive(Debug, Clone)]
pub struct PreparedBar {
    prev_hb: Option<PairHeartbeat>,
    journal_position: Option<PairPositionRecord>,
    snapshot: Option<LatestSnapshot>,
    loop_ms: i64,
    common: usize,
    refreshed: bool,
}

const BAR_CLOSE_GRACE_MS: i64 = 5_000;

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn dec_to_f64(d: Decimal) -> Result<f64, ExecutionError> {
    d.to_string()
        .parse::<f64>()
        .map_err(|e| ExecutionError::Exchange(ExchangeError::Decode(format!("decimal to f64: {e}"))))
}

fn parse_side(s: &str) -> Option<PairSide> {
    match s {
        "long_spread" => Some(PairSide::LongSpread),
        "short_spread" => Some(PairSide::ShortSpread),
        _ => None,
    }
}

fn should_refresh_closed_bar(prev_last_bar_ms: Option<i64>, timeframe: botcore::Timeframe, now_ms: i64) -> bool {
    let Some(prev_last_bar_ms) = prev_last_bar_ms else {
        return true;
    };
    let tf_ms = timeframe.duration_ms();
    now_ms >= prev_last_bar_ms + tf_ms + tf_ms + BAR_CLOSE_GRACE_MS
}

fn heartbeat_from_previous(bot_id: &str, prev_hb: &PairHeartbeat, loop_ms: i64) -> PairHeartbeat {
    PairHeartbeat {
        bot_id: bot_id.to_string(),
        last_bar_ms: prev_hb.last_bar_ms,
        last_loop_ms: Some(loop_ms),
        last_z: prev_hb.last_z,
        last_signal: prev_hb.last_signal.clone(),
        last_guard_reason: prev_hb.last_guard_reason.clone(),
    }
}

fn position_from_opened(bot_id: &str, latest_z: f64, notional: Decimal, opened: OpenedPair) -> PairPositionRecord {
    PairPositionRecord {
        bot_id: bot_id.into(),
        side: opened.side.as_str().into(),
        opened_at_ms: opened.opened_at_ms,
        entry_z: Decimal::from_str_exact(&latest_z.to_string()).unwrap_or(Decimal::ZERO),
        a_symbol: opened.a.symbol,
        a_qty: opened.a.executed_qty,
        a_entry: opened.a.avg_price,
        a_order_id: opened.a.order_id,
        b_symbol: opened.b.symbol,
        b_qty: opened.b.executed_qty,
        b_entry: opened.b.avg_price,
        b_order_id: opened.b.order_id,
        breakeven_armed: false,
        per_leg_notional: notional,
        capped_by: None,
    }
}

fn leg_reports_from_record(record: &PairPositionRecord, side: PairSide) -> [LegReport; 2] {
    let (a_side, b_side) = match side {
        PairSide::LongSpread => (botcore::Side::Buy, botcore::Side::Sell),
        PairSide::ShortSpread => (botcore::Side::Sell, botcore::Side::Buy),
    };
    [
        LegReport {
            leg: Leg::A,
            symbol: record.a_symbol.clone(),
            side: a_side,
            requested_qty: record.a_qty,
            executed_qty: record.a_qty,
            avg_price: record.a_entry,
            order_id: record.a_order_id.clone(),
            state: Some(botcore::OrderState::Filled),
        },
        LegReport {
            leg: Leg::B,
            symbol: record.b_symbol.clone(),
            side: b_side,
            requested_qty: record.b_qty,
            executed_qty: record.b_qty,
            avg_price: record.b_entry,
            order_id: record.b_order_id.clone(),
            state: Some(botcore::OrderState::Filled),
        },
    ]
}

async fn latest_snapshot(ctx: &PairContext) -> Result<(Option<LatestSnapshot>, usize), ExecutionError> {
    let limit = ctx.params.rolling_window as u16 + ctx.kline_margin_bars;
    let a = ctx.client.klines(&ctx.params.leg_a, ctx.params.timeframe, limit).await?;
    let b = ctx.client.klines(&ctx.params.leg_b, ctx.params.timeframe, limit).await?;
    let by_a: HashMap<i64, Candle> = a.into_iter().map(|c| (c.open_time_ms, c)).collect();
    let by_b: HashMap<i64, Candle> = b.into_iter().map(|c| (c.open_time_ms, c)).collect();
    let mut common: Vec<i64> = by_a.keys().copied().filter(|ms| by_b.contains_key(ms)).collect();
    common.sort_unstable();
    if common.len() < ctx.params.rolling_window + 1 {
        return Ok((None, common.len()));
    }

    let mut rz = RollingZ::new(ctx.params.rolling_window);
    let mut last_stats = None;
    let mut last_a = Decimal::ZERO;
    let mut last_b = Decimal::ZERO;
    for ms in &common {
        let a_close = by_a.get(ms).expect("present").close;
        let b_close = by_b.get(ms).expect("present").close;
        last_a = a_close;
        last_b = b_close;
        let spread = log_spread(dec_to_f64(a_close)?, dec_to_f64(b_close)?);
        last_stats = rz.push(spread);
    }
    let stats = last_stats.expect("window full");
    let latest_ms = *common.last().expect("common non-empty");
    let signal = SignalEngine::new(ctx.params.clone()).entry_signal(stats.z);
    Ok((
        Some(LatestSnapshot {
            latest_ms,
            latest_z: stats.z,
            spread_sigma: stats.sd,
            signal,
            a_close: last_a,
            b_close: last_b,
        }),
        common.len(),
    ))
}

async fn quotes_for(ctx: &PairContext) -> Result<(LegQuote, LegQuote), ExecutionError> {
    let a_t = ctx.client.ticker(&ctx.params.leg_a).await?;
    let b_t = ctx.client.ticker(&ctx.params.leg_b).await?;
    let a_i = ctx
        .instruments
        .get(ctx.params.leg_a.as_str())
        .cloned()
        .ok_or_else(|| ExecutionError::Exchange(ExchangeError::Decode(format!("missing instrument {}", ctx.params.leg_a))))?;
    let b_i = ctx
        .instruments
        .get(ctx.params.leg_b.as_str())
        .cloned()
        .ok_or_else(|| ExecutionError::Exchange(ExchangeError::Decode(format!("missing instrument {}", ctx.params.leg_b))))?;
    Ok((
        LegQuote { instrument: a_i, bid: a_t.bid1, ask: a_t.ask1, last: a_t.last_price },
        LegQuote { instrument: b_i, bid: b_t.bid1, ask: b_t.ask1, last: b_t.last_price },
    ))
}

pub async fn prepare_bar_with_state(
    ctx: &PairContext,
    prev_hb: Option<PairHeartbeat>,
    journal_position: Option<PairPositionRecord>,
) -> Result<PreparedBar, ExecutionError> {
    let loop_ms = now_ms();

    if !should_refresh_closed_bar(prev_hb.as_ref().and_then(|h| h.last_bar_ms), ctx.params.timeframe, loop_ms) {
        return Ok(PreparedBar {
            prev_hb,
            journal_position,
            snapshot: None,
            loop_ms,
            common: 0,
            refreshed: false,
        });
    }

    let (snapshot, common) = latest_snapshot(ctx).await?;
    Ok(PreparedBar {
        prev_hb,
        journal_position,
        snapshot,
        loop_ms,
        common,
        refreshed: true,
    })
}

pub async fn prepare_bar(ctx: &PairContext) -> Result<PreparedBar, ExecutionError> {
    let prev_hb = ctx.journal.pair_heartbeat(&ctx.bot_id).await.ok().flatten();
    let journal_position = ctx.journal.pair_position(&ctx.bot_id).await?;
    prepare_bar_with_state(ctx, prev_hb, journal_position).await
}

pub async fn evaluate_prepared_bar(ctx: &PairContext, prepared: PreparedBar) -> Result<BarOutcome, ExecutionError> {
    let PreparedBar {
        prev_hb,
        journal_position,
        snapshot,
        loop_ms,
        common,
        refreshed,
    } = prepared;

    if !refreshed {
        if let Some(prev_hb) = prev_hb.as_ref() {
            ctx.journal
                .upsert_pair_heartbeat(&heartbeat_from_previous(&ctx.bot_id, prev_hb, loop_ms))
                .await?;
        }
        return Ok(BarOutcome::Unchanged);
    }

    let prev_last_bar = prev_hb.as_ref().and_then(|h| h.last_bar_ms);

    let Some(snapshot) = snapshot else {
        ctx.journal
            .upsert_pair_heartbeat(&PairHeartbeat {
                bot_id: ctx.bot_id.clone(),
                last_bar_ms: None,
                last_loop_ms: Some(loop_ms),
                last_z: None,
                last_signal: None,
                last_guard_reason: None,
            })
            .await?;
        return Ok(BarOutcome::TooFewCandles { common });
    };

    ctx.journal
        .upsert_pair_heartbeat(&PairHeartbeat {
            bot_id: ctx.bot_id.clone(),
            last_bar_ms: Some(snapshot.latest_ms),
            last_loop_ms: Some(loop_ms),
            last_z: Some(Decimal::from_str_exact(&snapshot.latest_z.to_string()).unwrap_or(Decimal::ZERO)),
            last_signal: snapshot.signal.map(|s| s.as_str().to_string()),
            last_guard_reason: None,
        })
        .await?;

    {
        let mut guard = ctx.guard.write().await;
        guard.update(BotSnapshot {
            bot_id: ctx.bot_id.clone(),
            display_name: ctx.display_name.clone(),
            priority: ctx.priority,
            symbols: [ctx.params.leg_a.clone(), ctx.params.leg_b.clone()].into_iter().collect(),
            has_position: journal_position.is_some(),
            latest_bar_ms: Some(snapshot.latest_ms),
            signal: snapshot.signal,
        });
    }

    if prev_last_bar == Some(snapshot.latest_ms) {
        return Ok(BarOutcome::Unchanged);
    }

    match reconcile_pair(ctx.client.as_ref(), journal_position.as_ref(), &ctx.params).await? {
        Reconciliation::Flat => {}
        Reconciliation::Holding(rec) => {
            let Some(side) = parse_side(&rec.side) else {
                let reason = format!("journal row has invalid pair side {}", rec.side);
                ctx.journal.record_pair_event(&PairEvent {
                    bot_id: ctx.bot_id.clone(),
                    at_ms: snapshot.latest_ms,
                    kind: PairEventKind::Halted,
                    detail: reason.clone(),
                }).await?;
                return Ok(BarOutcome::Halted { reason });
            };
            let engine = SignalEngine::new(ctx.params.clone());
            let mut updated = rec.clone();
            if !updated.breakeven_armed && engine.should_arm_breakeven(side, snapshot.latest_z) {
                updated.breakeven_armed = true;
                ctx.journal.upsert_pair_position(&updated).await?;
            }
            let age_bars = ((snapshot.latest_ms - updated.opened_at_ms) / ctx.params.timeframe.duration_ms()).max(0);
            let pnl = unrealized_pnl_fraction(
                side,
                updated.a_entry,
                updated.b_entry,
                snapshot.a_close,
                snapshot.b_close,
                ctx.params.fee_per_leg,
            );
            if let Some(reason) = engine.exit_reason(side, snapshot.latest_z, age_bars, updated.breakeven_armed, Some(pnl)) {
                if ctx.shadow {
                    ctx.journal.record_pair_event(&PairEvent {
                        bot_id: ctx.bot_id.clone(),
                        at_ms: snapshot.latest_ms,
                        kind: PairEventKind::Exit,
                        detail: format!("shadow exit reason={}", reason.as_str()),
                    }).await?;
                    return Ok(BarOutcome::Closed { reason });
                }
                let quotes = quotes_for(ctx).await?;
                let legs = leg_reports_from_record(&updated, side);
                unwind_legs(ctx.client.as_ref(), &ctx.exec, &ctx.params, &legs, snapshot.latest_ms, (&quotes.0, &quotes.1), "cl").await?;
                ctx.journal.clear_pair_position(&ctx.bot_id).await?;
                ctx.journal.record_pair_event(&PairEvent {
                    bot_id: ctx.bot_id.clone(),
                    at_ms: snapshot.latest_ms,
                    kind: PairEventKind::Exit,
                    detail: format!("exit reason={}", reason.as_str()),
                }).await?;
                return Ok(BarOutcome::Closed { reason });
            }
            return Ok(BarOutcome::Holding);
        }
        Reconciliation::Halt { reason } => {
            ctx.journal.record_pair_event(&PairEvent {
                bot_id: ctx.bot_id.clone(),
                at_ms: snapshot.latest_ms,
                kind: PairEventKind::Halted,
                detail: reason.clone(),
            }).await?;
            return Ok(BarOutcome::Halted { reason });
        }
    }

    let Some(signal) = snapshot.signal else {
        return Ok(BarOutcome::NoSignal);
    };

    let guard_reason = { ctx.guard.read().await.defer_reason(&ctx.bot_id, signal, snapshot.latest_ms) };
    if let Some(reason) = guard_reason {
        ctx.journal
            .upsert_pair_heartbeat(&PairHeartbeat {
                bot_id: ctx.bot_id.clone(),
                last_bar_ms: Some(snapshot.latest_ms),
                last_loop_ms: Some(loop_ms),
                last_z: Some(Decimal::from_str_exact(&snapshot.latest_z.to_string()).unwrap_or(Decimal::ZERO)),
                last_signal: Some(signal.as_str().to_string()),
                last_guard_reason: Some(reason.clone()),
            })
            .await?;
        ctx.journal.record_pair_event(&PairEvent {
            bot_id: ctx.bot_id.clone(),
            at_ms: snapshot.latest_ms,
            kind: PairEventKind::EntrySkipped,
            detail: reason.clone(),
        }).await?;
        return Ok(BarOutcome::Deferred { reason });
    }

    let balance = ctx.client.balance().await?;
    let sizing = per_leg_notional(&ctx.params, balance.equity, balance.available, snapshot.spread_sigma)
        .map_err(|e| ExecutionError::Exchange(ExchangeError::Decode(format!("sizing: {e}"))))?;

    if ctx.shadow {
        ctx.journal.record_pair_event(&PairEvent {
            bot_id: ctx.bot_id.clone(),
            at_ms: snapshot.latest_ms,
            kind: PairEventKind::Entry,
            detail: format!("shadow entry side={} notional={}", signal.as_str(), sizing.notional),
        }).await?;
        return Ok(BarOutcome::ShadowEntry { side: signal });
    }

    let quotes = quotes_for(ctx).await?;
    match open_pair(
        ctx.client.as_ref(),
        &ctx.exec,
        &ctx.params,
        (&quotes.0, &quotes.1),
        signal,
        sizing.notional,
        snapshot.latest_ms,
    ).await? {
        Some(opened) => {
            let mut record = position_from_opened(&ctx.bot_id, snapshot.latest_z, sizing.notional, opened);
            record.capped_by = sizing.capped_by.map(|r| match r {
                crate::sizing::CapReason::AvailableEquity => "available_equity".to_string(),
                crate::sizing::CapReason::MaxNotionalMultiple => "max_notional_multiple".to_string(),
            });
            ctx.journal.upsert_pair_position(&record).await?;
            ctx.journal.record_pair_event(&PairEvent {
                bot_id: ctx.bot_id.clone(),
                at_ms: snapshot.latest_ms,
                kind: PairEventKind::Entry,
                detail: format!("entry side={} notional={}", signal.as_str(), sizing.notional),
            }).await?;
            Ok(BarOutcome::Opened { side: signal })
        }
        None => {
            ctx.journal.record_pair_event(&PairEvent {
                bot_id: ctx.bot_id.clone(),
                at_ms: snapshot.latest_ms,
                kind: PairEventKind::Unwound,
                detail: "entry did not produce a whole pair; account remains flat".into(),
            }).await?;
            Ok(BarOutcome::NoSignal)
        }
    }
}

pub async fn evaluate_bar(ctx: &PairContext) -> Result<BarOutcome, ExecutionError> {
    let prepared = prepare_bar(ctx).await?;
    evaluate_prepared_bar(ctx, prepared).await
}

pub async fn run_pair(ctx: PairContext) -> Result<(), ExecutionError> {
    loop {
        match evaluate_bar(&ctx).await {
            Ok(BarOutcome::Halted { reason }) => {
                error!(bot_id = %ctx.bot_id, reason = %reason, "pair halted");
                return Ok(());
            }
            Ok(outcome) => {
                info!(bot_id = %ctx.bot_id, ?outcome, "pair loop tick");
            }
            Err(ExecutionError::UnwindExhausted { legs }) => {
                let detail = format!("unwind failed for {} leg(s): {:?}", legs.len(), legs);
                let _ = ctx
                    .journal
                    .record_pair_event(&PairEvent {
                        bot_id: ctx.bot_id.clone(),
                        at_ms: now_ms(),
                        kind: PairEventKind::UnwindFailed,
                        detail: detail.clone(),
                    })
                    .await;
                error!(bot_id = %ctx.bot_id, detail = %detail, "pair loop halted after unwind failure");
                return Err(ExecutionError::UnwindExhausted { legs });
            }
            Err(e) => {
                warn!(bot_id = %ctx.bot_id, error = %e, "pair loop iteration failed; retrying next tick");
            }
        }
        tokio::time::sleep(ctx.loop_period).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PairSide;
    use botcore::Symbol;

    fn snapshot(id: &str, priority: u32, symbols: &[&str]) -> BotSnapshot {
        BotSnapshot {
            bot_id: id.into(),
            display_name: id.to_uppercase(),
            priority,
            symbols: symbols
                .iter()
                .map(|s| Symbol::new(*s))
                .collect::<HashSet<_>>(),
            has_position: false,
            latest_bar_ms: Some(100),
            signal: None,
        }
    }

    fn guard(snapshots: Vec<BotSnapshot>) -> PortfolioGuard {
        let mut g = PortfolioGuard::default();
        for s in snapshots {
            g.update(s);
        }
        g
    }

    #[test]
    fn a_lower_priority_bot_defers_to_a_higher_one_holding_a_shared_symbol() {
        let mut high = snapshot("doge_xrp", 100, &["DOGEUSDT", "XRPUSDT"]);
        high.has_position = true;
        let g = guard(vec![
            high,
            snapshot("link_xrp", 90, &["LINKUSDT", "XRPUSDT"]),
        ]);

        let reason = g
            .defer_reason("link_xrp", PairSide::ShortSpread, 100)
            .expect("must defer");
        assert!(reason.contains("DOGE_XRP"), "reason was {reason}");
        assert!(reason.contains("position"), "reason was {reason}");
    }

    #[test]
    fn a_lower_priority_bot_defers_to_a_higher_ones_signal_on_the_same_bar() {
        let mut high = snapshot("doge_xrp", 100, &["DOGEUSDT", "XRPUSDT"]);
        high.signal = Some(PairSide::ShortSpread);
        let g = guard(vec![
            high,
            snapshot("link_xrp", 90, &["LINKUSDT", "XRPUSDT"]),
        ]);

        let reason = g
            .defer_reason("link_xrp", PairSide::LongSpread, 100)
            .expect("must defer");
        assert!(reason.contains("same bar"), "reason was {reason}");
    }

    #[test]
    fn a_stale_peer_signal_from_an_earlier_bar_does_not_block() {
        let mut high = snapshot("doge_xrp", 100, &["DOGEUSDT", "XRPUSDT"]);
        high.signal = Some(PairSide::ShortSpread);
        high.latest_bar_ms = Some(99);
        let g = guard(vec![
            high,
            snapshot("link_xrp", 90, &["LINKUSDT", "XRPUSDT"]),
        ]);
        assert_eq!(g.defer_reason("link_xrp", PairSide::LongSpread, 100), None);
    }

    #[test]
    fn bots_that_share_no_symbol_never_block_each_other() {
        let mut high = snapshot("aave_eth", 100, &["AAVEUSDT", "ETHUSDT"]);
        high.has_position = true;
        high.signal = Some(PairSide::ShortSpread);
        let g = guard(vec![
            high,
            snapshot("bnb_xaut", 80, &["BNBUSDT", "XAUTUSDT"]),
        ]);
        assert_eq!(g.defer_reason("bnb_xaut", PairSide::LongSpread, 100), None);
    }

    #[test]
    fn a_higher_priority_bot_never_defers_to_a_lower_one() {
        let mut low = snapshot("bnb_xaut", 80, &["XRPUSDT", "XAUTUSDT"]);
        low.has_position = true;
        let g = guard(vec![
            snapshot("aave_eth", 100, &["AAVEUSDT", "XRPUSDT"]),
            low,
        ]);
        assert_eq!(g.defer_reason("aave_eth", PairSide::LongSpread, 100), None);
    }

    #[test]
    fn equal_priorities_do_not_block_each_other() {
        let mut peer = snapshot("b", 100, &["XRPUSDT", "SOLUSDT"]);
        peer.has_position = true;
        let g = guard(vec![snapshot("a", 100, &["AAVEUSDT", "XRPUSDT"]), peer]);
        assert_eq!(g.defer_reason("a", PairSide::LongSpread, 100), None);
    }

    #[test]
    fn an_unknown_bot_id_does_not_defer() {
        let g = guard(vec![snapshot("a", 100, &["AAVEUSDT", "ETHUSDT"])]);
        assert_eq!(g.defer_reason("nobody", PairSide::LongSpread, 100), None);
    }
}
