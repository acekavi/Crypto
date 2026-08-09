use std::time::Duration;

use botcore::{OrderState, Side, Symbol};
use rust_decimal::Decimal;
use turso::Connection;

use crate::schema::MIGRATIONS;

/// Bound on opening a synced (cloud-backed) journal: build + connect +
/// migrate. Turso Cloud calls are ordinary HTTPS requests over a hostname
/// that can be slow to resolve or simply unreachable; without a bound, an
/// outage or DNS black hole hangs `open_synced` forever, which — per
/// `main.rs`'s documented contract — must instead fall back to a local-only
/// journal within a bounded time. 10s comfortably covers a slow DNS lookup
/// plus TLS handshake plus the handful of migration statements against a
/// healthy remote, while still failing fast enough that a bot operator sees
/// the fallback happen well within a human-noticeable startup delay.
const SYNC_OPEN_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, thiserror::Error)]
pub enum JournalError {
    #[error("database error: {0}")]
    Db(String),
    #[error("decode error: {0}")]
    Decode(String),
    /// Opening a synced journal did not complete within `SYNC_OPEN_TIMEOUT`.
    /// Kept distinct from `Db` so callers (and their logs) can tell "the
    /// remote actively rejected us" apart from "the remote never answered" —
    /// the two point at different operational fixes.
    #[error("timed out after {0:?} opening the synced journal")]
    Timeout(Duration),
}

impl From<turso::Error> for JournalError {
    fn from(e: turso::Error) -> Self {
        JournalError::Db(e.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrderRecord {
    pub order_link_id: String,
    pub order_id: Option<String>,
    pub symbol: Symbol,
    pub side: Side,
    pub price: Decimal,
    pub qty: Decimal,
    pub stop_loss: Decimal,
    pub take_profit: Decimal,
    pub state: OrderState,
    pub cum_exec_qty: Decimal,
    /// SHA-256 of the effective config, so every order is attributable to an
    /// exact ruleset.
    pub config_hash: String,
    pub created_at_ms: i64,
}

/// One row of `stop_protections`: everything the engine needs to keep managing
/// an open position's stop, so a restart can rebuild its in-memory map instead
/// of orphaning the trade.
///
/// Keyed by symbol, because the engine holds at most one position per symbol.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProtectionRecord {
    pub symbol: Symbol,
    pub order_link_id: String,
    pub side: Side,
    /// The stop's current trigger price — rewritten once the stop has moved to
    /// breakeven, so the escalation ladder measures from where it now sits.
    pub trigger: Decimal,
    pub atr: Decimal,
    pub entry_price: Decimal,
    /// Entry to stop-limit distance, fixed when the entry was placed. Moving
    /// the stop later must not change what 1R meant.
    pub initial_risk: Decimal,
    pub stop_limit_offset: Decimal,
    /// `None` means the stop never moves; `Some(0)` would mean "move it to
    /// entry immediately", so the two must never collapse into each other.
    pub breakeven_at_r: Option<Decimal>,
    pub moved_to_breakeven: bool,
    pub updated_at_ms: i64,
}

/// What happened to a trade. One variant per event the audit log records.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TradeEventKind {
    EntryPlaced,
    EntryFilled,
    EntryExpired,
    EntryCancelled,
    StopPlaced,
    StopMovedToBreakeven,
    /// The exchange rejected a stop amend; the in-memory state did not change,
    /// so the engine retries next tick.
    StopAmendFailed,
    StopEscalated,
    StopLadderExhausted,
    PositionClosed,
    /// A protection adopted from the journal at startup, so the audit trail
    /// shows the restart.
    ProtectionRestored,
    HaltSet,
    HaltCleared,
}

impl TradeEventKind {
    /// Every variant. Iterated by the round-trip test, so a variant added
    /// without being listed here is caught rather than silently unparseable.
    pub const ALL: [TradeEventKind; 13] = [
        TradeEventKind::EntryPlaced,
        TradeEventKind::EntryFilled,
        TradeEventKind::EntryExpired,
        TradeEventKind::EntryCancelled,
        TradeEventKind::StopPlaced,
        TradeEventKind::StopMovedToBreakeven,
        TradeEventKind::StopAmendFailed,
        TradeEventKind::StopEscalated,
        TradeEventKind::StopLadderExhausted,
        TradeEventKind::PositionClosed,
        TradeEventKind::ProtectionRestored,
        TradeEventKind::HaltSet,
        TradeEventKind::HaltCleared,
    ];

