//! `backtest`: replays stored history through the live engine and reports the
//! pre-registered PASS/FAIL verdict.
//!
//! Reads only local data — no exchange credentials, no network. The verdict is
//! whatever the numbers say; nothing here tunes toward a PASS, and the
//! thresholds are fixed in `GateThresholds::pre_registered` with a test
//! guarding their values.

use std::collections::HashSet;

use backtest::gate::{GateThresholds, evaluate};
use backtest::random_entry::{percentile, run_benchmark};
use backtest::walk_forward::{WalkForwardConfig, folds, run_walk_forward};
use backtest::{BacktestConfig, CostModel, run_backtest};
use botcore::{Instrument, Symbol, Timeframe};
use history::HistoryDb;
use risk::{RiskManager, RiskParams};
use rust_decimal::Decimal;
use strategy::ict::{IctParams, IctStrategy};
use strategy::pullback::{PullbackStrategy, StrategyParams};
use strategy::reversion::{ReversionParams, ReversionStrategy};

/// Roughly one signal every 25 candles, chosen so the benchmark places a
/// comparable NUMBER of trades to the strategy rather than saturating the
/// position caps. Recorded in the report so the comparison is legible.
fn benchmark_probability() -> Decimal {
    Decimal::new(4, 2) // 0.04
}

/// A small, explicitly declared grid. There is deliberately no search: the
/// spec puts optimisation beyond a declared grid out of scope, because that is
/// how overfitting gets industrialised.
fn parameter_grid() -> Vec<StrategyParams> {
    let base = StrategyParams::defaults();
    vec![
        base.clone(),
        StrategyParams {
            atr_stop_multiple: base.atr_stop_multiple * Decimal::new(15, 1),
            ..base.clone()
        },
        StrategyParams {
            rsi_long_trigger: base.rsi_long_trigger + Decimal::from(5),
            rsi_short_trigger: base.rsi_short_trigger - Decimal::from(5),
            ..base
        },
    ]
}

/// What the report actually needs, so the two studies — which carry different
/// parameter types — can be summarised through one shape.
struct FoldLine {
    index: usize,
    trades: usize,
    expectancy: Decimal,
    drawdown_pct: Decimal,
}

struct RunOutcome {
    oos_metrics: backtest::metrics::Metrics,
    fold_lines: Vec<FoldLine>,
}

fn summarise_folds<P>(r: &backtest::walk_forward::WalkForwardResult<P>) -> RunOutcome {
    RunOutcome {
        oos_metrics: r.oos_metrics.clone(),
        fold_lines: r
            .folds
            .iter()
            .map(|f| FoldLine {
                index: f.fold.index,
                trades: f.oos_metrics.trade_count,
                expectancy: f.oos_metrics.expectancy,
                drawdown_pct: f.oos_metrics.max_drawdown_pct,
            })
            .collect(),
    }
}

struct Args {
    db_path: String,
    starting_equity: Decimal,
    /// Days at the END of history reserved for a single final validation.
    ///
    /// Research runs must never see these. Repeatedly testing ideas against
    /// the same data until one passes is how a backtest is talked into
    /// agreeing with you; holding a block back is the only cheap defence.
    holdout_days: i64,
    /// Run ON the reserved block instead of excluding it. For the ONE
    /// validation run, after the research is finished and frozen.
    holdout_only: bool,
    /// Score one frozen configuration over the whole range in a single pass,
    /// with no walk-forward.
    ///
    /// The walk-forward exists to stop parameters being tuned on the data they
    /// are scored against. When the configuration is frozen there is nothing to
    /// tune, and on a short window a walk-forward would fit one fold and score
    /// a fraction of the available days.
    single_pass: bool,
    /// Which pre-registered study to run. `pullback` is the original
    /// (failed) baseline; `reversion` sweeps the six declared variants.
    study: String,
    /// Restrict a reversion run to one named variant. Stage 2 uses this to
    /// send exactly ONE variant to the holdout.
    variant: Option<String>,
}

