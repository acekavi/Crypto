use std::sync::Arc;
use std::time::Duration;

use bot::config::{Config, Profile};
use botcore::{Symbol, Timeframe};
use exchange::bybit::rest::BybitRest;
use exchange::bybit::sign::Credentials;
use exchange::bybit::ws_public::BybitPublicFeed;
use exchange::{ExchangeClient, MarketEvent, MarketFeed, Subscription};
use persistence::{spawn_sync_task, Journal};
use tracing::{error, info};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .init();

    let profile_name = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "testnet".to_string());
    let profile = Profile::from_name(&profile_name)?;
    let config = Config::load(profile)?;

    info!(profile = profile.name(), config_hash = %config.hash(), "starting probe");

    let creds = Credentials::from_env()?;
    let rest = Arc::new(BybitRest::new(profile.rest_base_url().to_string(), creds));

    // 1. Prove authentication works.
    let balance = rest.balance().await?;
    info!(equity = %balance.equity, available = %balance.available, "authenticated");

    // 2. Prove market data works and instrument metadata parses.
    let instruments = rest.instruments().await?;
    info!(count = instruments.len(), "loaded tradable instruments");

    let tickers = rest.tickers().await?;
    let mut ranked: Vec<_> = tickers
        .into_iter()
        .filter(|t| {
            t.turnover_24h >= rust_decimal::Decimal::from(config.universe.min_turnover_24h)
        })
        .collect();
    ranked.sort_by_key(|t| std::cmp::Reverse(t.turnover_24h));
    ranked.truncate(config.universe.size);
    info!(count = ranked.len(), top = ?ranked.first().map(|t| t.symbol.as_str()), "universe ranked");

    // 3. Prove the journal works, falling back to local-only when Turso is
    //    not configured or unreachable — never a reason to refuse to start.
    let journal = match (std::env::var("TURSO_DATABASE_URL"), std::env::var("TURSO_AUTH_TOKEN")) {
        (Ok(url), Ok(token)) if !url.is_empty() && !token.is_empty() => {
            match Journal::open_synced("data/bot.db", &url, &token).await {
                Ok(j) => {
                    info!("journal opened with Turso cloud sync");
                    j
                }
                Err(e) => {
                    error!(error = %e, "Turso sync unavailable; falling back to local journal");
                    Journal::open_local("data/bot.db").await?
                }
            }
        }
        _ => {
            info!("Turso not configured; using local journal only");
            Journal::open_local("data/bot.db").await?
        }
    };
    let journal = Arc::new(journal);
    journal.record_equity(balance.equity, 0).await?;
    spawn_sync_task(Arc::clone(&journal), Duration::from_secs(30));

    // 4. Prove the streaming feed works end to end.
    let feed = BybitPublicFeed::new(profile.ws_public_url().to_string(), Arc::clone(&rest));
    let subs: Vec<Subscription> = ranked
        .iter()
        .take(3)
        .map(|t| Subscription { symbol: Symbol::new(t.symbol.as_str()), timeframe: Timeframe::H1 })
        .collect();
    let mut rx = feed.subscribe(&subs).await?;
    info!(symbols = ?subs.iter().map(|s| s.symbol.as_str()).collect::<Vec<_>>(), "streaming klines");

    // 5. Warm up from history so a candle close is not needed to see data.
    for sub in &subs {
        let candles = rest.klines(&sub.symbol, sub.timeframe, 250).await?;
        info!(symbol = %sub.symbol, candles = candles.len(), "warmup history loaded");
    }

    info!("probe running; Ctrl-C to exit");
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("shutdown signal received");
                if let Err(e) = journal.push().await {
                    error!(error = %e, "final journal push failed");
                }
                return Ok(());
            }
            event = rx.recv() => match event {
                Ok(MarketEvent::CandleClosed { symbol, tf, candle }) => {
                    info!(%symbol, ?tf, close = %candle.close, "candle closed");
                }
                Ok(MarketEvent::GapFilled { symbol, candles, .. }) => {
                    info!(%symbol, count = candles.len(), "gap backfilled");
                }
                Err(e) => error!(error = %e, "feed channel error"),
            },
        }
    }
}