    /// The stored form. Stable: it is written into rows that are never
    /// rewritten, so renaming one would orphan every event already logged.
    pub fn as_str(self) -> &'static str {
        match self {
            TradeEventKind::EntryPlaced => "EntryPlaced",
            TradeEventKind::EntryFilled => "EntryFilled",
            TradeEventKind::EntryExpired => "EntryExpired",
            TradeEventKind::EntryCancelled => "EntryCancelled",
            TradeEventKind::StopPlaced => "StopPlaced",
            TradeEventKind::StopMovedToBreakeven => "StopMovedToBreakeven",
            TradeEventKind::StopAmendFailed => "StopAmendFailed",
            TradeEventKind::StopEscalated => "StopEscalated",
            TradeEventKind::StopLadderExhausted => "StopLadderExhausted",
            TradeEventKind::PositionClosed => "PositionClosed",
            TradeEventKind::ProtectionRestored => "ProtectionRestored",
            TradeEventKind::HaltSet => "HaltSet",
            TradeEventKind::HaltCleared => "HaltCleared",
        }
    }
}

/// Parsing is `FromStr` rather than an inherent `from_str` so it composes with
/// `str::parse`; the error is a `JournalError::Decode` because the only thing
/// that ever parses one of these is a row coming back out of the log.
impl std::str::FromStr for TradeEventKind {
    type Err = JournalError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        TradeEventKind::ALL
            .into_iter()
            .find(|k| k.as_str() == s)
            .ok_or_else(|| JournalError::Decode(format!("unknown trade event kind {s}")))
    }
}

/// One row of `trade_events`. Append-only: written once, never updated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TradeEvent {
    pub at_ms: i64,
    pub symbol: Symbol,
    /// `None` for events that are not about one order, such as a halt.
    pub order_link_id: Option<String>,
    pub kind: TradeEventKind,
    /// Free text for humans, e.g. `"stop 61234.5 -> 62000.0 (entry)"`. Never
    /// parsed back — anything a machine needs belongs in its own column.
    pub detail: String,
    /// SHA-256 of the effective config, so every event is attributable to an
    /// exact ruleset.
    pub config_hash: String,
}

fn state_str(s: OrderState) -> &'static str {
    match s {
        OrderState::New => "New",
        OrderState::PartiallyFilled => "PartiallyFilled",
        OrderState::Filled => "Filled",
        OrderState::Cancelled => "Cancelled",
        OrderState::Rejected => "Rejected",
    }
}

fn parse_state(s: &str) -> Result<OrderState, JournalError> {
    Ok(match s {
        "New" => OrderState::New,
        "PartiallyFilled" => OrderState::PartiallyFilled,
        "Filled" => OrderState::Filled,
        "Cancelled" => OrderState::Cancelled,
        "Rejected" => OrderState::Rejected,
        other => return Err(JournalError::Decode(format!("unknown order state {other}"))),
    })
}

fn parse_side(s: &str) -> Result<Side, JournalError> {
    Ok(match s {
        "Buy" => Side::Buy,
        "Sell" => Side::Sell,
        other => return Err(JournalError::Decode(format!("unknown side {other}"))),
    })
}

fn parse_dec(s: &str, field: &str) -> Result<Decimal, JournalError> {
    s.parse::<Decimal>()
        .map_err(|e| JournalError::Decode(format!("{field}: {e}")))
}

