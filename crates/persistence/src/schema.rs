/// Table definitions, applied idempotently on every open.
///
/// Decimals are stored as TEXT rather than REAL: SQLite REAL is a float, and
/// round-tripping a price through it would defeat the Decimal discipline the
/// rest of the system maintains.
pub const MIGRATIONS: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS orders (
        order_link_id TEXT PRIMARY KEY,
        order_id      TEXT,
        symbol        TEXT NOT NULL,
        side          TEXT NOT NULL,
        price         TEXT NOT NULL,
        qty           TEXT NOT NULL,
        stop_loss     TEXT NOT NULL,
        take_profit   TEXT NOT NULL,
        state         TEXT NOT NULL,
        cum_exec_qty  TEXT NOT NULL,
        config_hash   TEXT NOT NULL,
        created_at_ms INTEGER NOT NULL
    )",
    "CREATE INDEX IF NOT EXISTS idx_orders_created ON orders(created_at_ms)",
    "CREATE INDEX IF NOT EXISTS idx_orders_state ON orders(state)",
    "CREATE TABLE IF NOT EXISTS fills (
        id            INTEGER PRIMARY KEY AUTOINCREMENT,
        order_link_id TEXT NOT NULL,
        price         TEXT NOT NULL,
        qty           TEXT NOT NULL,
        fee           TEXT NOT NULL,
        filled_at_ms  INTEGER NOT NULL
    )",
    "CREATE TABLE IF NOT EXISTS equity_snapshots (
        at_ms  INTEGER PRIMARY KEY,
        equity TEXT NOT NULL
    )",
    "CREATE TABLE IF NOT EXISTS halt_state (
        id       INTEGER PRIMARY KEY CHECK (id = 1),
        reason   TEXT NOT NULL,
        set_at_ms INTEGER NOT NULL
    )",
    "CREATE TABLE IF NOT EXISTS candles (
        symbol       TEXT NOT NULL,
        timeframe    TEXT NOT NULL,
        open_time_ms INTEGER NOT NULL,
        open TEXT NOT NULL, high TEXT NOT NULL, low TEXT NOT NULL, close TEXT NOT NULL,
        volume TEXT NOT NULL, turnover TEXT NOT NULL,
        PRIMARY KEY (symbol, timeframe, open_time_ms)
    )",
];
