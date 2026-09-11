#[path = "../../crates/pairs/tests/support/mod.rs"]
mod support;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use botcore::{Instrument, Symbol, Timeframe};
use pairs::{
    BarOutcome, BotSnapshot, ExecutorConfig, PairContext, PairParams, PairSide, PortfolioGuard,
    RiskGuardConfig, evaluate_bar,
};
use persistence::Journal;
use rust_decimal_macros::dec;
use support::FaultExchange;
use tokio::sync::RwLock;

fn instrument(symbol: &str) -> Instrument {
    Instrument {
        symbol: Symbol::new(symbol),
        tick_size: dec!(0.01),
        qty_step: dec!(0.01),
        min_order_qty: dec!(0.01),
        min_notional: dec!(5),
        launch_time_ms: 0,
    }
}

fn params(leg_a: &str, leg_b: &str) -> PairParams {
    PairParams {
        leg_a: Symbol::new(leg_a),
        leg_b: Symbol::new(leg_b),
        timeframe: Timeframe::H1,
        rolling_window: 180,
        entry_z: 3.0,
        stop_z: 4.0,
        target_z: 0.0,
        max_hold_bars: 48,
        fee_per_leg: dec!(0.0002),
        per_leg_notional_usdt: dec!(25),
        risk_pct_of_equity: dec!(0.03),
        max_notional_multiple_of_equity: dec!(1),
        enable_breakeven: false,
        breakeven_r_multiple: dec!(2),
    }
}

/// A risk guard whose thresholds are unreachable — the loop-behavior tests
/// below are about entry/exit/reconciliation, not the circuit breaker.
fn risk_off() -> RiskGuardConfig {
    RiskGuardConfig {
        daily_loss_halt_pct: dec!(100),
        total_loss_halt_pct: dec!(100),
    }
}

async fn ctx_for(
    ex: Arc<FaultExchange>,
    bot_id: &str,
    display_name: &str,
    priority: u32,
    params: PairParams,
    shadow: bool,
) -> (PairContext, Arc<Journal>, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let journal = Arc::new(
        Journal::open_local(dir.path().join("pairs.db").to_str().unwrap())
            .await
            .expect("journal"),
    );
    let guard = Arc::new(RwLock::new(PortfolioGuard::default()));
    let mut instruments = HashMap::new();
    instruments.insert(params.leg_a.to_string(), instrument(params.leg_a.as_str()));
    instruments.insert(params.leg_b.to_string(), instrument(params.leg_b.as_str()));
    (
        PairContext {
            client: ex,
            journal: Arc::clone(&journal),
            guard,
            bot_id: bot_id.into(),
            display_name: display_name.into(),
            priority,
            params,
            exec: ExecutorConfig {
                ticks_through: dec!(5),
                fill_timeout: std::time::Duration::ZERO,
                poll_interval: std::time::Duration::ZERO,
                unwind_ladder: vec![dec!(10), dec!(25), dec!(60)],
                unwind_on_partial_fill: true,
            },
            loop_period: std::time::Duration::from_secs(60),
            kline_margin_bars: 25,
            shadow,
            instruments,
            risk: risk_off(),
        },
        journal,
        dir,
    )
}

#[tokio::test]
async fn a_z_beyond_the_entry_band_opens_a_position_and_journals_it() {
    let ex = Arc::new(FaultExchange::new());
    let last = 1_700_000_000_000;
    ex.candles("AAVEUSDT", last, 181, |i| if i == 180 { dec!(160) } else { dec!(100) });
    ex.candles("ETHUSDT", last, 181, |_| dec!(100));
    let (ctx, journal, _dir) = ctx_for(ex.clone(), "aave_eth", "AAVE/ETH", 100, params("AAVEUSDT", "ETHUSDT"), false).await;

    let out = evaluate_bar(&ctx).await.expect("bar evaluates");
    assert!(matches!(out, BarOutcome::Opened { side: PairSide::ShortSpread }));
    assert!(journal.pair_position("aave_eth").await.expect("journal").is_some());
    assert_eq!(ex.placed.lock().unwrap().len(), 2);
}

