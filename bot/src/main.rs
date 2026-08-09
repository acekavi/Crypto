use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use bot::config::{Config, Profile};
use bot::engine_loop::EngineLoop;
use botcore::{ErrorClass, Symbol, Timeframe};
use engine::{EscalationLadder, UniverseFilter, reconcile, select_universe};
use exchange::bybit::rest::BybitRest;
use exchange::bybit::sign::Credentials;
use exchange::bybit::ws_private::BybitPrivateFeed;
use exchange::bybit::ws_public::BybitPublicFeed;
use exchange::{ExchangeClient, MarketEvent, Subscription};
use persistence::{Journal, spawn_sync_task};
use risk::{RiskManager, RiskParams};
use rust_decimal::Decimal;
use rust_decimal::prelude::FromPrimitive;
use strategy::{IctStrategy, Strategy, ict_params_from_config};
use tokio::sync::broadcast::error::RecvError;
use tracing::{error, info, warn};

/// Convert a config `f64` knob into a `Decimal`, refusing rather than
/// silently producing a garbage value if the file ever carries a NaN,
/// infinity, or a value with no `Decimal` representation. These knobs govern
/// real risk limits, so a bad conversion must fail loudly at startup rather
/// than reach `RiskManager` as a wrong number.
fn config_decimal(value: f64, field: &'static str) -> Result<Decimal, Box<dyn std::error::Error>> {
    if !value.is_finite() {
        return Err(format!("config field {field} ({value}) is not a finite number").into());
    }
    Decimal::from_f64(value)
        .ok_or_else(|| format!("config field {field} ({value}) cannot convert to Decimal").into())
}

