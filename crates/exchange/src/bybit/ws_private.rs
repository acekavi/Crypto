use botcore::{Balance, ErrorClass, OpenOrder, Position, Symbol};
use serde::Deserialize;
use serde_json::Value;

use super::sign::{sign_ws_auth, Credentials};
use super::transport::ExchangeError;
use super::wire::{OpenOrderRow, PositionRow, WalletRow};

/// An account-state change pushed over the private stream.
#[derive(Debug, Clone)]
pub enum AccountEvent {
    OrderUpdate(OpenOrder),
    PositionUpdate(Position),
    /// Emitted when the exchange reports size 0 — the position is gone.
    PositionClosed { symbol: Symbol },
    WalletUpdate(Balance),
}

/// Build the private-stream auth frame.
///
/// `expires_ms` must be comfortably in the future; the caller derives it from
/// the clock-corrected time so local drift cannot invalidate it.
pub fn build_auth_frame(creds: &Credentials, expires_ms: i64) -> String {
    let signature = sign_ws_auth(&creds.api_secret, expires_ms);
    serde_json::json!({
        "op": "auth",
        "args": [creds.api_key, expires_ms, signature],
    })
    .to_string()
}

#[derive(Debug, Deserialize)]
struct PrivateFrame {
    topic: String,
    data: Vec<Value>,
}

/// Parse one private-stream frame into zero or more account events.
///
/// Acks (`auth`, `subscribe`, `pong`) carry no topic and yield no events;
/// treating them as errors would cause a reconnect loop.
pub fn parse_private_message(raw: &str) -> Result<Vec<AccountEvent>, ExchangeError> {
    let value: Value = serde_json::from_str(raw)
        .map_err(|e| ExchangeError::Decode(format!("private frame: {e}")))?;

    if value.get("topic").is_none() {
        return Ok(Vec::new());
    }

    let frame: PrivateFrame = serde_json::from_value(value)
        .map_err(|e| ExchangeError::Decode(format!("private frame: {e}")))?;

    let mut out = Vec::new();
    for item in frame.data {
        match frame.topic.as_str() {
            "order" => {
                let row: OpenOrderRow = serde_json::from_value(item)
                    .map_err(|e| ExchangeError::Decode(format!("order update: {e}")))?;
                out.push(AccountEvent::OrderUpdate(row.into_open_order()?));
            }
            "position" => {
                let row: PositionRow = serde_json::from_value(item)
                    .map_err(|e| ExchangeError::Decode(format!("position update: {e}")))?;
                let symbol = Symbol::new(row.symbol.clone());
                match row.into_position()? {
                    Some(p) => out.push(AccountEvent::PositionUpdate(p)),
                    None => out.push(AccountEvent::PositionClosed { symbol }),
                }
            }
            "wallet" => {
                let row: WalletRow = serde_json::from_value(item)
                    .map_err(|e| ExchangeError::Decode(format!("wallet update: {e}")))?;
                out.push(AccountEvent::WalletUpdate(row.into_balance()?));
            }
            // `execution` frames duplicate information the order topic already
            // carries; ignored rather than double-counted.
            _ => {}
        }
    }
    Ok(out)
}

/// True when the frame is a rejected auth response.
///
/// Checked structurally rather than by substring match on the raw text:
/// a rejected auth is the one Fatal condition on this stream, and it must
/// not be missed because of incidental whitespace in the wire format.
pub fn is_auth_rejected(value: &Value) -> bool {
    value.get("op").and_then(|v| v.as_str()) == Some("auth")
        && value.get("success").and_then(|v| v.as_bool()) == Some(false)
}

#[cfg(test)]
mod parsing_tests {
    use super::*;