#[tokio::test]
async fn a_z_inside_the_band_opens_nothing_and_still_writes_a_heartbeat() {
    let ex = Arc::new(FaultExchange::new());
    let last = 1_700_000_000_000;
    ex.candles("AAVEUSDT", last, 181, |_| dec!(100));
    ex.candles("ETHUSDT", last, 181, |_| dec!(100));
    let (ctx, journal, _dir) = ctx_for(ex, "aave_eth", "AAVE/ETH", 100, params("AAVEUSDT", "ETHUSDT"), false).await;

    let out = evaluate_bar(&ctx).await.expect("bar evaluates");
    assert!(matches!(out, BarOutcome::NoSignal));
    assert!(journal.pair_heartbeat("aave_eth").await.expect("heartbeat").is_some());
}

#[tokio::test]
async fn a_repeated_bar_is_skipped_without_re_evaluating_the_signal() {
    let ex = Arc::new(FaultExchange::new());
    let hour_ms = Timeframe::H1.duration_ms();
    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("unix epoch")
        .as_millis() as i64;
    let last = now_ms - (now_ms % hour_ms) - hour_ms;
    ex.candles("AAVEUSDT", last, 181, |_| dec!(100));
    ex.candles("ETHUSDT", last, 181, |_| dec!(100));
    let (ctx, _journal, _dir) = ctx_for(ex.clone(), "aave_eth", "AAVE/ETH", 100, params("AAVEUSDT", "ETHUSDT"), false).await;

    let _ = evaluate_bar(&ctx).await.expect("first");
    assert_eq!(ex.kline_call_count(), 2, "first evaluation should fetch both legs once");
    let out = evaluate_bar(&ctx).await.expect("second");
    assert!(matches!(out, BarOutcome::Unchanged));
    assert_eq!(ex.kline_call_count(), 2, "second evaluation should reuse the prior closed bar without refetching klines");
}

#[tokio::test]
async fn a_guard_deferral_journals_entry_skipped_and_places_no_order() {
    let ex = Arc::new(FaultExchange::new());
    let last = 1_700_000_000_000;
    ex.candles("LINKUSDT", last, 181, |i| if i == 180 { dec!(160) } else { dec!(100) });
    ex.candles("XRPUSDT", last, 181, |_| dec!(100));
    let (ctx, journal, _dir) = ctx_for(ex.clone(), "link_xrp", "LINK/XRP", 90, params("LINKUSDT", "XRPUSDT"), false).await;
    {
        let mut g = ctx.guard.write().await;
        g.update(BotSnapshot {
            bot_id: "doge_xrp".into(),
            display_name: "DOGE/XRP".into(),
            priority: 100,
            symbols: [Symbol::new("DOGEUSDT"), Symbol::new("XRPUSDT")].into_iter().collect(),
            has_position: true,
            latest_bar_ms: Some(last),
            signal: Some(PairSide::ShortSpread),
        });
    }

    let out = evaluate_bar(&ctx).await.expect("bar evaluates");
    assert!(matches!(out, BarOutcome::Deferred { .. }));
    assert_eq!(ex.placed.lock().unwrap().len(), 0);
    assert_eq!(journal.pair_position("link_xrp").await.unwrap(), None);
    assert!(journal.pair_heartbeat("link_xrp").await.unwrap().unwrap().last_guard_reason.is_some());
}

#[tokio::test]
async fn shadow_mode_journals_the_entry_it_would_have_made_and_places_nothing() {
    let ex = Arc::new(FaultExchange::new());
    let last = 1_700_000_000_000;
    ex.candles("AAVEUSDT", last, 181, |i| if i == 180 { dec!(160) } else { dec!(100) });
    ex.candles("ETHUSDT", last, 181, |_| dec!(100));
    let (ctx, journal, _dir) = ctx_for(ex.clone(), "aave_eth", "AAVE/ETH", 100, params("AAVEUSDT", "ETHUSDT"), true).await;

    let out = evaluate_bar(&ctx).await.expect("bar evaluates");
    assert!(matches!(out, BarOutcome::ShadowEntry { side: PairSide::ShortSpread }));
    assert_eq!(ex.placed.lock().unwrap().len(), 0);
    assert_eq!(journal.pair_position("aave_eth").await.unwrap(), None);
}

#[tokio::test]
async fn a_reconciliation_halt_prevents_any_order_being_placed() {
    let ex = Arc::new(FaultExchange::new());
    let last = 1_700_000_000_000;
    ex.candles("AAVEUSDT", last, 181, |i| if i == 180 { dec!(160) } else { dec!(100) });
    ex.candles("ETHUSDT", last, 181, |_| dec!(100));
    ex.position("AAVEUSDT", dec!(1.5));
    let (ctx, _journal, _dir) = ctx_for(ex.clone(), "aave_eth", "AAVE/ETH", 100, params("AAVEUSDT", "ETHUSDT"), false).await;

    let out = evaluate_bar(&ctx).await.expect("bar evaluates");
    assert!(matches!(out, BarOutcome::Halted { .. }));
    assert_eq!(ex.placed.lock().unwrap().len(), 0);
}