/// The two turso database handle shapes.
///
/// The installed `turso` 0.7.2 API splits local and cloud-synced databases
/// into two distinct builder/database types (`turso::Builder`/`turso::Database`
/// for local-only, `turso::sync::Builder`/`turso::sync::Database` for a local
/// file kept in sync with Turso Cloud) rather than one `Database` type with
/// both a `new_local` and `new_remote` constructor. Both database types hand
/// out the same `turso::Connection`, so all reads and writes below go through
/// a single `Connection` regardless of which variant opened it; this enum
/// only exists to keep the right handle alive and to know whether `push` is
/// meaningful. The `Local` variant's `Database` is never read directly — it
/// is held purely so the underlying database is not dropped while `conn` is
/// still in use.
#[allow(dead_code)]
enum DbHandle {
    Local(turso::Database),
    Synced(turso::sync::Database),
}

/// The trade journal. All reads and writes hit the local database file; a
/// background task pushes to Turso cloud separately, so the cloud is never in
/// the order path.
pub struct Journal {
    db: DbHandle,
    conn: Connection,
}

impl Journal {
    pub async fn open_local(path: &str) -> Result<Self, JournalError> {
        let db = turso::Builder::new_local(path).build().await?;
        let conn = db.connect()?;
        let j = Journal {
            db: DbHandle::Local(db),
            conn,
        };
        j.migrate().await?;
        Ok(j)
    }

    pub async fn open_synced(path: &str, url: &str, token: &str) -> Result<Self, JournalError> {
        match tokio::time::timeout(SYNC_OPEN_TIMEOUT, Self::open_synced_inner(path, url, token))
            .await
        {
            Ok(result) => result,
            Err(_elapsed) => Err(JournalError::Timeout(SYNC_OPEN_TIMEOUT)),
        }
    }

    /// The unbounded build+connect+migrate sequence, split out so
    /// `open_synced` can wrap the whole thing — not just the network build —
    /// in one timeout. An unreachable or slow-DNS host can hang inside
    /// `build()` indefinitely otherwise, which per `main.rs`'s contract must
    /// never be a reason the bot refuses to start.
    async fn open_synced_inner(path: &str, url: &str, token: &str) -> Result<Self, JournalError> {
        let db = turso::sync::Builder::new_remote(path)
            .with_remote_url(url)
            .with_auth_token(token)
            .build()
            .await?;
        let conn = db.connect().await?;
        let j = Journal {
            db: DbHandle::Synced(db),
            conn,
        };
        j.migrate().await?;
        Ok(j)
    }

    async fn migrate(&self) -> Result<(), JournalError> {
        for stmt in MIGRATIONS {
            self.conn.execute(stmt, ()).await?;
        }
        Ok(())
    }

    /// Push local changes to Turso cloud. Errors are returned, never
    /// propagated into trading logic — the caller logs and retries.
    pub async fn push(&self) -> Result<(), JournalError> {
        match &self.db {
            DbHandle::Local(_) => Ok(()),
            DbHandle::Synced(db) => {
                db.push().await?;
                Ok(())
            }
        }
    }

    /// Insert an order. Idempotent on `order_link_id`: a retried placement can
    /// never create a second row.
    pub async fn record_order(&self, o: &OrderRecord) -> Result<(), JournalError> {
        self.conn
            .execute(
                "INSERT OR IGNORE INTO orders
                 (order_link_id, order_id, symbol, side, price, qty, stop_loss,
                  take_profit, state, cum_exec_qty, config_hash, created_at_ms)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
                (
                    o.order_link_id.clone(),
                    o.order_id.clone(),
                    o.symbol.as_str().to_string(),
                    o.side.as_bybit().to_string(),
                    o.price.to_string(),
                    o.qty.to_string(),
                    o.stop_loss.to_string(),
                    o.take_profit.to_string(),
                    state_str(o.state).to_string(),
                    o.cum_exec_qty.to_string(),
                    o.config_hash.clone(),
                    o.created_at_ms,
                ),
            )
            .await?;
        Ok(())
    }