    #[test]
    fn rejected_auth_is_detected() {
        let value: Value = serde_json::from_str(r#"{"op":"auth","success":false}"#).unwrap();
        assert!(is_auth_rejected(&value));
    }

    #[test]
    fn successful_auth_is_not_rejected() {
        let value: Value = serde_json::from_str(r#"{"op":"auth","success":true}"#).unwrap();
        assert!(!is_auth_rejected(&value));
    }

    #[test]
    fn subscribe_ack_is_not_a_rejected_auth() {
        let value: Value = serde_json::from_str(r#"{"op":"subscribe","success":true}"#).unwrap();
        assert!(!is_auth_rejected(&value));
    }

    #[test]
    fn a_normal_topic_frame_is_not_a_rejected_auth() {
        let value: Value = serde_json::from_str(
            r#"{"topic":"wallet","data":[{"totalEquity":"1","totalAvailableBalance":"1"}]}"#,
        )
        .unwrap();
        assert!(!is_auth_rejected(&value));
    }

    #[test]
    fn rejected_auth_is_still_detected_with_incidental_whitespace() {
        // The check must be structural, not a substring match on the raw
        // text: a rejected auth serialized with spaces after colons (e.g. by
        // a different JSON encoder) must still be caught, because it is the
        // one Fatal condition on this stream.
        let spaced = "{\n  \"op\": \"auth\",\n  \"success\": false\n}";
        let value: Value = serde_json::from_str(spaced).unwrap();
        assert!(is_auth_rejected(&value));
    }
}

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use tokio::sync::broadcast;
use tokio_tungstenite::tungstenite::Message;
use tracing::{error, info, warn};

use super::sign::ClockOffset;
use super::transport::backoff_delay;

const PING_INTERVAL: Duration = Duration::from_secs(20);
const EVENT_CHANNEL_CAPACITY: usize = 1024;
const AUTH_VALIDITY_MS: i64 = 10_000;
const TOPICS: [&str; 4] = ["order", "position", "execution", "wallet"];

pub struct BybitPrivateFeed {
    ws_url: String,
    creds: Credentials,
    clock: Arc<ClockOffset>,
}

impl BybitPrivateFeed {
    pub fn new(ws_url: String, creds: Credentials, clock: Arc<ClockOffset>) -> Self {
        BybitPrivateFeed { ws_url, creds, clock }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<AccountEvent> {
        let (tx, rx) = broadcast::channel(EVENT_CHANNEL_CAPACITY);
        let url = self.ws_url.clone();
        let creds = self.creds.clone();
        let clock = Arc::clone(&self.clock);

        tokio::spawn(async move {
            let mut attempt: u32 = 0;
            loop {
                match run_private_session(&url, &creds, &clock, &tx).await {
                    Ok(()) => {
                        info!("private feed session ended; reconnecting");
                        attempt = 0;
                    }
                    Err(e) if e.class() == ErrorClass::Fatal => {
                        // Fatal (e.g. a rejected auth from a revoked or
                        // invalid API key) is unrecoverable without a
                        // human: retrying forever would look, from the
                        // outside, exactly like a flaky network. Ending the
                        // task instead drops `tx`, so every
                        // broadcast::Receiver a caller holds immediately
                        // starts returning RecvError::Closed — an
                        // unambiguous signal on the existing subscribe()
                        // signature, with no new health channel required.
                        error!(error = %e, "private feed session failed fatally; not retrying");
                        break;
                    }
                    Err(e) => {
                        warn!(error = %e, attempt, "private feed session failed");
                        attempt = attempt.saturating_add(1);
                    }
                }
                tokio::time::sleep(backoff_delay(attempt, 500, 0.25)).await;
            }
        });

        rx
    }
}

async fn run_private_session(
    url: &str,
    creds: &Credentials,
    clock: &ClockOffset,
    tx: &broadcast::Sender<AccountEvent>,
) -> Result<(), ExchangeError> {
    let (mut ws, _) = tokio_tungstenite::connect_async(url)
        .await
        .map_err(|e| ExchangeError::WebSocket(e.to_string()))?;

    // Expiry is derived from the clock-corrected time, so NTP drift on this
    // machine cannot produce an already-expired auth frame.
    let expires = clock.now_ms() + AUTH_VALIDITY_MS;
    ws.send(Message::Text(build_auth_frame(creds, expires)))
        .await
        .map_err(|e| ExchangeError::WebSocket(e.to_string()))?;

    let sub = serde_json::json!({ "op": "subscribe", "args": TOPICS }).to_string();
    ws.send(Message::Text(sub))
        .await
        .map_err(|e| ExchangeError::WebSocket(e.to_string()))?;

    info!("subscribed to private topics");

    let mut ping = tokio::time::interval(PING_INTERVAL);
    ping.tick().await;

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

                // An auth failure arrives as a frame, not a transport error,
                // and it is the one Fatal condition on this stream — checked
                // structurally via `is_auth_rejected` rather than by
                // substring match, so incidental whitespace in the wire
                // format can never let a rejection slip through as a
                // harmless no-topic ack. This parses `text` a second time
                // (parse_private_message parses it again below); kept as
                // two calls rather than widening parse_private_message to
                // take an already-parsed Value, since these frames are small
                // and infrequent and the simpler signature keeps its
                // existing tests untouched.
                let value: Value = serde_json::from_str(&text)
                    .map_err(|e| ExchangeError::Decode(format!("private frame: {e}")))?;
                if is_auth_rejected(&value) {
                    return Err(ExchangeError::Api {
                        code: 10004,
                        msg: format!("private stream auth rejected: {text}"),
                    });
                }

                for event in parse_private_message(&text)? {
                    let _ = tx.send(event);
                }
            }
        }
    }
}
