use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use bot::config::Profile;
use bot::envfile::load_env_file;
use bot::pairs_config::load_pairs_config;
use botcore::Instrument;
use exchange::ExchangeClient;
use exchange::bybit::rest::BybitRest;
use exchange::bybit::sign::Credentials;
use pairs::{BarOutcome, PairContext, PortfolioGuard, evaluate_bar};
use persistence::{Journal, spawn_sync_task};
use tokio::sync::RwLock;
use tracing::{error, info, warn};

async fn open_runtime_journal(path: &str) -> Result<Journal, Box<dyn std::error::Error>> {
    match (
        std::env::var("TURSO_DATABASE_URL"),
        std::env::var("TURSO_AUTH_TOKEN"),
    ) {
        (Ok(url), Ok(token)) if !url.is_empty() && !token.is_empty() => {
            match Journal::open_synced(path, &url, &token).await {
                Ok(j) => {
                    info!("journal opened with Turso cloud sync");
                    Ok(j)
                }
                Err(e) => {
                    error!(kind = e.kind(), "Turso sync unavailable; falling back to local journal");
                    Ok(Journal::open_local(path).await?)
                }
            }
        }
        _ => Ok(Journal::open_local(path).await?),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("no rustls crypto provider may be installed before this point");

    let profile = Profile::from_name(&std::env::args().nth(1).unwrap_or_else(|| "testnet".into()))?;
    let shadow = std::env::args().any(|a| a == "--shadow");
    load_env_file("/home/acekavi/Projects/Crypto/.env")?;
    let cfg = load_pairs_config(profile)?;

    let creds = Credentials::from_env()?;
    let rest = Arc::new(BybitRest::new(profile.rest_base_url().to_string(), creds));
    let _balance = rest.balance().await?;
    let instruments = rest.instruments().await?;

    std::fs::create_dir_all("data")?;
    let instrument_map: HashMap<String, Instrument> = instruments
        .into_iter()
        .map(|i| (i.symbol.as_str().to_string(), i))
        .collect();
    let guard = Arc::new(RwLock::new(PortfolioGuard::default()));
    let client: Arc<dyn ExchangeClient> = rest;

    let loop_period = Duration::from_secs(cfg.runtime.loop_seconds);
    let mut contexts = Vec::new();
    for bot in cfg.bots {
        let journal = Arc::new(open_runtime_journal(&cfg.runtime.journal_path).await?);
        spawn_sync_task(Arc::clone(&journal), Duration::from_secs(30));
        contexts.push(PairContext {
            client: Arc::clone(&client),
            journal,
            guard: Arc::clone(&guard),
            bot_id: bot.id.clone(),
            display_name: bot.name.clone(),
            priority: bot.priority,
            params: bot.params.clone(),
            exec: cfg.executor.clone(),
            loop_period,
            kline_margin_bars: cfg.runtime.kline_margin_bars,
            shadow,
            instruments: instrument_map.clone(),
        });
    }

    loop {
        for ctx in &contexts {
            match evaluate_bar(ctx).await {
                Ok(BarOutcome::Halted { reason }) => {
                    error!(bot_id = %ctx.bot_id, reason = %reason, "pair halted; continuing the rest of the portfolio sweep");
                }
                Ok(outcome) => {
                    info!(bot_id = %ctx.bot_id, ?outcome, "pair loop tick");
                }
                Err(e) => {
                    warn!(bot_id = %ctx.bot_id, error = %e, "pair loop iteration failed; retrying next sweep");
                }
            }
        }
        tokio::time::sleep(loop_period).await;
    }
}
