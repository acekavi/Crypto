use turso::Connection;

use crate::schema::MIGRATIONS;

#[derive(Debug, thiserror::Error)]
pub enum HistoryError {
    #[error("database error: {0}")]
    Db(String),
    #[error("decode error: {0}")]
    Decode(String),
}

impl From<turso::Error> for HistoryError {
    fn from(e: turso::Error) -> Self {
        HistoryError::Db(e.to_string())
    }
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