fn today_start_ms() -> i64 {
    const DAY_MS: i64 = 86_400_000;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).expect("unix epoch").as_millis() as i64;
    now.div_euclid(DAY_MS) * DAY_MS
}

#[tokio::test]
async fn a_daily_equity_drawdown_defers_a_new_entry_and_places_no_order() {
    let ex = Arc::new(FaultExchange::new());
    let last = 1_700_000_000_000;
    ex.candles("AAVEUSDT", last, 181, |i| if i == 180 { dec!(160) } else { dec!(100) }); // would signal
    ex.candles("ETHUSDT", last, 181, |_| dec!(100));
    let (mut ctx, journal, _dir) = ctx_for(ex.clone(), "aave_eth", "AAVE/ETH", 100, params("AAVEUSDT", "ETHUSDT"), false).await;
    // Today's account opened at 10,000; it is now down 6%, past a 5% daily
    // threshold but nowhere near a 15% total one.
    journal.record_equity(dec!(10000), today_start_ms() + 1_000).await.unwrap();
    ex.equity(dec!(9400));
    ctx.risk = RiskGuardConfig { daily_loss_halt_pct: dec!(5), total_loss_halt_pct: dec!(15) };

    let out = evaluate_bar(&ctx).await.expect("bar evaluates");
    assert!(matches!(out, BarOutcome::Deferred { .. }), "got {out:?}");
    assert_eq!(ex.placed.lock().unwrap().len(), 0, "no entry order may be placed under a daily halt");
    assert_eq!(journal.halt_reason().await.unwrap(), None, "a daily-only breach must not write halt_state");
}

#[tokio::test]
async fn a_total_equity_drawdown_halts_and_a_human_clearing_it_resumes_trading() {
    let ex = Arc::new(FaultExchange::new());
    let last = 1_700_000_000_000;
    ex.candles("AAVEUSDT", last, 181, |i| if i == 180 { dec!(160) } else { dec!(100) }); // would signal
    ex.candles("ETHUSDT", last, 181, |_| dec!(100));
    let (mut ctx, journal, _dir) = ctx_for(ex.clone(), "aave_eth", "AAVE/ETH", 100, params("AAVEUSDT", "ETHUSDT"), false).await;
    journal.record_equity(dec!(10000), today_start_ms() + 1_000).await.unwrap(); // all-time peak
    ex.equity(dec!(8000)); // -20%, past a 15% total threshold
    ctx.risk = RiskGuardConfig { daily_loss_halt_pct: dec!(100), total_loss_halt_pct: dec!(15) };

    let out = evaluate_bar(&ctx).await.expect("first bar evaluates");
    assert!(matches!(out, BarOutcome::Halted { .. }), "got {out:?}");
    assert_eq!(ex.placed.lock().unwrap().len(), 0);
    assert!(journal.halt_reason().await.unwrap().is_some(), "a total breach must persist to halt_state");

    // Equity recovers, but the halt must stay in force without a human's
    // clear_halt — that is the entire point of the total-drawdown tier.
    // Each call needs a fresh bar (a new `last`), or the unchanged-bar
    // short-circuit above the halt check would return Unchanged instead.
    ex.equity(dec!(10000));
    ex.candles("AAVEUSDT", last + 3_600_000, 181, |i| if i == 180 { dec!(160) } else { dec!(100) });
    ex.candles("ETHUSDT", last + 3_600_000, 181, |_| dec!(100));
    let out = evaluate_bar(&ctx).await.expect("second bar evaluates");
    assert!(matches!(out, BarOutcome::Halted { .. }), "a total halt must not self-clear on recovered equity, got {out:?}");

    journal.clear_halt().await.unwrap();
    ex.candles("AAVEUSDT", last + 7_200_000, 181, |i| if i == 180 { dec!(160) } else { dec!(100) });
    ex.candles("ETHUSDT", last + 7_200_000, 181, |_| dec!(100));
    let out = evaluate_bar(&ctx).await.expect("third bar evaluates");
    assert!(!matches!(out, BarOutcome::Halted { .. }), "clearing halt_state must let trading resume, got {out:?}");
}
