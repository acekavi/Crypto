use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::Duration;

use bot::config::Profile;
use bot::envfile::load_env_file;
use bot::pairs_config::load_pairs_config;
use bot::pairs_report::{
    LoadedParamsView, RuntimePositionView, RuntimeStateView, atomic_write, loaded_params_view,
    runtime_snapshot_path,
};
use bot::trade_alerts::{AlertEvent, AlertOutcome, send_trade_alert};
use botcore::Instrument;
use exchange::ExchangeClient;
use exchange::bybit::rest::BybitRest;
use exchange::bybit::sign::Credentials;
use pairs::{BarOutcome, PairContext, PortfolioGuard, evaluate_bar};
use persistence::{Journal, PairHeartbeat, PairPositionRecord, spawn_sync_task};
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

fn snapshot_state(
    hb: Option<PairHeartbeat>,
    pos: Option<PairPositionRecord>,
    loaded_params: LoadedParamsView,
) -> RuntimeStateView {
    RuntimeStateView {
        last_bar_ms: hb.as_ref().and_then(|h| h.last_bar_ms),
        last_loop_wall_time: hb.as_ref().and_then(|h| h.last_loop_ms).map(|ms| ms as f64 / 1000.0),
        last_seen_z: hb.as_ref().and_then(|h| h.last_z.as_ref()).and_then(|d| d.to_string().parse::<f64>().ok()),
        last_seen_signal: hb.as_ref().and_then(|h| h.last_signal.clone()),
        last_guard_reason: hb.as_ref().and_then(|h| h.last_guard_reason.clone()),
        position: pos.map(|p| RuntimePositionView {
            side: p.side,
            opened_at_ms: p.opened_at_ms,
            entry_z: p.entry_z.to_string(),
            per_leg_notional_usdt: p.per_leg_notional.to_string(),
            capped_by: p.capped_by,
            breakeven_armed: p.breakeven_armed,
        }),
        loaded_params: Some(loaded_params),
        snapshot_age_s: None,
        snapshot_stale: false,
    }
}

async fn write_runtime_snapshot(contexts: &[PairContext], db_path: &str) -> Result<(), Box<dyn std::error::Error>> {
    let mut states = BTreeMap::new();
    for ctx in contexts {
        let hb = ctx.journal.pair_heartbeat(&ctx.bot_id).await?;
        let pos = ctx.journal.pair_position(&ctx.bot_id).await?;
        states.insert(
            ctx.bot_id.clone(),
            snapshot_state(hb, pos, loaded_params_view(&bot::pairs_config::BotConfig {
                id: ctx.bot_id.clone(),
                name: ctx.display_name.clone(),
                priority: ctx.priority,
                params: ctx.params.clone(),
            })),
        );
    }
    let path = runtime_snapshot_path(db_path);
    atomic_write(&path, serde_json::to_vec_pretty(&states)?.as_slice())?;
    Ok(())
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
    let journal_path = cfg.runtime.journal_path.clone();
    let mut contexts = Vec::new();
    for bot in cfg.bots {
        let journal = Arc::new(open_runtime_journal(&cfg.runtime.journal_path).await?);
        spawn_sync_task(Arc::clone(&journal), Duration::from_secs(30));
        let loaded = loaded_params_view(&bot);
        info!(
            bot_id = %bot.id,
            pair = %bot.params.display_pair(),
            rolling_window = loaded.rolling_window,
            entry_z = %loaded.entry_z,
            stop_z = %loaded.stop_z,
            target_z = %loaded.target_z,
            max_hold_bars = loaded.max_hold_bars,
            risk_pct = %loaded.risk_pct_of_equity,
            "loaded runtime params"
        );
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
                    let alert = match &outcome {
                        BarOutcome::Opened { side } => Some(AlertEvent {
                            bot_id: ctx.bot_id.clone(),
                            display_name: ctx.display_name.clone(),
                            at_ms: ctx.journal.pair_heartbeat(&ctx.bot_id).await.ok().flatten().and_then(|h| h.last_bar_ms).unwrap_or_default(),
                            outcome: AlertOutcome::Opened,
                            side: Some(side.as_str().to_string()),
                            detail: "entry filled; pair position opened".into(),
                        }),
                        BarOutcome::Closed { reason } => Some(AlertEvent {
                            bot_id: ctx.bot_id.clone(),
                            display_name: ctx.display_name.clone(),
                            at_ms: ctx.journal.pair_heartbeat(&ctx.bot_id).await.ok().flatten().and_then(|h| h.last_bar_ms).unwrap_or_default(),
                            outcome: AlertOutcome::Closed,
                            side: None,
                            detail: format!("exit reason={}", reason.as_str()),
                        }),
                        _ => None,
                    };
                    if let Some(event) = alert {
                        send_trade_alert(&event)
                            .await
                            .unwrap_or_else(|e| warn!(bot_id = %ctx.bot_id, error = %e, "failed to send trade alert"));
                    }
                }
                Err(e) => {
                    warn!(bot_id = %ctx.bot_id, error = %e, "pair loop iteration failed; retrying next sweep");
                }
            }
        }
        if let Err(e) = write_runtime_snapshot(&contexts, &journal_path).await {
            warn!(error = %e, "failed to write runtime snapshot");
        }
        tokio::time::sleep(loop_period).await;
    }
}