    /// Update an order's state and cumulative filled quantity.
    ///
    /// `filled_at_ms` records when the fill actually happened — pass
    /// `Some(now)` when `state` becomes `Filled` or `PartiallyFilled`, and
    /// `None` for every other transition (e.g. `Cancelled`, `Rejected`).
    /// When `None`, the existing `filled_at_ms` column is left untouched
    /// rather than set to NULL, so a later no-fill-time update can never
    /// erase a fill time already recorded by an earlier call.
    pub async fn update_order_state(
        &self,
        link_id: &str,
        state: OrderState,
        cum_exec_qty: Decimal,
        filled_at_ms: Option<i64>,
    ) -> Result<(), JournalError> {
        match filled_at_ms {
            Some(ms) => {
                self.conn
                    .execute(
                        "UPDATE orders SET state = ?1, cum_exec_qty = ?2, filled_at_ms = ?3
                         WHERE order_link_id = ?4",
                        (
                            state_str(state).to_string(),
                            cum_exec_qty.to_string(),
                            ms,
                            link_id.to_string(),
                        ),
                    )
                    .await?;
            }
            None => {
                self.conn
                    .execute(
                        "UPDATE orders SET state = ?1, cum_exec_qty = ?2 WHERE order_link_id = ?3",
                        (
                            state_str(state).to_string(),
                            cum_exec_qty.to_string(),
                            link_id.to_string(),
                        ),
                    )
                    .await?;
            }
        }
        Ok(())
    }

    pub async fn order_by_link_id(
        &self,
        link_id: &str,
    ) -> Result<Option<OrderRecord>, JournalError> {
        let mut rows = self
            .conn
            .query(
                "SELECT order_link_id, order_id, symbol, side, price, qty, stop_loss,
                        take_profit, state, cum_exec_qty, config_hash, created_at_ms
                 FROM orders WHERE order_link_id = ?1",
                (link_id.to_string(),),
            )
            .await?;

        let Some(row) = rows.next().await? else {
            return Ok(None);
        };
        let get_text = |i: usize| -> Result<String, JournalError> {
            row.get_value(i)
                .map_err(|e| JournalError::Db(e.to_string()))?
                .as_text()
                .map(|s| s.to_string())
                .ok_or_else(|| JournalError::Decode(format!("column {i} is not text")))
        };

        Ok(Some(OrderRecord {
            order_link_id: get_text(0)?,
            order_id: row
                .get_value(1)
                .ok()
                .and_then(|v| v.as_text().map(|s| s.to_string())),
            symbol: Symbol::new(get_text(2)?),
            side: parse_side(&get_text(3)?)?,
            price: parse_dec(&get_text(4)?, "price")?,
            qty: parse_dec(&get_text(5)?, "qty")?,
            stop_loss: parse_dec(&get_text(6)?, "stop_loss")?,
            take_profit: parse_dec(&get_text(7)?, "take_profit")?,
            state: parse_state(&get_text(8)?)?,
            cum_exec_qty: parse_dec(&get_text(9)?, "cum_exec_qty")?,
            config_hash: get_text(10)?,
            created_at_ms: row
                .get_value(11)
                .map_err(|e| JournalError::Db(e.to_string()))?
                .as_integer()
                .copied()
                .ok_or_else(|| JournalError::Decode("created_at_ms is not an integer".into()))?,
        }))
    }

    pub async fn order_count(&self) -> Result<i64, JournalError> {
        self.scalar_i64("SELECT COUNT(*) FROM orders", ()).await
    }

