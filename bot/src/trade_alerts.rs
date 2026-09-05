use std::time::Duration;

use tokio::process::Command;
use tokio::time::timeout;

pub const DEFAULT_ALERT_TARGET: &str = "telegram:6034564398";
const ALERT_TIMEOUT: Duration = Duration::from_secs(20);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AlertOutcome {
    Opened,
    Closed,
    NoSignal,
    Unchanged,
    Deferred,
}

#[derive(Debug, Clone)]
pub struct AlertEvent {
    pub bot_id: String,
    pub display_name: String,
    pub at_ms: i64,
    pub outcome: AlertOutcome,
    pub side: Option<String>,
    pub detail: String,
}

pub fn should_alert_for_outcome(outcome: AlertOutcome) -> bool {
    matches!(outcome, AlertOutcome::Opened | AlertOutcome::Closed)
}

pub fn format_trade_alert(event: &AlertEvent) -> String {
    let action = match event.outcome {
        AlertOutcome::Opened => "opened",
        AlertOutcome::Closed => "closed",
        AlertOutcome::NoSignal => "idle",
        AlertOutcome::Unchanged => "unchanged",
        AlertOutcome::Deferred => "deferred",
    };
    let side = event
        .side
        .as_deref()
        .map(|s| format!(" | side {s}"))
        .unwrap_or_default();
    format!(
        "Crypto pairs bot trade {action} | {name} ({bot}) | at_ms {at_ms}{side} | {detail}",
        name = event.display_name,
        bot = event.bot_id,
        at_ms = event.at_ms,
        detail = event.detail,
    )
}

pub async fn send_trade_alert(event: &AlertEvent) -> Result<(), String> {
    if !should_alert_for_outcome(event.outcome) {
        return Ok(());
    }
    let target = std::env::var("PAIRS_ALERT_TARGET").unwrap_or_else(|_| DEFAULT_ALERT_TARGET.to_string());
    let message = format_trade_alert(event);
    let fut = Command::new("hermes")
        .args(["send", "--to", &target, &message])
        .output();
    let out = timeout(ALERT_TIMEOUT, fut)
        .await
        .map_err(|_| format!("timed out after {:?}", ALERT_TIMEOUT))
        .and_then(|r| r.map_err(|e| e.to_string()))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
    }
}
