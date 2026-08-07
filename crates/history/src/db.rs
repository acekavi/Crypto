use botcore::{Candle, Symbol, Timeframe};
use exchange::bybit::wire::FundingRate;
use rust_decimal::Decimal;
use turso::Connection;

use crate::schema::MIGRATIONS;

#[derive(Debug, thiserror::Error)]
pub enum HistoryError {
    #[error("database error: {0}")]
    Db(String),
    #[error("decode error: {0}")]
    Decode(String),
    /// A fetch from the exchange failed, as distinct from `Db`/`Decode`: a
    /// caller (or a test) needs to tell "the network/API failed" apart from
    /// "our own storage failed" without string-matching an error message.
    #[error("exchange error: {0}")]
    Exchange(String),
}

impl From<turso::Error> for HistoryError {
    fn from(e: turso::Error) -> Self {
        HistoryError::Db(e.to_string())
    }
}

fn parse_dec(s: &str, field: &str) -> Result<Decimal, HistoryError> {
    s.parse::<Decimal>()
        .map_err(|e| HistoryError::Decode(format!("{field}: {e}")))
}

/// The historical-data store: candles, funding rates, and what has actually
/// been downloaded so far. Unlike the trade journal, history has no cloud
/// sync requirement — it is rebuilt from Bybit on demand — so this is
/// local-only.
pub struct HistoryDb {
    // Never read directly; held purely so the underlying database is not
    // dropped while `conn` is still in use.
    #[allow(dead_code)]
    db: turso::Database,
    conn: Connection,
}

impl HistoryDb {
    pub async fn open_local(path: &str) -> Result<Self, HistoryError> {
        let db = turso::Builder::new_local(path).build().await?;
        let conn = db.connect()?;
        let history_db = HistoryDb { db, conn };
        history_db.migrate().await?;
        Ok(history_db)
    }

    async fn migrate(&self) -> Result<(), HistoryError> {
        for stmt in MIGRATIONS {
            self.conn.execute(stmt, ()).await?;
        }
        Ok(())
    }

    pub async fn candle_count(&self) -> Result<i64, HistoryError> {
        self.scalar_i64("SELECT COUNT(*) FROM candles", ()).await
    }

    /// Idempotent batch insert: `INSERT OR REPLACE` on the composite primary
    /// key means a resumed or overlapping download can never create a second
    /// row for one timestamp, and re-running it is always safe.
    ///
    /// `download_ranges` is updated in the same transaction as the candle
    /// rows so a crash mid-write cannot leave the bookkeeping claiming a
    /// range this call did not actually finish writing.
    pub async fn insert_candles(
        &self,
        symbol: &Symbol,
        tf: Timeframe,
        candles: &[Candle],
    ) -> Result<(), HistoryError> {
        if candles.is_empty() {
            return Ok(());
        }

        let tf_str = tf.as_bybit_interval();
        let batch_min = candles
            .iter()
            .map(|c| c.open_time_ms)
            .min()
            .expect("non-empty");
        let batch_max = candles
            .iter()
            .map(|c| c.open_time_ms)
            .max()
            .expect("non-empty");

        // `&self`, not `&mut self`, so `unchecked_transaction` rather than
        // `Connection::transaction` — the latter requires unique access to
        // guarantee no nesting, which this API's shared-reference methods
        // can't offer.
        let tx = self.conn.unchecked_transaction().await?;

        for c in candles {
            tx.execute(
                "INSERT OR REPLACE INTO candles
                 (symbol, timeframe, open_time_ms, open, high, low, close, volume, turnover)
                 VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
                (
                    symbol.as_str().to_string(),
                    tf_str.to_string(),
                    c.open_time_ms,
                    c.open.to_string(),
                    c.high.to_string(),
                    c.low.to_string(),
                    c.close.to_string(),
                    c.volume.to_string(),
                    c.turnover.to_string(),
                ),
            )
            .await?;
        }

        // Read the existing bounds inside this same transaction and merge in
        // Rust rather than via SQL MIN/MAX, so there is no dependency on
        // scalar (as opposed to aggregate) MIN/MAX support in the engine.
        let existing = {
            let mut rows = tx
                .query(
                    "SELECT earliest_ms, latest_ms FROM download_ranges
                     WHERE symbol = ?1 AND timeframe = ?2",
                    (symbol.as_str().to_string(), tf_str.to_string()),
                )
                .await?;
            match rows.next().await? {
                Some(row) => {
                    let earliest = row
                        .get_value(0)
                        .map_err(|e| HistoryError::Db(e.to_string()))?
                        .as_integer()
                        .copied()
                        .ok_or_else(|| {
                            HistoryError::Decode("earliest_ms is not an integer".into())
                        })?;
                    let latest = row
                        .get_value(1)
                        .map_err(|e| HistoryError::Db(e.to_string()))?
                        .as_integer()
                        .copied()
                        .ok_or_else(|| {
                            HistoryError::Decode("latest_ms is not an integer".into())
                        })?;
                    Some((earliest, latest))
                }
                None => None,
            }
        };

        let (earliest_ms, latest_ms) = match existing {
            Some((e, l)) => (e.min(batch_min), l.max(batch_max)),
            None => (batch_min, batch_max),
        };

        tx.execute(
            "INSERT OR REPLACE INTO download_ranges (symbol, timeframe, earliest_ms, latest_ms)
             VALUES (?1, ?2, ?3, ?4)",
            (
                symbol.as_str().to_string(),
                tf_str.to_string(),
                earliest_ms,
                latest_ms,
            ),
        )
        .await?;

        tx.commit().await?;
        Ok(())
    }