    /// Filled entries within one UTC day, keyed by when the fill happened
    /// (`filled_at_ms`), not when the order was placed (`created_at_ms`).
    /// Counting fills rather than placements is what makes cancelled limit
    /// orders free of budget cost; using the fill time rather than the
    /// placement time is what makes a resting PostOnly limit that fills
    /// after crossing midnight UTC charge against the day it actually
    /// filled, not the day it was placed.
    pub async fn daily_fill_count(&self, utc_day_start_ms: i64) -> Result<i64, JournalError> {
        self.scalar_i64(
            "SELECT COUNT(*) FROM orders
             WHERE state = 'Filled' AND filled_at_ms >= ?1 AND filled_at_ms < ?2",
            (utc_day_start_ms, utc_day_start_ms + 86_400_000),
        )
        .await
    }

    /// Write a protection, replacing any row already held for that symbol.
    ///
    /// Called on every mutation of the engine's in-memory map, so the table is
    /// authoritative at all times rather than a periodic snapshot.
    pub async fn upsert_protection(&self, p: &ProtectionRecord) -> Result<(), JournalError> {
        self.conn
            .execute(
                "INSERT INTO stop_protections
                 (symbol, order_link_id, side, trigger, atr, entry_price, initial_risk,
                  stop_limit_offset, breakeven_at_r, moved_to_breakeven, updated_at_ms)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
                 ON CONFLICT(symbol) DO UPDATE SET
                     order_link_id = excluded.order_link_id,
                     side = excluded.side,
                     trigger = excluded.trigger,
                     atr = excluded.atr,
                     entry_price = excluded.entry_price,
                     initial_risk = excluded.initial_risk,
                     stop_limit_offset = excluded.stop_limit_offset,
                     breakeven_at_r = excluded.breakeven_at_r,
                     moved_to_breakeven = excluded.moved_to_breakeven,
                     updated_at_ms = excluded.updated_at_ms",
                (
                    p.symbol.as_str().to_string(),
                    p.order_link_id.clone(),
                    p.side.as_bybit().to_string(),
                    p.trigger.to_string(),
                    p.atr.to_string(),
                    p.entry_price.to_string(),
                    p.initial_risk.to_string(),
                    p.stop_limit_offset.to_string(),
                    p.breakeven_at_r.map(|d| d.to_string()),
                    i64::from(p.moved_to_breakeven),
                    p.updated_at_ms,
                ),
            )
            .await?;
        Ok(())
    }

    /// Drop a symbol's protection — the position is gone.
    pub async fn delete_protection(&self, symbol: &Symbol) -> Result<(), JournalError> {
        self.conn
            .execute(
                "DELETE FROM stop_protections WHERE symbol = ?1",
                (symbol.as_str().to_string(),),
            )
            .await?;
        Ok(())
    }

