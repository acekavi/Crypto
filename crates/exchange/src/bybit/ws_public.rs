use botcore::{Candle, ErrorClass, Symbol, Timeframe};
use rust_decimal::Decimal;
use serde::Deserialize;

use super::transport::ExchangeError;
use crate::traits::Subscription;

/// Bybit public topic name, e.g. `kline.60.BTCUSDT`.
pub fn topic_for(sub: &Subscription) -> String {
    format!(
        "kline.{}.{}",
        sub.timeframe.as_bybit_interval(),
        sub.symbol.as_str()
    )
}

#[derive(Debug, Deserialize)]
struct KlineFrame {
    data: Vec<KlineData>,
}

#[derive(Debug, Deserialize)]
struct KlineData {
    start: i64,
    open: String,
    close: String,
    high: String,
    low: String,
    volume: String,
    turnover: String,
    confirm: bool,
}

fn interval_to_timeframe(interval: &str) -> Result<Timeframe, ExchangeError> {
    // Delegates to the inverse of `as_bybit_interval` rather than repeating
    // the mapping. The local copy handled only "60" and "240", so every M15
    // and D1 message this bot subscribes to failed to decode and the feed
    // reconnect-looped without ever delivering a candle.
    Timeframe::from_bybit_interval(interval)
        .ok_or_else(|| ExchangeError::Decode(format!("unsupported kline interval {interval}")))
}

/// Parse one WebSocket text frame.
///
/// Returns `Ok(None)` for frames that are not kline data (subscription acks,
/// pongs) — those are normal traffic, not failures. Returns an empty vector
/// when the frame holds only unconfirmed candles.
#[allow(clippy::type_complexity)]
pub fn parse_kline_message(
    raw: &str,
) -> Result<Option<Vec<(Symbol, Timeframe, Candle)>>, ExchangeError> {
    let value: serde_json::Value =
        serde_json::from_str(raw).map_err(|e| ExchangeError::Decode(format!("ws frame: {e}")))?;

    let Some(topic) = value.get("topic").and_then(|t| t.as_str()) else {
        return Ok(None);
    };
    if !topic.starts_with("kline.") {
        return Ok(None);
    }

    // topic is kline.{interval}.{symbol}
    let mut parts = topic.splitn(3, '.');
    parts.next();
    let interval = parts
        .next()
        .ok_or_else(|| ExchangeError::Decode(format!("malformed topic {topic}")))?;
    let symbol_str = parts
        .next()
        .ok_or_else(|| ExchangeError::Decode(format!("malformed topic {topic}")))?;

    let tf = interval_to_timeframe(interval)?;
    let symbol = Symbol::new(symbol_str);

    let frame: KlineFrame = serde_json::from_value(value)
        .map_err(|e| ExchangeError::Decode(format!("kline frame: {e}")))?;

    let parse = |s: &str, field: &str| -> Result<Decimal, ExchangeError> {
        s.parse::<Decimal>()
            .map_err(|e| ExchangeError::Decode(format!("ws kline {field}: {e}")))
    };

    let mut out = Vec::new();
    for d in frame.data {
        // Only confirmed candles matter: an unconfirmed bar is still moving.
        if !d.confirm {
            continue;
        }
        out.push((
            symbol.clone(),
            tf,
            Candle {
                open_time_ms: d.start,
                open: parse(&d.open, "open")?,
                high: parse(&d.high, "high")?,
                low: parse(&d.low, "low")?,
                close: parse(&d.close, "close")?,
                volume: parse(&d.volume, "volume")?,
                turnover: parse(&d.turnover, "turnover")?,
            },
        ));
    }
    Ok(Some(out))
}

/// How many candles are missing between two consecutive open times.
///
/// A reconnect after a dropped socket will resume mid-series; the feed uses
/// this to decide whether a REST backfill is required before emitting further
/// events, so indicators never see a hole.
pub fn missing_candle_count(prev_open_ms: i64, next_open_ms: i64, tf: Timeframe) -> i64 {
    let step = tf.duration_ms();
    let delta = next_open_ms - prev_open_ms;
    if delta <= step {
        return 0;
    }
    delta / step - 1
}

/// What the feed should do with a newly-confirmed candle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GapAction {
    /// No gap: emit the candle.
    Emit,
    /// `missing` candles are absent; backfill before emitting.
    BackfillThenEmit { missing: i64 },
}

/// Decide how to handle a candle given the last one seen for its stream.
///
/// `last_open` is `None` on the very first candle observed for a
/// (symbol, timeframe) pair, which is not a gap — there is nothing to
/// compare against yet.
pub fn gap_action(last_open: Option<i64>, candle_open_ms: i64, tf: Timeframe) -> GapAction {
    match last_open {
        None => GapAction::Emit,
        Some(prev) => match missing_candle_count(prev, candle_open_ms, tf) {
            0 => GapAction::Emit,
            missing => GapAction::BackfillThenEmit { missing },
        },
    }
}

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;
use tracing::{error, info, warn};

use super::rest::BybitRest;
use super::transport::backoff_delay;
use crate::traits::{ExchangeClient, MarketEvent, MarketFeed};

const PING_INTERVAL: Duration = Duration::from_secs(20);
const EVENT_CHANNEL_CAPACITY: usize = 1024;
const GAP_REFETCH_LIMIT: u16 = 200;

/// Public kline feed with automatic reconnect and REST gap backfill.
pub struct BybitPublicFeed {
    ws_url: String,
    rest: Arc<BybitRest>,
}

impl BybitPublicFeed {
    pub fn new(ws_url: String, rest: Arc<BybitRest>) -> Self {
        BybitPublicFeed { ws_url, rest }
    }