/// Cross-product every symbol with every declared timeframe.
///
/// Shared between the startup subscription and each daily re-rank so the two
/// can never drift into subscribing a different shape of topic set.
fn build_subscriptions(symbols: &[Symbol], timeframes: &[Timeframe]) -> Vec<Subscription> {
    symbols
        .iter()
        .flat_map(|s| {
            timeframes.iter().map(move |&tf| Subscription {
                symbol: s.clone(),
                timeframe: tf,
            })
        })
        .collect()
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    // rustls 0.23 refuses to pick a crypto provider when more than one is
    // compiled in — reqwest and tokio-tungstenite pull different ones — and it
    // panics deep inside the WebSocket handshake rather than failing at
    // startup. Choose explicitly here, before anything opens a connection.
    rustls::crypto::aws_lc_rs::default_provider()
        .install_default()
        .expect("no rustls crypto provider may be installed before this point");

    let profile_name = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "testnet".to_string());
    let profile = Profile::from_name(&profile_name)?;
    let config = Config::load(profile)?;

    info!(profile = profile.name(), config_hash = %config.hash(), "starting bot");

    let creds = Credentials::from_env()?;
    let rest = Arc::new(BybitRest::new(profile.rest_base_url().to_string(), creds));

    // 1. Authenticate.
    let balance = rest.balance().await?;
    info!(equity = %balance.equity, available = %balance.available, "authenticated");

    // 2. Load instrument metadata; every order and every universe filter
    //    needs tick size, quantity step and listing age.
    let instruments = rest.instruments().await?;
    info!(count = instruments.len(), "loaded tradable instruments");

    // turso::Builder::new_local expects the parent directory to already
    // exist; create it so a fresh checkout can run without a manual
    // `mkdir -p data` first.
    std::fs::create_dir_all("data")
        .map_err(|e| format!("failed to create data directory \"data\": {e}"))?;

    // 3. Open the journal, falling back to local-only when Turso is not
    //    configured or unreachable — never a reason to refuse to start.
    let journal = match (
        std::env::var("TURSO_DATABASE_URL"),
        std::env::var("TURSO_AUTH_TOKEN"),
    ) {
        (Ok(url), Ok(token)) if !url.is_empty() && !token.is_empty() => {
            match Journal::open_synced("data/bot.db", &url, &token).await {
                Ok(j) => {
                    info!("journal opened with Turso cloud sync");
                    j
                }
                Err(e) => {
                    // Deliberately not logging the error's Display: it comes
                    // from the underlying HTTP client and commonly embeds the
                    // connection target, which would leak TURSO_DATABASE_URL
                    // (a credential-adjacent value) into logs on exactly the
                    // misconfiguration path most likely to trigger it.
                    error!(
                        kind = e.kind(),
                        "Turso sync unavailable; falling back to local journal"
                    );
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
    journal
        .record_equity(balance.equity, rest.clock().now_ms())
        .await?;
    spawn_sync_task(Arc::clone(&journal), Duration::from_secs(30));

    // 4. Build the strategy, the risk envelope and the engine that wires
    //    them together. The strategy's own declared timeframes and warmup
    //    requirement drive every subsequent warm-up and subscription — they
    //    are captured here, before the strategy is boxed into the engine,
    //    because nothing downstream can reach inside the engine to ask it.
    let strategy_params = ict_params_from_config(
        config.strategy.bias_ema,
        config.strategy.swing_lookback,
        config.strategy.atr_period,
        config.strategy.ob_lookback,
        config.strategy.fvg_entry_fraction,
        config.strategy.stop_buffer_atr,
        config.strategy.stop_widen_multiple,
        config.strategy.reward_multiple,
        config.strategy.breakeven_at_r,
        config.strategy.use_pdh_pdl,
        config.strategy.use_session_levels,
        config.strategy.use_order_block,
        config.strategy.require_mss,
        config.strategy.session_filter,
        config.strategy.allow_long,
        config.strategy.allow_short,
    )?;
    info!(
        reward_multiple = %strategy_params.reward_multiple,
        breakeven_at_r = ?strategy_params.breakeven_at_r,
        structure_tf = ?strategy_params.structure_tf,
        execution_tf = ?strategy_params.execution_tf,
        "strategy: ICT liquidity sweep"
    );
    let ict = IctStrategy::new(strategy_params);
    let strategy_timeframes: Vec<Timeframe> = ict.timeframes().to_vec();
    let warmup_candles = ict.warmup_candles();
    let warmup_limit = warmup_candles as u16;

    let risk_params = RiskParams {
        risk_pct: config_decimal(config.risk.risk_pct, "risk.risk_pct")?,
        max_concurrent_positions: config.risk.max_concurrent_positions as usize,
        max_daily_entries: config.risk.max_daily_entries,
        daily_drawdown_halt_pct: config_decimal(
            config.risk.daily_drawdown_halt_pct,
            "risk.daily_drawdown_halt_pct",
        )?,
        total_drawdown_halt_pct: config_decimal(
            config.risk.total_drawdown_halt_pct,
            "risk.total_drawdown_halt_pct",
        )?,
        liq_buffer_multiple: config_decimal(
            config.risk.liq_buffer_multiple,
            "risk.liq_buffer_multiple",
        )?,
    };
    let stop_limit_offset_atr = config_decimal(
        config.strategy.stop_limit_offset_atr,
        "strategy.stop_limit_offset_atr",
    )?;

    // Entries are timed on the strategy's fastest declared timeframe — the one
    // it emits signals on, M15 for ICT — so the expiry window the reconciler
    // uses to judge a resting order's age is expressed in those candles.
    // Derived from the declaration rather than naming a timeframe here: 12
    // candles is three hours on M15 and twelve on H1, so a hard-coded
    // timeframe leaves stale entries resting four times too long after a
    // restart the moment the strategy changes.
    let entry_timeframe = strategy_timeframes
        .iter()
        .copied()
        .min_by_key(|tf| tf.duration_ms())
        .expect("a strategy declares at least one timeframe");
    let expiry_window_ms =
        entry_timeframe.duration_ms() * i64::from(config.strategy.entry_expiry_candles);

    let mut engine_loop = EngineLoop::new(
        Box::new(ict),
        RiskManager::new(risk_params, stop_limit_offset_atr),
        Arc::clone(&rest) as Arc<dyn ExchangeClient>,
        Arc::clone(&journal),
        instruments.clone(),
        config.hash(),
        config.strategy.entry_expiry_candles,
        warmup_candles,
    );

    // Seed the drawdown baselines from the journal before anything else can
    // touch them, so a restart mid-drawdown resumes measuring from the true
    // all-time peak and the true 00:00 UTC mark rather than snapping both
    // to whatever equity exists at this moment.
    engine_loop.load_baselines(rest.clock().now_ms()).await?;

    // 5. Reconcile BEFORE any strategy evaluation. A restart, crash or
    //    manual intervention can leave this process believing something
    //    untrue; the exchange settles every disagreement. Resting orders the
    //    exchange still shows land straight in the engine's own tracker —
    //    there is no separate tracker to reconcile into and then copy over.
    let report = reconcile(
        rest.as_ref(),
        engine_loop.tracker_mut(),
        rest.clock().now_ms(),
        expiry_window_ms,
    )
    .await?;
    info!(
        adopted_positions = report.adopted_positions.len(),
        adopted_orders = report.adopted_orders.len(),
        cancelled_stale = report.cancelled_stale.len(),
        "reconciled against the exchange"
    );
    for symbol in &report.unprotected {
        warn!(%symbol, "adopted position — verify it carries a stop and target");
    }

    // Rebuild the stop protections for the positions reconciliation just
    // adopted. This must run AFTER reconcile and BEFORE the loop starts: the
    // exchange decides which positions exist, and the journal supplies only
    // what the exchange does not report — a trigger, the 1R the position was
    // sized against, and whether its stop has already moved to entry. Without
    // it a restart mid-trade orphans the position: no breakeven management and
    // no escalation ladder, for a target that can take days to reach.
    //
    // `adopted_positions` is exactly the list `reconcile` read from the
    // exchange a moment ago, so no second `positions()` call can disagree with
    // it. A journal row with no matching position is stale and is dropped
    // there, never adopted.
    let restored = engine_loop
        .restore_protections(&report.adopted_positions, rest.clock().now_ms())
        .await?;
    info!(
        restored,
        open_positions = report.adopted_positions.len(),
        "restored stop protections from the journal"
    );

    // 6. Establish the tradable universe. A pinned list is used verbatim; only
    //    when none is configured is the turnover/age screen consulted, which
    //    always keeps symbols reconciliation just adopted so their candles keep
    //    arriving and the engine can manage them to a close.
    let universe_filter = UniverseFilter {
        size: config.universe.size,
        min_turnover_24h: Decimal::from(config.universe.min_turnover_24h),
        min_listing_age_days: config.universe.min_listing_age_days,
    };
    let symbols: Vec<Symbol> = match &config.universe.symbols {
        // Pinned: the strategy's measured results describe exactly these
        // symbols, so screening the top N by turnover instead would spend the
        // concurrent-position budget on symbols nobody measured.
        Some(pinned) => {
            let known: HashSet<&str> = instruments.iter().map(|i| i.symbol.as_str()).collect();
            // Refuse at startup rather than warn per candle: without instrument
            // metadata no order can be formed for a symbol, so a typo here
            // would look like a healthy bot that silently never trades it.
            let unknown: Vec<&String> = pinned
                .iter()
                .filter(|s| !known.contains(s.as_str()))
                .collect();
            if !unknown.is_empty() {
                error!(?unknown, "pinned symbols are not tradable instruments");
                return Err(format!(
                    "universe.symbols contains symbols the exchange does not list: {unknown:?}"
                )
                .into());
            }
            info!(count = pinned.len(), symbols = ?pinned, "universe pinned by config");
            pinned.iter().map(Symbol::new).collect()
        }
        None => {
            let protected = engine_loop.protected_symbols().await?;
            let tickers = rest.tickers().await?;
            let ranked = select_universe(
                &tickers,
                &instruments,
                &universe_filter,
                rest.clock().now_ms(),
                &protected,
            );
            info!(
                count = ranked.len(),
                top = ?ranked.first().map(Symbol::as_str),
                "universe ranked"
            );
            ranked
        }
    };
    if symbols.is_empty() {
        error!(
            min_turnover_24h = config.universe.min_turnover_24h,
            "no symbol met the turnover floor; there is nothing to subscribe to"
        );
        return Err("universe filter produced an empty symbol list".into());
    }

    // 7. Warm EVERY timeframe the strategy declares, for every symbol in the
    //    universe. `CandleStore::is_stale` treats a stream that has never
    //    produced a candle as stale forever, and staleness is checked across
    //    every declared timeframe on each candle close — so a symbol whose
    //    H4 history was never fetched would refuse every signal permanently,
    //    not just until the next H4 close.
    for symbol in &symbols {
        for &tf in &strategy_timeframes {
            let candles = rest.klines(symbol, tf, warmup_limit).await?;
            info!(%symbol, ?tf, candles = candles.len(), "warmup history loaded");
            engine_loop.warm(symbol, tf, candles);
        }
    }
    let mut current_universe: HashSet<Symbol> = symbols.iter().cloned().collect();

    // 8. Stream market data for every symbol across every declared
    //    timeframe — not just H1. The staleness gate above only stays
    //    satisfied if H4 keeps receiving live candles too.
    let feed = BybitPublicFeed::new(profile.ws_public_url().to_string(), Arc::clone(&rest));
    let subs = build_subscriptions(&symbols, &strategy_timeframes);
    let (mut rx, mut feed_handle) = feed.subscribe_with_handle(&subs).await?;
    info!(
        symbols = symbols.len(),
        timeframes = strategy_timeframes.len(),
        "streaming klines"
    );

    // 9. Stream account data. `ClockOffset` cannot be cloned, so the private
    //    feed shares the exact instance `rest` uses for signing via
    //    `clock_handle()` (an `Arc` accessor added to `BybitRest` for this)
    //    rather than starting from an independent, uncorrected clock.
    let private = BybitPrivateFeed::new(
        profile.ws_private_url().to_string(),
        Credentials::from_env()?,
        rest.clock_handle(),
    );
    let mut account_rx = private.subscribe();

    let mut rerank = tokio::time::interval(Duration::from_secs(86_400));
    rerank.tick().await; // the first tick fires immediately; skip it

    // Ticked at a THIRD of the ladder's own per-rung timeout, not the full
    // timeout: driving the ladder only as often as a rung can time out would
    // let a rung's true resting time reach almost 2x what the ladder intends
    // (triggered just after a tick, then waiting a full timeout for the
    // next one). `EscalationLadder::defaults()` is the same source
    // `EngineLoop` itself builds its ladder from, so the two can never drift
    // out of step with each other.
    let escalation_ladder = EscalationLadder::defaults();
    let mut escalation_interval = tokio::time::interval(Duration::from_millis(
        (escalation_ladder.timeout_ms / 3).max(1) as u64,
    ));
    escalation_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    info!("bot running; Ctrl-C to exit");
    loop {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                info!("shutdown signal received");
                if let Err(e) = journal.push().await {
                    // See the Turso-connect error above: the Display of a
                    // sync failure can carry TURSO_DATABASE_URL, so only the
                    // error variant is logged, never its message.
                    error!(kind = e.kind(), "final journal push failed");
                }
                return Ok(());
            }
            event = rx.recv() => match event {
                Ok(MarketEvent::CandleClosed { symbol, tf, candle }) => {
                    // The seam that drives BOTH per-candle passes — the
                    // breakeven check against this candle's high/low, then the
                    // strategy — in the order the backtester applies them; see
                    // `drive_candle_close`.
                    match engine_loop.drive_candle_close(&symbol, tf, &candle).await {
                        Ok(outcome) => info!(%symbol, ?tf, ?outcome, "candle processed"),
                        Err(e) => {
                            if e.class() == ErrorClass::Fatal {
                                error!(%symbol, error = %e, "fatal error processing a candle; halting");
                                return Err(e.into());
                            }
                            warn!(%symbol, error = %e, "processing a candle failed");
                        }
                    }
                }
                Ok(MarketEvent::GapFilled { symbol, tf, candles }) => {
                    info!(%symbol, count = candles.len(), "rewarming after a gap backfill");
                    engine_loop.warm(&symbol, tf, candles);
                }
                // A closed channel means the feed task itself has ended. That
                // is the signal a Fatal feed error uses to reach us, and it is
                // never transient — continuing here would spin the loop at
                // full CPU logging the same line millions of times a second.
                Err(RecvError::Closed) => {
                    error!("market feed channel closed; the feed task has died");
                    return Err("market feed channel closed".into());
                }
                // Lagged means we fell behind a live feed, not that it died.
                Err(RecvError::Lagged(skipped)) => {
                    warn!(skipped, "fell behind the market feed");
                }
            },
            account = account_rx.recv() => match account {
                Ok(event) => engine_loop.on_account_event(&event).await,
                // The private feed breaks its loop on a Fatal error (e.g. a
                // rejected auth) for exactly the same reason the public feed
                // does: dropping the sender is an unambiguous signal on the
                // existing subscribe() receiver, with no separate health
                // channel required. Mirrored here identically to the market
                // feed's Closed arm above — both mean a feed died, and that is
                // a halt condition.
                Err(RecvError::Closed) => {
                    error!("account feed channel closed; the private feed has died");
                    return Err("account feed channel closed".into());
                }
                Err(RecvError::Lagged(skipped)) => {
                    warn!(skipped, "fell behind the account feed");
                }
            },
            _ = escalation_interval.tick() => {
                // The ladder alone rides the timer. It reacts to a stop that
                // has already fired and not filled, which is genuinely time
                // sensitive — every second it rests unfilled is a position
                // running unprotected. The breakeven threshold is not: a
                // closed candle records the extreme it reached exactly, so
                // that check moved to the candle arm above, where it matches
                // the backtester.
                //
                // Mirrors the candle arm exactly: Fatal halts the process,
                // anything else is logged and the loop continues — one bad
                // tick must not stop the ladder from being driven on every
                // OTHER open position.
                if let Err(e) = engine_loop.drive_stop_escalation(rest.clock().now_ms()).await {
                    if e.class() == ErrorClass::Fatal {
                        error!(error = %e, "fatal error driving the stop escalation ladder; halting");
                        return Err(e.into());
                    }
                    warn!(error = %e, "driving the stop escalation ladder failed");
                }
            }
            _ = rerank.tick() => {
                let protected = engine_loop.protected_symbols().await?;
                let tickers = rest.tickers().await?;
                let fresh_instruments = rest.instruments().await?;
                let ranked = select_universe(
                    &tickers,
                    &fresh_instruments,
                    &universe_filter,
                    rest.clock().now_ms(),
                    &protected,
                );
                let ranked_set: HashSet<Symbol> = ranked.iter().cloned().collect();

                // Warm every declared timeframe for any symbol that was not
                // already being tracked, for the same reason startup does:
                // an unwarmed timeframe reports Stale forever, not just until
                // its next close.
                for symbol in ranked.iter().filter(|s| !current_universe.contains(*s)) {
                    for &tf in &strategy_timeframes {
                        let candles = rest.klines(symbol, tf, warmup_limit).await?;
                        engine_loop.warm(symbol, tf, candles);
                    }
                }

                let dropped = engine_loop.retain_symbols(&ranked_set);
                info!(universe = ranked.len(), dropped, "daily universe re-rank");

                // Only tear down and rebuild the socket when membership
                // actually changed — every symbol above has already been
                // warmed over REST before this point, so a newly-entered
                // symbol's first candle on the new subscription never lands
                // in a cold store.
                if ranked_set != current_universe {
                    let joined = ranked_set.difference(&current_universe).count();
                    let left = current_universe.difference(&ranked_set).count();
                    info!(joined, left, "universe membership changed; resubscribing market data");

                    feed_handle.abort();
                    let new_subs = build_subscriptions(&ranked, &strategy_timeframes);
                    let (new_rx, new_handle) = feed.subscribe_with_handle(&new_subs).await?;
                    rx = new_rx;
                    feed_handle = new_handle;
                }

                current_universe = ranked_set;
            }
        }
    }
}