    /// Inclusive of both bounds, ascending by `open_time_ms`.
    pub async fn candles_in_range(
        &self,
        symbol: &Symbol,
        tf: Timeframe,
        start_ms: i64,
        end_ms: i64,
    ) -> Result<Vec<Candle>, HistoryError> {
        let mut rows = self
            .conn
            .query(
                "SELECT open_time_ms, open, high, low, close, volume, turnover
                 FROM candles
                 WHERE symbol = ?1 AND timeframe = ?2 AND open_time_ms BETWEEN ?3 AND ?4
                 ORDER BY open_time_ms ASC",
                (
                    symbol.as_str().to_string(),
                    tf.as_bybit_interval().to_string(),
                    start_ms,
                    end_ms,
                ),
            )
            .await?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            let get_text = |i: usize| -> Result<String, HistoryError> {
                row.get_value(i)
                    .map_err(|e| HistoryError::Db(e.to_string()))?
                    .as_text()
                    .map(|s| s.to_string())
                    .ok_or_else(|| HistoryError::Decode(format!("column {i} is not text")))
            };
            out.push(Candle {
                open_time_ms: row
                    .get_value(0)
                    .map_err(|e| HistoryError::Db(e.to_string()))?
                    .as_integer()
                    .copied()
                    .ok_or_else(|| HistoryError::Decode("open_time_ms is not an integer".into()))?,
                open: parse_dec(&get_text(1)?, "open")?,
                high: parse_dec(&get_text(2)?, "high")?,
                low: parse_dec(&get_text(3)?, "low")?,
                close: parse_dec(&get_text(4)?, "close")?,
                volume: parse_dec(&get_text(5)?, "volume")?,
                turnover: parse_dec(&get_text(6)?, "turnover")?,
            });
        }
        Ok(out)
    }

    /// The `(earliest_ms, latest_ms)` this symbol/timeframe has actually had
    /// written for it, or `None` if nothing has been downloaded yet.
    pub async fn recorded_range(
        &self,
        symbol: &Symbol,
        tf: Timeframe,
    ) -> Result<Option<(i64, i64)>, HistoryError> {
        let mut rows = self
            .conn
            .query(
                "SELECT earliest_ms, latest_ms FROM download_ranges
                 WHERE symbol = ?1 AND timeframe = ?2",
                (
                    symbol.as_str().to_string(),
                    tf.as_bybit_interval().to_string(),
                ),
            )
            .await?;
        let Some(row) = rows.next().await? else {
            return Ok(None);
        };
        let earliest = row
            .get_value(0)
            .map_err(|e| HistoryError::Db(e.to_string()))?
            .as_integer()
            .copied()
            .ok_or_else(|| HistoryError::Decode("earliest_ms is not an integer".into()))?;
        let latest = row
            .get_value(1)
            .map_err(|e| HistoryError::Db(e.to_string()))?
            .as_integer()
            .copied()
            .ok_or_else(|| HistoryError::Decode("latest_ms is not an integer".into()))?;
        Ok(Some((earliest, latest)))
    }

    /// Idempotent batch insert: `INSERT OR REPLACE` on `(symbol,
    /// funding_time_ms)` means a resumed or overlapping download can never
    /// duplicate a settlement.
    pub async fn insert_funding(&self, rates: &[FundingRate]) -> Result<(), HistoryError> {
        if rates.is_empty() {
            return Ok(());
        }

        let tx = self.conn.unchecked_transaction().await?;
        for r in rates {
            tx.execute(
                "INSERT OR REPLACE INTO funding_rates (symbol, funding_time_ms, rate)
                 VALUES (?1, ?2, ?3)",
                (
                    r.symbol.as_str().to_string(),
                    r.funding_time_ms,
                    r.rate.to_string(),
                ),
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// Inclusive of both bounds, ascending by `funding_time_ms`.
    pub async fn funding_in_range(
        &self,
        symbol: &Symbol,
        start_ms: i64,
        end_ms: i64,
    ) -> Result<Vec<FundingRate>, HistoryError> {
        let mut rows = self
            .conn
            .query(
                "SELECT funding_time_ms, rate FROM funding_rates
                 WHERE symbol = ?1 AND funding_time_ms BETWEEN ?2 AND ?3
                 ORDER BY funding_time_ms ASC",
                (symbol.as_str().to_string(), start_ms, end_ms),
            )
            .await?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            let funding_time_ms = row
                .get_value(0)
                .map_err(|e| HistoryError::Db(e.to_string()))?
                .as_integer()
                .copied()
                .ok_or_else(|| HistoryError::Decode("funding_time_ms is not an integer".into()))?;
            let rate_text = row
                .get_value(1)
                .map_err(|e| HistoryError::Db(e.to_string()))?
                .as_text()
                .map(|s| s.to_string())
                .ok_or_else(|| HistoryError::Decode("rate is not text".into()))?;
            out.push(FundingRate {
                symbol: symbol.clone(),
                funding_time_ms,
                rate: parse_dec(&rate_text, "rate")?,
            });
        }
        Ok(out)
    }

    /// Sum of `turnover` over the 24 H1 candles ending at `at_ms`, i.e.
    /// `candles_in_range(symbol, H1, at_ms - 24h + H1, at_ms)` — the `+ H1`
    /// is what keeps the window to exactly 24 candles rather than 25.
    ///
    /// Returns `None`, not `Some(0)`, when the window holds no candles at
    /// all: absent data is not evidence of zero turnover, and a caller
    /// ranking symbols must be able to drop such a symbol entirely rather
    /// than rank it at the bottom as if it were genuinely illiquid.
    /// Every `(symbol, timeframe)` series present, with its recorded bounds.
    ///
    /// Reads `download_ranges` rather than scanning `candles`, so it stays
    /// cheap as history grows.
    ///
    /// Ordered by symbol, then by timeframe DURATION — finest first — so a
    /// caller gets a deterministic and unsurprising list. The sort happens in
    /// Rust because `timeframe` is stored as Bybit's interval TEXT, and
    /// `ORDER BY` on it compares lexicographically: `"240"` sorts before
    /// `"60"`, putting H4 ahead of H1. The same TEXT-comparison hazard as the
    /// journal's equity column.
    pub async fn stored_series(&self) -> Result<Vec<(Symbol, Timeframe, i64, i64)>, HistoryError> {
        let mut rows = self
            .conn
            .query(
                "SELECT symbol, timeframe, earliest_ms, latest_ms FROM download_ranges",
                (),
            )
            .await?;

        let mut out = Vec::new();
        while let Some(row) = rows.next().await? {
            let text = |i: usize| -> Result<String, HistoryError> {
                row.get_value(i)
                    .map_err(|e| HistoryError::Db(e.to_string()))?
                    .as_text()
                    .map(|s| s.to_string())
                    .ok_or_else(|| HistoryError::Decode(format!("column {i} is not text")))
            };
            let int = |i: usize| -> Result<i64, HistoryError> {
                row.get_value(i)
                    .map_err(|e| HistoryError::Db(e.to_string()))?
                    .as_integer()
                    .copied()
                    .ok_or_else(|| HistoryError::Decode(format!("column {i} is not an integer")))
            };

            let tf_text = text(1)?;
            // Stored as Bybit's own interval string, so map it back rather
            // than inventing a second encoding that could drift from it.
            let tf = match tf_text.as_str() {
                "60" => Timeframe::H1,
                "240" => Timeframe::H4,
                other => {
                    return Err(HistoryError::Decode(format!(
                        "unknown stored timeframe {other:?}"
                    )));
                }
            };
            out.push((Symbol::new(text(0)?), tf, int(2)?, int(3)?));
        }
        out.sort_by(|a, b| {
            a.0.as_str()
                .cmp(b.0.as_str())
                .then_with(|| a.1.duration_ms().cmp(&b.1.duration_ms()))
        });
        Ok(out)
    }

    pub async fn rolling_turnover_24h(
        &self,
        symbol: &Symbol,
        at_ms: i64,
    ) -> Result<Option<Decimal>, HistoryError> {
        const DAY_MS: i64 = 86_400_000;
        let start_ms = at_ms - DAY_MS + Timeframe::H1.duration_ms();
        let candles = self
            .candles_in_range(symbol, Timeframe::H1, start_ms, at_ms)
            .await?;
        if candles.is_empty() {
            return Ok(None);
        }
        Ok(Some(candles.iter().map(|c| c.turnover).sum()))
    }

    async fn scalar_i64(
        &self,
        sql: &str,
        params: impl turso::params::IntoParams,
    ) -> Result<i64, HistoryError> {
        let mut rows = self.conn.query(sql, params).await?;
        let Some(row) = rows.next().await? else {
            return Ok(0);
        };
        Ok(row
            .get_value(0)
            .map_err(|e| HistoryError::Db(e.to_string()))?
            .as_integer()
            .copied()
            .unwrap_or(0))
    }
}
