/// Applied in order on every open. Each is `IF NOT EXISTS`, so opening an
/// existing database is a no-op rather than an error.
pub const MIGRATIONS: &[&str] = &[
    // Decimals are TEXT for the same reason as the trading journal: SQLite
    // REAL is binary floating point and would silently round prices.
    "CREATE TABLE IF NOT EXISTS candles (
        symbol TEXT NOT NULL,
        timeframe TEXT NOT NULL,
        open_time_ms INTEGER NOT NULL,
        open TEXT NOT NULL,
        high TEXT NOT NULL,
        low TEXT NOT NULL,
        close TEXT NOT NULL,
        volume TEXT NOT NULL,
        turnover TEXT NOT NULL,
        PRIMARY KEY (symbol, timeframe, open_time_ms)
    )",
    "CREATE TABLE IF NOT EXISTS funding_rates (
        symbol TEXT NOT NULL,
        funding_time_ms INTEGER NOT NULL,
        rate TEXT NOT NULL,
        PRIMARY KEY (symbol, funding_time_ms)
    )",
    // What has actually been fetched, so an interrupted download resumes
    // instead of restarting or leaving an unnoticed hole.
    "CREATE TABLE IF NOT EXISTS download_ranges (
        symbol TEXT NOT NULL,
        timeframe TEXT NOT NULL,
        earliest_ms INTEGER NOT NULL,
        latest_ms INTEGER NOT NULL,
        PRIMARY KEY (symbol, timeframe)
    )",
];
