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
        created_at_ms INTEGER NOT NULL,
        filled_at_ms  INTEGER
    )",
    "CREATE INDEX IF NOT EXISTS idx_orders_created ON orders(created_at_ms)",
    "CREATE INDEX IF NOT EXISTS idx_orders_state ON orders(state)",
    "CREATE INDEX IF NOT EXISTS idx_orders_filled_at ON orders(filled_at_ms)",
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
    // Mirrors `EngineLoop::protections`, written on every mutation so the
    // database is authoritative rather than a periodic snapshot. `symbol` is
    // the primary key because the engine holds at most one position per
    // symbol, so an upsert on it can never create a duplicate.
    "CREATE TABLE IF NOT EXISTS stop_protections (
        symbol             TEXT PRIMARY KEY,
        order_link_id      TEXT NOT NULL,
        side               TEXT NOT NULL,
        trigger            TEXT NOT NULL,
        atr                TEXT NOT NULL,
        entry_price        TEXT NOT NULL,
        initial_risk       TEXT NOT NULL,
        stop_limit_offset  TEXT NOT NULL,
        breakeven_at_r     TEXT,
        moved_to_breakeven INTEGER NOT NULL,
        updated_at_ms      INTEGER NOT NULL
    )",
    // Append-only audit log: one row per lifecycle event, never updated and
    // never deleted. `at_ms` is INTEGER, so ordering it in SQL is safe — it is
    // a timestamp, not a Decimal.
    "CREATE TABLE IF NOT EXISTS trade_events (
        id            INTEGER PRIMARY KEY AUTOINCREMENT,
        at_ms         INTEGER NOT NULL,
        symbol        TEXT NOT NULL,
        order_link_id TEXT,
        kind          TEXT NOT NULL,
        detail        TEXT NOT NULL,
        config_hash   TEXT NOT NULL
    )",
    "CREATE INDEX IF NOT EXISTS idx_trade_events_symbol ON trade_events(symbol, at_ms)",
    // One row per bot: a pair is flat or it is not, so there is nothing to
    // accumulate. Decimals are TEXT, matching every other table here — never
    // ORDER BY them; order by the integer timestamp.
    "CREATE TABLE IF NOT EXISTS pair_positions (
        bot_id TEXT PRIMARY KEY,
        side TEXT NOT NULL,
        opened_at_ms INTEGER NOT NULL,
        entry_z TEXT NOT NULL,
        a_symbol TEXT NOT NULL,
        a_qty TEXT NOT NULL,
        a_entry TEXT NOT NULL,
        a_order_id TEXT NOT NULL,
        b_symbol TEXT NOT NULL,
        b_qty TEXT NOT NULL,
        b_entry TEXT NOT NULL,
        b_order_id TEXT NOT NULL,
        breakeven_armed INTEGER NOT NULL,
        per_leg_notional TEXT NOT NULL,
        capped_by TEXT
    )",
    // Append-only narrative: entries, exits, guard deferrals, unwinds, halts.
    "CREATE TABLE IF NOT EXISTS pair_events (
        id INTEGER PRIMARY KEY AUTOINCREMENT,
        bot_id TEXT NOT NULL,
        at_ms INTEGER NOT NULL,
        kind TEXT NOT NULL,
        detail TEXT NOT NULL
    )",
    "CREATE INDEX IF NOT EXISTS idx_pair_events_bot_at ON pair_events (bot_id, at_ms)",
    // Liveness, replacing the JSON state file's heartbeat fields. Separate
    // from pair_positions because it is written every loop while a position
    // changes rarely, and because a heartbeat write must never risk a
    // position row.
    "CREATE TABLE IF NOT EXISTS pair_heartbeats (
        bot_id TEXT PRIMARY KEY,
        last_bar_ms INTEGER,
        last_loop_ms INTEGER,
        last_z TEXT,
        last_signal TEXT,
        last_guard_reason TEXT
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
