//! The pre-registered pass/fail gate.
//!
//! These thresholds were fixed in the approved spec BEFORE any result was
//! seen. That is the whole point: a bar chosen after looking at a
//! disappointing number is not a bar. `GateThresholds::pre_registered` has its
//! own test asserting the exact values, so editing one fails loudly rather
//! than silently changing what PASS means.
//!
//! A FAIL is a successful outcome — it means the tooling stopped a losing
//! strategy from reaching real money.

use rust_decimal::Decimal;

use crate::metrics::Metrics;
use crate::random_entry::{BenchmarkDistribution, percentile};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateThresholds {
    pub min_expectancy: Decimal,
    pub min_trades: usize,
    pub max_drawdown_pct: Decimal,
    pub min_profit_factor: Decimal,
    pub benchmark_percentile: Decimal,
}

impl GateThresholds {
    /// The approved spec's values. **Do not change these here.** Changing the
    /// bar is the owner's decision, made deliberately and visibly, never a
    /// quiet edit while looking at a result.
    pub fn pre_registered() -> Self {
        GateThresholds {
            min_expectancy: Decimal::ZERO,
            min_trades: 200,
            max_drawdown_pct: Decimal::from(15),
            min_profit_factor: Decimal::new(13, 1),
            benchmark_percentile: Decimal::from(95),
        }
    }
}

impl GateThresholds {
    /// The mean-reversion study's bar, from
    /// `docs/superpowers/specs/2026-08-07-mean-reversion-study-design.md`.
    ///
    /// Four criteria are IDENTICAL to `pre_registered`. Only the benchmark
    /// percentile is tightened, because that study selects one variant from
    /// six and reporting the best of six is six chances to find noise:
    /// Bonferroni spends the 5% across the looks, giving p(100 - 5/6).
    ///
    /// A SEPARATE constructor rather than an edit to `pre_registered`, whose
    /// tripwire test must keep passing untouched — a study may raise its own
    /// bar, never lower the shared one.
    pub fn mean_reversion_study() -> Self {
        GateThresholds {
            benchmark_percentile: Decimal::new(9917, 2), // p99.17
            ..Self::pre_registered()
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Criterion {
    Expectancy,
    TradeCount,
    MaxDrawdown,
    ProfitFactor,
    BenchmarkPercentile,
}

impl Criterion {
    pub fn label(self) -> &'static str {
        match self {
            Criterion::Expectancy => "OOS expectancy",
            Criterion::TradeCount => "OOS trade count",
            Criterion::MaxDrawdown => "Max drawdown",
            Criterion::ProfitFactor => "Profit factor",
            Criterion::BenchmarkPercentile => "Beats random-entry benchmark",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CriterionResult {
    pub criterion: Criterion,
    pub passed: bool,
    pub actual: String,
    pub required: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    pub passed: bool,
    pub criteria: Vec<CriterionResult>,
}

/// Evaluate all five criteria against out-of-sample results.
///
/// Every criterion is evaluated and reported even after one has already
/// failed. Short-circuiting would hide how far off the others were, which is
/// the most useful information a FAIL carries.
pub fn evaluate(
    oos: &Metrics,
    benchmark: &BenchmarkDistribution,
    thresholds: &GateThresholds,
) -> Verdict {
    let mut criteria = Vec::new();

    criteria.push(CriterionResult {
        criterion: Criterion::Expectancy,
        passed: oos.expectancy > thresholds.min_expectancy,
        actual: format!("{}", oos.expectancy.round_dp(4)),
        required: format!("> {}", thresholds.min_expectancy),
    });

    criteria.push(CriterionResult {
        criterion: Criterion::TradeCount,
        passed: oos.trade_count >= thresholds.min_trades,
        actual: oos.trade_count.to_string(),
        required: format!(">= {}", thresholds.min_trades),
    });

    criteria.push(CriterionResult {
        criterion: Criterion::MaxDrawdown,
        passed: oos.max_drawdown_pct <= thresholds.max_drawdown_pct,
        actual: format!("{}%", oos.max_drawdown_pct.round_dp(2)),
        required: format!("<= {}%", thresholds.max_drawdown_pct),
    });

    // `None` means no losing trades, which FAILS: such a sample has not
    // demonstrated a profit factor, it has demonstrated that it is too small.
    let (pf_passed, pf_actual) = match oos.profit_factor {
        Some(pf) => (
            pf >= thresholds.min_profit_factor,
            format!("{}", pf.round_dp(4)),
        ),
        None => (false, "undefined — no losing trades".to_string()),
    };
    criteria.push(CriterionResult {
        criterion: Criterion::ProfitFactor,
        passed: pf_passed,
        actual: pf_actual,
        required: format!(">= {}", thresholds.min_profit_factor),
    });

    // Strictly greater: matching the benchmark is not beating it.
    let sorted = benchmark.sorted_expectancies();
    let bar = percentile(&sorted, thresholds.benchmark_percentile);
    criteria.push(CriterionResult {
        criterion: Criterion::BenchmarkPercentile,
        passed: oos.expectancy > bar,
        actual: format!(
            "{} vs benchmark {}",
            oos.expectancy.round_dp(4),
            bar.round_dp(4)
        ),
        required: format!(
            "> p{} of {} random runs",
            thresholds.benchmark_percentile,
            benchmark.seeds.len()
        ),
    });

    // Every criterion, no weighting, no partial credit.
    let passed = criteria.iter().all(|c| c.passed);
    Verdict { passed, criteria }
}
