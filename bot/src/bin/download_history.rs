//! `download-history`: fetches Bybit historical klines into `HistoryDb` so a
//! backtest has candles to run over.
//!
//! Public market-data endpoints (`/v5/market/kline`) don't check the request
//! signature — confirmed in Task 4/5 of the history-layer plan — so this
//! binary never asks for `BYBIT_API_KEY`/`BYBIT_API_SECRET`. Only the trading
//! binary (`crypto-bot`) needs real credentials.

use bot::config::Profile;
use botcore::{Symbol, Timeframe};
use exchange::bybit::rest::BybitRest;
use exchange::bybit::sign::{Credentials, local_now_ms};
use history::{HistoryDb, download_symbol};
use tracing::{error, info, warn};

const DAY_MS: i64 = 86_400_000;
/// Both timeframes the live strategy consumes (see `bot/src/main.rs`'s
/// `strategy_timeframes`) — a backtest needs the same history the live bot
/// warms up on.
const TIMEFRAMES: [Timeframe; 2] = [Timeframe::H1, Timeframe::H4];

struct Args {
    symbols: Vec<Symbol>,
    days: i64,
    profile: Profile,
    db_path: String,
}

fn parse_args() -> Result<Args, Box<dyn std::error::Error>> {
    let mut symbols = Vec::new();
    let mut days: Option<i64> = None;
    let mut profile_name = "testnet".to_string();
    let mut db_path = "data/history.db".to_string();

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--symbol" => {
                let v = args.next().ok_or("--symbol requires a value")?;
                symbols.push(Symbol::new(v));
            }
            "--days" => {
                let v = args.next().ok_or("--days requires a value")?;
                days = Some(
                    v.parse::<i64>()
                        .map_err(|_| format!("--days value \"{v}\" is not an integer"))?,
                );
            }
            "--profile" => {
                profile_name = args.next().ok_or("--profile requires a value")?;
            }
            "--db" => {
                db_path = args.next().ok_or("--db requires a value")?;
            }
            other => return Err(format!("unrecognised argument: {other}").into()),
        }
    }

    if symbols.is_empty() {
        return Err("at least one --symbol is required".into());
    }
    let days = days.ok_or("--days is required")?;
    if days <= 0 {
        return Err(format!("--days must be positive, got {days}").into());
    }
    let profile = Profile::from_name(&profile_name)?;

    Ok(Args {
        symbols,
        days,
        profile,
        db_path,
    })
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    // See main.rs: two TLS crypto providers compiled in (reqwest and
    // tokio-tungstenite) makes rustls panic on first use rather than fail at
    // startup unless one is chosen explicitly before anything opens a
    // connection. This binary only opens REST connections, but installing it
    // unconditionally keeps the two binaries' startup sequences identical.
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("no rustls crypto provider may be installed before this point");

    let args = parse_args()?;

    info!(
        profile = args.profile.name(),
        symbols = ?args.symbols.iter().map(Symbol::as_str).collect::<Vec<_>>(),
        days = args.days,
        "starting historical download"
    );

    // Public kline/funding endpoints don't validate the signature, so empty
    // credentials work — `BybitRest::new` still requires a `Credentials`
    // value structurally, but nothing here reads `BYBIT_API_KEY`/`_SECRET`.
    let rest = BybitRest::new(
        args.profile.rest_base_url().to_string(),
        Credentials {
            api_key: String::new(),
            api_secret: String::new(),
        },
    );

    // turso::Builder::new_local expects the parent directory to already
    // exist; create it so a fresh checkout can run without a manual
    // `mkdir -p data` first (matches main.rs).
    std::fs::create_dir_all("data")
        .map_err(|e| format!("failed to create data directory \"data\": {e}"))?;
    let db = HistoryDb::open_local(&args.db_path).await?;

    let end_ms = local_now_ms();
    let start_ms = end_ms - args.days * DAY_MS;

    let mut total_candles_written = 0usize;
    let mut total_gaps = 0usize;
    let mut total_funding_rows = 0usize;

    for symbol in &args.symbols {
        // Funding first: a backtest that finds no funding rows charges ZERO
        // funding and silently understates the cost of every multi-day hold,
        // which is exactly what this strategy does. `SimulatedExchange` reads
        // these from the database, so an empty table looks identical to a
        // market with no funding at all.
        match rest.funding_history(symbol, start_ms, end_ms).await {
            Ok(rates) => {
                db.insert_funding(&rates).await?;
                total_funding_rows += rates.len();
                info!(%symbol, rows = rates.len(), "funding history stored");
            }
            // Not fatal: candles are still worth having, and the backtest
            // reports its own funding total so a zero is visible there too.
            Err(e) => warn!(%symbol, error = %e, "funding history download failed"),
        }

        for &tf in &TIMEFRAMES {
            let report = download_symbol(&rest, &db, symbol, tf, start_ms, end_ms).await?;
            info!(
                %symbol,
                ?tf,
                candles_written = report.candles_written,
                gaps = report.gaps.len(),
                "symbol/timeframe download complete"
            );
            for gap in &report.gaps {
                warn!(
                    %symbol, ?tf,
                    from_ms = gap.from_ms, to_ms = gap.to_ms,
                    "gap in stored history — not filled, needs a later re-run"
                );
            }
            total_candles_written += report.candles_written;
            total_gaps += report.gaps.len();
        }
    }

    info!(
        symbols = args.symbols.len(),
        candles_written = total_candles_written,
        funding_rows = total_funding_rows,
        gaps = total_gaps,
        "historical download complete"
    );
    if total_funding_rows == 0 {
        error!(
            "no funding rows stored — a backtest over this data would charge zero \
             funding and understate the cost of every position held across an 8h period"
        );
    }
    if total_gaps > 0 {
        error!(
            gaps = total_gaps,
            "download finished with unresolved gaps — see warnings above"
        );
    }

    Ok(())
}
