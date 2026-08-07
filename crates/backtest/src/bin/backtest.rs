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
use backtest::{BacktestConfig, CostModel};
use botcore::{Instrument, Symbol, Timeframe};
use history::HistoryDb;
use risk::RiskParams;
use rust_decimal::Decimal;
use strategy::pullback::{PullbackStrategy, StrategyParams};

/// Seeds for the random-entry benchmark. Fixed and recorded so a reported
/// percentile can be reproduced exactly by anyone re-running this.
const BENCHMARK_SEEDS: std::ops::RangeInclusive<u64> = 1..=100;

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

struct Args {
    db_path: String,
    starting_equity: Decimal,
}

fn parse_args() -> Result<Args, Box<dyn std::error::Error>> {
    let mut db_path = "data/history.db".to_string();
    let mut starting_equity = Decimal::from(10_000);

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
            other => return Err(format!("unrecognised argument: {other}").into()),
        }
    }
    Ok(Args {
        db_path,
        starting_equity,
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
    let start_ms = stored.iter().map(|(_, _, e, _)| *e).max().unwrap_or(0);
    let end_ms = stored.iter().map(|(_, _, _, l)| *l).min().unwrap_or(0);

    let cfg = BacktestConfig {
        start_ms,
        end_ms,
        starting_equity: args.starting_equity,
        symbols: symbols.clone(),
        instruments: symbols.iter().map(placeholder_instrument).collect(),
        costs: CostModel { maker_fee_rate },
        warmup_candles: 250,
        entry_expiry_candles: 3,
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

    let grid = parameter_grid();
    let risk_params = RiskParams::defaults();
    let stop_offset = Decimal::new(3, 1);

    println!("\nrunning walk-forward over {} folds...", available.len());
    let wf_result = run_walk_forward(&db, &cfg, &wf, &grid, &risk_params, stop_offset, &|p| {
        Box::new(PullbackStrategy::new(p.clone()))
    })
    .await?;

    let seeds: Vec<u64> = BENCHMARK_SEEDS.collect();
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

    let thresholds = GateThresholds::pre_registered();
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
    for f in &wf_result.folds {
        println!(
            "  fold {:>2}  trades {:>5}  expectancy {:>12}  maxDD {:>7}%",
            f.fold.index,
            f.oos_metrics.trade_count,
            f.oos_metrics.expectancy.round_dp(4),
            f.oos_metrics.max_drawdown_pct.round_dp(2)
        );
    }

    let sorted = benchmark.sorted_expectancies();
    println!("\n---- random-entry benchmark ----");
    println!("  seeds             : {} (1..=100)", benchmark.seeds.len());
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
    println!("  grid entries  : {}", grid.len());
    println!("  seeds         : 1..=100");

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