fn parse_args() -> Result<Args, Box<dyn std::error::Error>> {
    let mut db_path = "data/history.db".to_string();
    let mut starting_equity = Decimal::from(10_000);
    let mut holdout_days = 0i64;
    let mut holdout_only = false;
    let mut single_pass = false;
    let mut study = "pullback".to_string();
    let mut variant: Option<String> = None;

    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--db" => db_path = args.next().ok_or("--db requires a value")?,
            "--equity" => {
                let v = args.next().ok_or("--equity requires a value")?;
                starting_equity = v
                    .parse::<Decimal>()
                    .map_err(|_| format!("--equity value \"{v}\" is not a number"))?;
            }
            "--holdout-days" => {
                let v = args.next().ok_or("--holdout-days requires a value")?;
                holdout_days = v
                    .parse::<i64>()
                    .map_err(|_| format!("--holdout-days value \"{v}\" is not an integer"))?;
            }
            "--holdout-only" => holdout_only = true,
            "--single-pass" => single_pass = true,
            "--study" => study = args.next().ok_or("--study requires a value")?,
            "--variant" => variant = Some(args.next().ok_or("--variant requires a value")?),
            other => return Err(format!("unrecognised argument: {other}").into()),
        }
    }
    if holdout_only && holdout_days <= 0 {
        return Err("--holdout-only requires --holdout-days".into());
    }
    if !["pullback", "reversion", "ict", "sweep"].contains(&study.as_str()) {
        return Err(
            format!("--study must be pullback, reversion, ict or sweep, got {study:?}").into(),
        );
    }
    Ok(Args {
        db_path,
        starting_equity,
        holdout_days,
        holdout_only,
        single_pass,
        study,
        variant,
    })
}

/// The maker fee, read from config rather than hardcoded. See
/// `config/backtest.toml` for its provenance — it is corroborated, not
/// primary-sourced, and a wrong value scales every result silently.
fn load_maker_fee() -> Result<Decimal, Box<dyn std::error::Error>> {
    let text = std::fs::read_to_string("config/backtest.toml")
        .map_err(|e| format!("reading config/backtest.toml: {e}"))?;
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("maker_fee_rate") {
            let value = rest.trim_start_matches([' ', '=']).trim().trim_matches('"');
            return Ok(value.parse::<Decimal>()?);
        }
    }
    Err("maker_fee_rate not found in config/backtest.toml".into())
}

