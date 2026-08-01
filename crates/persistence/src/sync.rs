use std::sync::Arc;
use std::time::Duration;

use tracing::warn;

use crate::journal::Journal;

/// Push the local journal to Turso cloud on an interval.
///
/// Failures are logged and retried on the next tick. They are deliberately not
/// surfaced to the caller: an unreachable cloud must never stop the bot from
/// trading or from recording state locally.
pub fn spawn_sync_task(journal: Arc<Journal>, interval: Duration) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        loop {
            ticker.tick().await;
            if let Err(e) = journal.push().await {
                warn!(error = %e, "turso sync failed; will retry on next tick");
            }
        }
    });
}