    /// Every protection on record, for rebuilding the engine's map at startup.
    ///
    /// Unordered on purpose: the caller keys these by symbol, and the Decimal
    /// columns are TEXT, so no SQL ordering over them would be meaningful.
    pub async fn load_protections(&self) -> Result<Vec<ProtectionRecord>, JournalError> {
        let mut rows = self
            .conn
            .query(
                "SELECT symbol, order_link_id, side, trigger, atr, entry_price, initial_risk,
                        stop_limit_offset, breakeven_at_r, moved_to_breakeven, updated_at_ms
                 FROM stop_protections",
                (),
            )
            .await?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            let get_text = |i: usize| -> Result<String, JournalError> {
                row.get_value(i)
                    .map_err(|e| JournalError::Db(e.to_string()))?
                    .as_text()
                    .map(|s| s.to_string())
                    .ok_or_else(|| JournalError::Decode(format!("column {i} is not text")))
            };
            let get_int = |i: usize, field: &str| -> Result<i64, JournalError> {
                row.get_value(i)
                    .map_err(|e| JournalError::Db(e.to_string()))?
                    .as_integer()
                    .copied()
                    .ok_or_else(|| JournalError::Decode(format!("{field} is not an integer")))
            };

            let breakeven = row
                .get_value(8)
                .map_err(|e| JournalError::Db(e.to_string()))?;
            let breakeven_at_r = match breakeven.as_text() {
                Some(s) => Some(parse_dec(s, "breakeven_at_r")?),
                None => None,
            };

            out.push(ProtectionRecord {
                symbol: Symbol::new(get_text(0)?),
                order_link_id: get_text(1)?,
                side: parse_side(&get_text(2)?)?,
                trigger: parse_dec(&get_text(3)?, "trigger")?,
                atr: parse_dec(&get_text(4)?, "atr")?,
                entry_price: parse_dec(&get_text(5)?, "entry_price")?,
                initial_risk: parse_dec(&get_text(6)?, "initial_risk")?,
                stop_limit_offset: parse_dec(&get_text(7)?, "stop_limit_offset")?,
                breakeven_at_r,
                moved_to_breakeven: get_int(9, "moved_to_breakeven")? != 0,
                updated_at_ms: get_int(10, "updated_at_ms")?,
            });
        }
        Ok(out)
    }

    /// Append one lifecycle event. Never idempotent: two identical events
    /// really did happen twice, and the log's job is to say so.
    pub async fn record_event(&self, e: &TradeEvent) -> Result<(), JournalError> {
        self.conn
            .execute(
                "INSERT INTO trade_events
                 (at_ms, symbol, order_link_id, kind, detail, config_hash)
                 VALUES (?1,?2,?3,?4,?5,?6)",
                (
                    e.at_ms,
                    e.symbol.as_str().to_string(),
                    e.order_link_id.clone(),
                    e.kind.as_str().to_string(),
                    e.detail.clone(),
                    e.config_hash.clone(),
                ),
            )
            .await?;
        Ok(())
    }

    /// One symbol's events, oldest first.
    ///
    /// Ordered by `at_ms` then `id`: both are INTEGER, so SQL compares them
    /// numerically, and the `id` tiebreak keeps insertion order for events
    /// written within the same millisecond.
    pub async fn events_for(&self, symbol: &Symbol) -> Result<Vec<TradeEvent>, JournalError> {
        let mut rows = self
            .conn
            .query(
                "SELECT at_ms, symbol, order_link_id, kind, detail, config_hash
                 FROM trade_events WHERE symbol = ?1 ORDER BY at_ms ASC, id ASC",
                (symbol.as_str().to_string(),),
            )
            .await?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            let get_text = |i: usize| -> Result<String, JournalError> {
                row.get_value(i)
                    .map_err(|e| JournalError::Db(e.to_string()))?
                    .as_text()
                    .map(|s| s.to_string())
                    .ok_or_else(|| JournalError::Decode(format!("column {i} is not text")))
            };

            out.push(TradeEvent {
                at_ms: row
                    .get_value(0)
                    .map_err(|e| JournalError::Db(e.to_string()))?
                    .as_integer()
                    .copied()
                    .ok_or_else(|| JournalError::Decode("at_ms is not an integer".into()))?,
                symbol: Symbol::new(get_text(1)?),
                order_link_id: row
                    .get_value(2)
                    .ok()
                    .and_then(|v| v.as_text().map(|s| s.to_string())),
                kind: get_text(3)?.parse()?,
                detail: get_text(4)?,
                config_hash: get_text(5)?,
            });
        }
        Ok(out)
    }

    pub async fn event_count(&self) -> Result<i64, JournalError> {
        self.scalar_i64("SELECT COUNT(*) FROM trade_events", ())
            .await
    }

    pub async fn record_equity(&self, equity: Decimal, at_ms: i64) -> Result<(), JournalError> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO equity_snapshots (at_ms, equity) VALUES (?1, ?2)",
                (at_ms, equity.to_string()),
            )
            .await?;
        Ok(())
    }

    /// The highest equity ever recorded.
    ///
    /// The total-drawdown halt measures against the all-time peak, so this must
    /// survive restarts — otherwise the baseline silently resets to whatever
    /// equity exists at startup and the halt fires far later than intended.
    ///
    /// Equity is stored as TEXT (see `schema`), so SQL's `MAX(equity)` would
    /// compare lexicographically — `"9" > "10000"` — and could silently
    /// corrupt the peak. This fetches every snapshot and compares as
    /// `Decimal` in Rust instead of adding a parallel numeric column to keep
    /// in sync: the comparison only runs once, at startup, never on the
    /// per-candle write path, so the O(n) scan costs nothing that matters,
    /// and there is no derived column that could ever drift from the TEXT
    /// value it mirrors.
    pub async fn high_water_mark(&self) -> Result<Option<Decimal>, JournalError> {
        let mut rows = self
            .conn
            .query("SELECT equity FROM equity_snapshots", ())
            .await?;
        let mut peak: Option<Decimal> = None;
        while let Some(row) = rows.next().await? {
            let text = row
                .get_value(0)
                .map_err(|e| JournalError::Db(e.to_string()))?
                .as_text()
                .map(|s| s.to_string())
                .ok_or_else(|| JournalError::Decode("equity is not text".into()))?;
            let equity = parse_dec(&text, "equity")?;
            peak = Some(match peak {
                Some(p) if p >= equity => p,
                _ => equity,
            });
        }
        Ok(peak)
    }

    /// Equity as of the first snapshot at or after `day_start_ms`.
    ///
    /// The daily-drawdown baseline. Returns None when the day has no snapshot
    /// yet, which the caller treats as "capture the current equity now".
    ///
    /// Ordering here is on `at_ms`, an INTEGER column, so SQL's `ORDER BY`
    /// compares numerically already — the TEXT lexicographic hazard above
    /// only applies to comparing `equity` values against each other, which
    /// this method never does.
    pub async fn day_start_equity(
        &self,
        day_start_ms: i64,
    ) -> Result<Option<Decimal>, JournalError> {
        let mut rows = self
            .conn
            .query(
                "SELECT equity FROM equity_snapshots
                 WHERE at_ms >= ?1 ORDER BY at_ms ASC LIMIT 1",
                (day_start_ms,),
            )
            .await?;
        let Some(row) = rows.next().await? else {
            return Ok(None);
        };
        let text = row
            .get_value(0)
            .map_err(|e| JournalError::Db(e.to_string()))?
            .as_text()
            .map(|s| s.to_string())
            .ok_or_else(|| JournalError::Decode("equity is not text".into()))?;
        Ok(Some(parse_dec(&text, "equity")?))
    }

    pub async fn set_halt(&self, reason: &str, set_at_ms: i64) -> Result<(), JournalError> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO halt_state (id, reason, set_at_ms) VALUES (1, ?1, ?2)",
                (reason.to_string(), set_at_ms),
            )
            .await?;
        Ok(())
    }

    pub async fn clear_halt(&self) -> Result<(), JournalError> {
        self.conn
            .execute("DELETE FROM halt_state WHERE id = 1", ())
            .await?;
        Ok(())
    }

    pub async fn halt_reason(&self) -> Result<Option<String>, JournalError> {
        let mut rows = self
            .conn
            .query("SELECT reason FROM halt_state WHERE id = 1", ())
            .await?;
        let Some(row) = rows.next().await? else {
            return Ok(None);
        };
        Ok(row
            .get_value(0)
            .map_err(|e| JournalError::Db(e.to_string()))?
            .as_text()
            .map(|s| s.to_string()))
    }

    async fn scalar_i64(
        &self,
        sql: &str,
        params: impl turso::params::IntoParams,
    ) -> Result<i64, JournalError> {
        let mut rows = self.conn.query(sql, params).await?;
        let Some(row) = rows.next().await? else {
            return Ok(0);
        };
        Ok(row
            .get_value(0)
            .map_err(|e| JournalError::Db(e.to_string()))?
            .as_integer()
            .copied()
            .unwrap_or(0))
    }
}
