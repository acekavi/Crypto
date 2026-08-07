use backtest::gate::{Criterion, GateThresholds, evaluate};
use backtest::metrics::Metrics;
use backtest::random_entry::BenchmarkDistribution;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;

/// A benchmark whose 95th percentile is exactly 95, so a strategy expectancy
/// above it clears the bar and one at or below it does not.
fn benchmark() -> BenchmarkDistribution {
    BenchmarkDistribution {
        seeds: (1..=100).collect(),
        expectancies: (1..=100).map(Decimal::from).collect(),
    }
}

/// Metrics that clear all five criteria comfortably, so each test below can
/// break exactly one and attribute the failure to it.
fn passing_metrics() -> Metrics {
    Metrics {
        trade_count: 250,
        wins: 150,
        losses: 100,
        win_rate: dec!(0.6),
        expectancy: dec!(120),
        gross_profit: dec!(30000),
        gross_loss: dec!(10000),
        profit_factor: Some(dec!(3)),
        max_drawdown_pct: dec!(10),
        total_fees: dec!(100),
        total_funding: dec!(50),
        net_pnl: dec!(30000),
        ambiguous_exits: 0,
    }
}

fn verdict_for(m: Metrics) -> backtest::gate::Verdict {
    evaluate(&m, &benchmark(), &GateThresholds::pre_registered())
}

#[test]
fn all_five_satisfied_passes() {
    let v = verdict_for(passing_metrics());
    assert!(v.passed, "criteria: {:?}", v.criteria);
    assert_eq!(v.criteria.len(), 5);
    assert!(v.criteria.iter().all(|c| c.passed));
}

#[test]
fn a_non_positive_expectancy_alone_fails_the_verdict() {
    let mut m = passing_metrics();
    m.expectancy = dec!(0);
    let v = verdict_for(m);
    assert!(!v.passed);
    assert!(
        !v.criteria
            .iter()
            .find(|c| c.criterion == Criterion::Expectancy)
            .expect("expectancy criterion")
            .passed
    );
}

#[test]
fn too_few_trades_alone_fails_the_verdict() {
    let mut m = passing_metrics();
    m.trade_count = 199;
    let v = verdict_for(m);
    assert!(!v.passed);
    assert!(
        !v.criteria
            .iter()
            .find(|c| c.criterion == Criterion::TradeCount)
            .expect("trade count criterion")
            .passed
    );
}

#[test]
fn exactly_two_hundred_trades_passes_the_boundary() {
    // The threshold is inclusive; 200 is enough, 199 is not.
    let mut m = passing_metrics();
    m.trade_count = 200;
    assert!(verdict_for(m).passed);
}

#[test]
fn too_deep_a_drawdown_alone_fails_the_verdict() {
    let mut m = passing_metrics();
    m.max_drawdown_pct = dec!(15.01);
    let v = verdict_for(m);
    assert!(!v.passed);
    assert!(
        !v.criteria
            .iter()
            .find(|c| c.criterion == Criterion::MaxDrawdown)
            .expect("drawdown criterion")
            .passed
    );
}

#[test]
fn exactly_fifteen_percent_drawdown_passes_the_boundary() {
    let mut m = passing_metrics();
    m.max_drawdown_pct = dec!(15);
    assert!(verdict_for(m).passed);
}

#[test]
fn too_low_a_profit_factor_alone_fails_the_verdict() {
    let mut m = passing_metrics();
    m.profit_factor = Some(dec!(1.29));
    let v = verdict_for(m);
    assert!(!v.passed);
    assert!(
        !v.criteria
            .iter()
            .find(|c| c.criterion == Criterion::ProfitFactor)
            .expect("profit factor criterion")
            .passed
    );
}

#[test]
fn an_undefined_profit_factor_fails_and_says_why() {
    // No losing trades. Not a flattering infinity — such a sample has not
    // demonstrated a profit factor, only that it is too small.
    let mut m = passing_metrics();
    m.profit_factor = None;
    let v = verdict_for(m);
    assert!(!v.passed);
    let c = v
        .criteria
        .iter()
        .find(|c| c.criterion == Criterion::ProfitFactor)
        .expect("profit factor criterion");
    assert!(!c.passed);
    assert!(
        c.actual.contains("no losing trades"),
        "the reason must be legible, got {:?}",
        c.actual
    );
}

#[test]
fn failing_to_beat_the_benchmark_alone_fails_the_verdict() {
    let mut m = passing_metrics();
    // Below the 95th percentile of 1..=100, which is 95.
    m.expectancy = dec!(94);
    let v = verdict_for(m);
    assert!(!v.passed);
    assert!(
        !v.criteria
            .iter()
            .find(|c| c.criterion == Criterion::BenchmarkPercentile)
            .expect("benchmark criterion")
            .passed
    );
}

#[test]
fn merely_matching_the_benchmark_is_not_beating_it() {
    // Strictly greater is required: tying with the 95th-percentile random run
    // is not evidence of an entry edge.
    let mut m = passing_metrics();
    m.expectancy = dec!(95);
    let v = verdict_for(m);
    assert!(!v.passed);
}

#[test]
fn a_failing_verdict_still_reports_all_five_criteria() {
    // How far off the OTHER criteria were is the most useful thing a FAIL
    // carries, so nothing short-circuits.
    let mut m = passing_metrics();
    m.expectancy = dec!(-500);
    m.trade_count = 3;
    m.max_drawdown_pct = dec!(80);
    m.profit_factor = Some(dec!(0.2));
    let v = verdict_for(m);

    assert!(!v.passed);
    assert_eq!(
        v.criteria.len(),
        5,
        "every criterion must still be reported"
    );
    assert_eq!(
        v.criteria.iter().filter(|c| !c.passed).count(),
        5,
        "all five genuinely failed here"
    );
}

#[test]
fn the_pre_registered_thresholds_are_exactly_what_the_spec_fixed() {
    // A TRIPWIRE, not a formality. These numbers were fixed in the approved
    // spec before any result was seen. If someone edits one, this fails
    // loudly rather than silently changing what PASS means.
    let t = GateThresholds::pre_registered();
    assert_eq!(t.min_expectancy, dec!(0));
    assert_eq!(t.min_trades, 200);
    assert_eq!(t.max_drawdown_pct, dec!(15));
    assert_eq!(t.min_profit_factor, dec!(1.3));
    assert_eq!(t.benchmark_percentile, dec!(95));
}

#[test]
fn the_study_bar_tightens_only_the_benchmark_and_never_the_shared_one() {
    // A study may raise its own bar; it may never lower the shared one. The
    // mean-reversion study picks one variant from six, so reporting the best
    // is six chances to find noise — Bonferroni spends the 5% across the
    // looks. Every other criterion must be untouched.
    let base = GateThresholds::pre_registered();
    let study = GateThresholds::mean_reversion_study();

    assert_eq!(study.benchmark_percentile, dec!(99.17));
    assert!(
        study.benchmark_percentile > base.benchmark_percentile,
        "a study bar must be stricter, never looser"
    );
    assert_eq!(study.min_expectancy, base.min_expectancy);
    assert_eq!(study.min_trades, base.min_trades);
    assert_eq!(study.max_drawdown_pct, base.max_drawdown_pct);
    assert_eq!(study.min_profit_factor, base.min_profit_factor);
}