    /// Subscribe and return the driving task's handle alongside the receiver.
    ///
    /// The trait's `subscribe` deliberately hides the task, but a long-running
    /// bot re-ranks its universe daily and must be able to stop the old
    /// subscription before starting a new one — otherwise every re-rank leaks
    /// a task and a socket.
    pub async fn subscribe_with_handle(
        &self,
        subs: &[Subscription],
    ) -> Result<(broadcast::Receiver<MarketEvent>, JoinHandle<()>), ExchangeError> {
        let (tx, rx) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let url = self.ws_url.clone();
        let rest = Arc::clone(&self.rest);
        let subs = subs.to_vec();

        let handle = tokio::spawn(async move {
            // Last confirmed candle open time per (symbol, timeframe), used to
            // detect gaps across reconnects.
            let mut last_open: HashMap<(Symbol, Timeframe), i64> = HashMap::new();
            let mut attempt: u32 = 0;

            loop {
                match run_session(&url, &subs, &tx, &rest, &mut last_open).await {
                    Ok(()) => {
                        info!("public feed session ended cleanly; reconnecting");
                        attempt = 0;
                    }
                    Err(e) if e.class() == ErrorClass::Fatal => {
                        // rest.klines signs every request, including this
                        // nominally-public gap backfill, so a revoked or
                        // expired API key or an IP mismatch surfaces here as
                        // Fatal — none of which resolve without a human.
                        // Retrying forever at `warn!` would be
                        // indistinguishable from ordinary network flakiness
                        // in the logs, so end the task instead. That drops
                        // `tx`, so every broadcast::Receiver a caller holds
                        // starts seeing RecvError::Closed — an unambiguous
                        // halt signal. Mirrors ws_private.rs's Fatal arm
                        // deliberately.
                        error!(error = %e, "public feed hit a fatal error; ending the feed task");
                        break;
                    }
                    Err(e) => {
                        warn!(error = %e, attempt, "public feed session failed");
                        attempt = attempt.saturating_add(1);
                    }
                }
                tokio::time::sleep(backoff_delay(attempt, 500, 0.25)).await;
            }
        });

        Ok((rx, handle))
    }
}

#[async_trait]
impl MarketFeed for BybitPublicFeed {
    async fn subscribe(
        &self,
        subs: &[Subscription],
    ) -> Result<broadcast::Receiver<MarketEvent>, ExchangeError> {
        self.subscribe_with_handle(subs)
            .await
            .map(|(rx, _handle)| rx)
    }
}

/// One connection lifetime: connect, subscribe, pump messages until failure.
async fn run_session(
    url: &str,
    subs: &[Subscription],
    tx: &broadcast::Sender<MarketEvent>,
    rest: &BybitRest,
    last_open: &mut HashMap<(Symbol, Timeframe), i64>,
) -> Result<(), ExchangeError> {
    let (mut ws, _) = tokio_tungstenite::connect_async(url)
        .await
        .map_err(|e| ExchangeError::WebSocket(e.to_string()))?;

    let topics: Vec<String> = subs.iter().map(topic_for).collect();
    let sub_msg = serde_json::json!({ "op": "subscribe", "args": topics }).to_string();
    ws.send(Message::Text(sub_msg))
        .await
        .map_err(|e| ExchangeError::WebSocket(e.to_string()))?;

    info!(count = subs.len(), "subscribed to public kline topics");

    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.tick().await; // first tick fires immediately; skip it

    loop {
        tokio::select! {
            _ = ping.tick() => {
                ws.send(Message::Text(r#"{"op":"ping"}"#.into()))
                    .await
                    .map_err(|e| ExchangeError::WebSocket(e.to_string()))?;
            }
            frame = ws.next() => {
                let Some(frame) = frame else {
                    return Err(ExchangeError::WebSocket("stream closed".into()));
                };
                let msg = frame.map_err(|e| ExchangeError::WebSocket(e.to_string()))?;
                let Message::Text(text) = msg else { continue };

                let Some(candles) = parse_kline_message(&text)? else { continue };
                for (symbol, tf, candle) in candles {
                    let key = (symbol.clone(), tf);
                    let prev = last_open.get(&key).copied();
                    if let GapAction::BackfillThenEmit { missing } = gap_action(prev, candle.open_time_ms, tf) {
                        warn!(%symbol, missing, "kline gap detected; backfilling via REST");
                        match rest.klines(&symbol, tf, GAP_REFETCH_LIMIT).await {
                            Ok(backfill) => {
                                let _ = tx.send(MarketEvent::GapFilled {
                                    symbol: symbol.clone(),
                                    tf,
                                    candles: backfill,
                                });
                            }
                            Err(e) => {
                                // The backfill failed, so the hole is still
                                // open. Do NOT advance last_open and do NOT
                                // emit this candle: doing either would let
                                // the gap slip past undetected (last_open
                                // would jump past the hole, making it
                                // permanently invisible to future gap
                                // checks) and would feed indicators a
                                // discontinuous series with nothing aware of
                                // it. Instead, tear the session down so the
                                // outer loop reconnects with backoff; on the
                                // next candle last_open still holds the
                                // pre-gap value, so the same gap is
                                // re-detected and the backfill retried.
                                // Emitting nothing lets the engine's
                                // feed-staleness guard block new entries,
                                // which is the safe failure mode — silence
                                // beats a lying feed.
                                error!(%symbol, error = %e, "gap backfill failed; reconnecting to retry");
                                return Err(e);
                            }
                        }
                    }
                    last_open.insert(key, candle.open_time_ms);
                    // A send error means no receivers are listening yet; the
                    // engine may not have started consuming. Not fatal.
                    let _ = tx.send(MarketEvent::CandleClosed { symbol, tf, candle });
                }
            }
        }
    }
}