/// Instrument metadata is not stored with the history, so the backtest uses
/// permissive placeholders. Tick and step sizes only round an order down;
/// with `min_order_qty` near zero nothing is rejected for being too small,
/// which keeps the sample from silently shrinking on low-priced symbols.
fn placeholder_instrument(symbol: &Symbol) -> Instrument {
    Instrument {
        symbol: symbol.clone(),
        tick_size: Decimal::new(1, 4),
        qty_step: Decimal::new(1, 6),
        min_order_qty: Decimal::new(1, 6),
        launch_time_ms: 0,
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = parse_args()?;

    let db = HistoryDb::open_local(&args.db_path).await?;
    let maker_fee_rate = load_maker_fee()?;

    // Discover what history actually exists rather than assuming a universe.
    let stored = db.stored_series().await?;
    if stored.is_empty() {
        println!(
            "No history in {}. Run download-history first.",
            args.db_path
        );
        return Ok(());
    }

    let symbols: Vec<Symbol> = stored
        .iter()
        .map(|(s, _, _, _)| s.clone())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let mut symbols = symbols;
    // Deterministic ordering: the replay driver tie-breaks on symbol name, and
    // the config order should not be able to influence anything either.
    symbols.sort();

    // The replay needs BOTH timeframes present for every symbol, since the
    // strategy declares H1 and H4.
    let full_start = stored.iter().map(|(_, _, e, _)| *e).max().unwrap_or(0);
    let full_end = stored.iter().map(|(_, _, _, l)| *l).min().unwrap_or(0);

    // The reserved block sits at the END of history, so research always runs
    // on the earlier period and validation on genuinely unseen data.
    let split_ms = full_end - args.holdout_days * 86_400_000;
    let (start_ms, end_ms) = if args.holdout_only {
        (split_ms, full_end)
    } else if args.holdout_days > 0 {
        (full_start, split_ms)
    } else {
        (full_start, full_end)
    };

    let cfg = BacktestConfig {
        start_ms,
        end_ms,
        starting_equity: args.starting_equity,
        symbols: symbols.clone(),
        instruments: symbols.iter().map(placeholder_instrument).collect(),
        costs: CostModel { maker_fee_rate },
        warmup_candles: 250,
        entry_expiry_candles: if args.study == "sweep" { 12 } else { 3 },
        breakeven_at_r: None,
    };

    let wf = WalkForwardConfig::defaults();
    let available = folds(start_ms, end_ms, &wf);

    println!("\n================ BACKTEST ================");
    println!("history      : {} symbol(s), {:?}", symbols.len(), symbols);
    println!("range        : {start_ms} .. {end_ms}");
    println!("span         : {} days", (end_ms - start_ms) / 86_400_000);
    println!("maker fee    : {maker_fee_rate}");
    println!(
        "walk-forward : {} day IS / {} day OOS",
        wf.in_sample_ms / 86_400_000,
        wf.out_of_sample_ms / 86_400_000
    );
    println!("folds        : {}", available.len());
    println!(
        "data segment : {}",
        if args.holdout_only {
            "HELD-OUT BLOCK — this is the one validation run"
        } else if args.holdout_days > 0 {
            "RESEARCH ONLY — the held-out block is excluded"
        } else {
            "FULL HISTORY — no block reserved"
        }
    );

    if available.is_empty() {
        println!("\n---------------- VERDICT: FAIL ----------------");
        println!("Reason: INSUFFICIENT DATA — not one complete walk-forward fold.");
        println!(
            "A fold needs {} days (180 in-sample + 60 out-of-sample); {} are stored.",
            (wf.in_sample_ms + wf.out_of_sample_ms) / 86_400_000,
            (end_ms - start_ms) / 86_400_000
        );
        println!("\nThis is the correct result, not a defect. Download more history");
        println!("with `download-history --symbol <SYM> --days <N>` and re-run.");
        println!("No threshold has been adjusted to produce a verdict.");
        return Ok(());
    }

    let risk_params = RiskParams::defaults();
    let stop_offset = Decimal::new(3, 1);
    let reversion = args.study == "reversion";
    let ict = args.study == "ict";
    let sweep = args.study == "sweep";

    // Six declared variants means six chances to find noise, so the study
    // raises its own benchmark bar and estimates it from more runs.
    // Both pre-registered studies select one variant from six, so both raise
    // the benchmark bar by the same Bonferroni argument.
    let thresholds = if reversion || ict || sweep {
        GateThresholds::mean_reversion_study()
    } else {
        GateThresholds::pre_registered()
    };
    let seed_count: u64 = if reversion || ict || sweep { 500 } else { 100 };

    println!("\nrunning walk-forward over {} folds...", available.len());
    // The reversion study sweeps its six declared variants and reports each,
    // so a selection is made on evidence rather than on one number. Stage 2
    // passes --variant to send exactly ONE of them to the holdout.
    let (wf_result, chosen_label, grid_size) = if sweep && args.single_pass {
        let p = IctParams::liquidity_sweep_v1();
        println!("study        : consolidated liquidity sweep (v1)");
        println!("method       : SINGLE PASS, frozen config, no walk-forward");
        let r = run_backtest(
            &db,
            &cfg,
            Box::new(IctStrategy::new(p)),
            RiskManager::new(risk_params.clone(), stop_offset),
        )
        .await?;
        let m = backtest::metrics::compute(&r.trades, cfg.starting_equity);
        (
            RunOutcome {
                oos_metrics: m,
                // One pass has no folds to break down.
                fold_lines: Vec::new(),
            },
            "liquidity_sweep_v1".to_string(),
            1,
        )
    } else if sweep {
        // ONE configuration, no variant sweep: everything in it was already
        // selected by measuring changes individually. Running a grid here
        // would be selecting twice on the same data.
        let p = IctParams::liquidity_sweep_v1();
        println!("study        : consolidated liquidity sweep (v1)");
        println!("variants     : 1 (no search — the config is fixed)");
        let r = run_walk_forward(
            &db,
            &cfg,
            &wf,
            std::slice::from_ref(&p),
            &risk_params,
            stop_offset,
            &|q| Box::new(IctStrategy::new(q.clone())),
        )
        .await?;
        (summarise_folds(&r), "liquidity_sweep_v1".to_string(), 1)
    } else if ict {
        let mut declared = IctParams::declared_variants();
        if let Some(want) = &args.variant {
            declared.retain(|(name, _)| name.eq_ignore_ascii_case(want));
            if declared.is_empty() {
                return Err(format!("unknown variant {:?}", args.variant).into());
            }
        }
        println!("study        : ICT structural (pre-registered)");
        println!("variants     : {}", declared.len());

        let n = declared.len();
        let mut best: Option<(String, backtest::walk_forward::WalkForwardResult<IctParams>)> = None;
        for (name, p) in &declared {
            let r = run_walk_forward(
                &db,
                &cfg,
                &wf,
                std::slice::from_ref(p),
                &risk_params,
                stop_offset,
                &|q| Box::new(IctStrategy::new(q.clone())),
            )
            .await?;
            println!(
                "  variant {name}: trades {:>5}  expectancy {:>12}  PF {:>8}  worstFoldDD {:>7}%",
                r.oos_metrics.trade_count,
                r.oos_metrics.expectancy.round_dp(4),
                r.oos_metrics
                    .profit_factor
                    .map(|v| v.round_dp(3).to_string())
                    .unwrap_or_else(|| "undef".into()),
                r.oos_metrics.max_drawdown_pct.round_dp(2)
            );
            let eligible = r.oos_metrics.trade_count >= thresholds.min_trades;
            let better = match &best {
                None => eligible,
                Some((_, b)) => eligible && r.oos_metrics.expectancy > b.oos_metrics.expectancy,
            };
            if better {
                best = Some((name.to_string(), r));
            }
        }
        match best {
            Some((name, r)) => {
                println!("\nselected variant: {name}");
                (summarise_folds(&r), name, n)
            }
            None => {
                println!("\n---------------- VERDICT: FAIL ----------------");
                println!(
                    "Reason: NO VARIANT reached {} out-of-sample trades.",
                    thresholds.min_trades
                );
                println!("The study fails at stage 1 for insufficient setup frequency.");
                println!("The holdout is NOT opened. No definition was loosened.");
                return Ok(());
            }
        }
    } else if reversion {
        let mut declared = ReversionParams::declared_variants();
        if let Some(want) = &args.variant {
            declared.retain(|(name, _)| name.eq_ignore_ascii_case(want));
            if declared.is_empty() {
                return Err(format!("unknown variant {:?}", args.variant).into());
            }
        }
        // A variant whose 2R target sits past the mean would need price to
        // overshoot the level it is reverting to. Refuse rather than quietly
        // produce numbers for an incoherent setup.
        for (name, p) in &declared {
            if !p.target_lands_before_mean(stop_offset) {
                return Err(format!("variant {name} targets past the mean; invalid").into());
            }
        }

        println!("study        : mean-reversion (pre-registered)");
        println!("variants     : {}", declared.len());
        println!(
            "benchmark bar: p{} over {seed_count} seeds",
            thresholds.benchmark_percentile
        );

        let n = declared.len();
        let mut best: Option<(
            String,
            backtest::walk_forward::WalkForwardResult<ReversionParams>,
        )> = None;
        for (name, p) in &declared {
            let r = run_walk_forward(
                &db,
                &cfg,
                &wf,
                std::slice::from_ref(p),
                &risk_params,
                stop_offset,
                &|q| Box::new(ReversionStrategy::new(q.clone())),
            )
            .await?;
            println!(
                "  variant {name}: trades {:>5}  expectancy {:>12}  PF {:>8}  worstFoldDD {:>7}%",
                r.oos_metrics.trade_count,
                r.oos_metrics.expectancy.round_dp(4),
                r.oos_metrics
                    .profit_factor
                    .map(|v| v.round_dp(3).to_string())
                    .unwrap_or_else(|| "undef".into()),
                r.oos_metrics.max_drawdown_pct.round_dp(2)
            );

            // Selection rule fixed in the spec: highest OOS expectancy among
            // variants that produced at least the minimum trade count.
            let eligible = r.oos_metrics.trade_count >= thresholds.min_trades;
            let better = match &best {
                None => eligible,
                Some((_, b)) => eligible && r.oos_metrics.expectancy > b.oos_metrics.expectancy,
            };
            if better {
                best = Some((name.to_string(), r));
            }
        }

        match best {
            Some((name, r)) => {
                println!(
                    "\nselected variant: {name} (highest OOS expectancy with >= {} trades)",
                    thresholds.min_trades
                );
                (summarise_folds(&r), name, n)
            }
            None => {
                println!("\n---------------- VERDICT: FAIL ----------------");
                println!(
                    "Reason: NO VARIANT reached {} out-of-sample trades.",
                    thresholds.min_trades
                );
                println!("The study fails at stage 1 for insufficient signal frequency.");
                println!("The holdout is NOT opened. No threshold was adjusted.");
                return Ok(());
            }
        }
    } else {
        let grid = parameter_grid();
        let n = grid.len();
        let r = run_walk_forward(&db, &cfg, &wf, &grid, &risk_params, stop_offset, &|p| {
            Box::new(PullbackStrategy::new(p.clone()))
        })
        .await?;
        (summarise_folds(&r), "pullback".to_string(), n)
    };

    let seeds: Vec<u64> = (1..=seed_count).collect();
    println!(
        "running random-entry benchmark over {} seeds...",
        seeds.len()
    );
    let benchmark = run_benchmark(
        &db,
        &cfg,
        &risk_params,
        stop_offset,
        &seeds,
        benchmark_probability(),
        &StrategyParams::defaults(),
        vec![Timeframe::H1, Timeframe::H4],
        cfg.warmup_candles,
    )
    .await?;

    let verdict = evaluate(&wf_result.oos_metrics, &benchmark, &thresholds);

    // The verdict comes FIRST, before any supporting number, so a reader
    // cannot absorb encouraging figures before learning the answer.
    println!(
        "\n---------------- VERDICT: {} ----------------",
        if verdict.passed { "PASS" } else { "FAIL" }
    );
    for c in &verdict.criteria {
        println!(
            "  [{}] {:<30} actual {:<34} required {}",
            if c.passed { "PASS" } else { "FAIL" },
            c.criterion.label(),
            c.actual,
            c.required
        );
    }

    let m = &wf_result.oos_metrics;
    println!("\n---- out-of-sample metrics (the only evidence) ----");
    println!("  trades            : {}", m.trade_count);
    println!("  wins / losses     : {} / {}", m.wins, m.losses);
    println!("  win rate          : {}", m.win_rate.round_dp(4));
    println!("  expectancy        : {}", m.expectancy.round_dp(4));
    println!("  net pnl           : {}", m.net_pnl.round_dp(2));
    // Separate line items: a strategy paying its entire edge away in costs
    // must be visibly doing so.
    println!("  fees paid         : {}", m.total_fees.round_dp(2));
    println!("  funding paid      : {}", m.total_funding.round_dp(2));
    println!("  max drawdown      : {}%", m.max_drawdown_pct.round_dp(2));

    let ambiguous_pct = if m.trade_count == 0 {
        Decimal::ZERO
    } else {
        Decimal::from(m.ambiguous_exits) / Decimal::from(m.trade_count) * Decimal::ONE_HUNDRED
    };
    println!(
        "  ambiguous exits   : {} ({}% of trades)",
        m.ambiguous_exits,
        ambiguous_pct.round_dp(2)
    );
    if ambiguous_pct > Decimal::from(20) {
        println!("  WARNING: a large share of exits hit the intra-candle ambiguity rule, so this");
        println!(
            "           result rests substantially on the pessimistic assumption, not on data."
        );
    }

    println!("\n---- per fold (a result driven by one lucky fold is visible here) ----");
    for f in &wf_result.fold_lines {
        println!(
            "  fold {:>2}  trades {:>5}  expectancy {:>12}  maxDD {:>7}%",
            f.index,
            f.trades,
            f.expectancy.round_dp(4),
            f.drawdown_pct.round_dp(2)
        );
    }

    let sorted = benchmark.sorted_expectancies();
    println!("\n---- random-entry benchmark ----");
    println!(
        "  seeds             : {} (1..={seed_count})",
        benchmark.seeds.len()
    );
    println!("  signal probability: {}", benchmark_probability());
    println!(
        "  median            : {}",
        percentile(&sorted, Decimal::from(50)).round_dp(4)
    );
    println!(
        "  95th percentile   : {}",
        percentile(&sorted, Decimal::from(95)).round_dp(4)
    );
    println!("  strategy          : {}", m.expectancy.round_dp(4));

    println!("\n---- reproducibility ----");
    println!("  db            : {}", args.db_path);
    println!("  range         : {start_ms} .. {end_ms}");
    println!("  maker fee     : {maker_fee_rate}");
    println!("  grid entries  : {grid_size}");
    println!("  seeds         : 1..={seed_count}");
    println!("  study         : {} / variant {chosen_label}", args.study);

    if verdict.passed {
        println!("\n---- limitations that survive a PASS ----");
        println!("  * Delisted symbols are absent from Bybit's kline history, so results are");
        println!("    biased toward survivors by an amount this data cannot measure.");
        println!("  * The intra-candle path is unknown; every ambiguous exit assumed the stop");
        println!("    filled first, which is pessimistic but still an assumption.");
        println!("  * Stop-escalation fidelity is limited at candle resolution; only the");
        println!("    testnet soak validates that ladder.");
        println!("  * The maker fee is corroborated, not primary-sourced. Confirm it against");
        println!("    Bybit's own schedule before acting on this.");
        println!("  * A passing backtest is EVIDENCE, NOT PROOF. Size small on real money.");
    }

    println!();
    Ok(())
}
