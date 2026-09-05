use bot::config::Profile;
use bot::pairs_backtest::{
    evaluate_parameter_candidate, rank_parameter_candidates, rejected_parameter_candidate,
    render_parameter_audit_text, CompactBacktestResult, SplitBacktestSummary,
};
use bot::pairs_config::load_pairs_config;

fn compact(trades: usize, win_rate: f64, pf: f64, net: f64, dd: f64) -> CompactBacktestResult {
    CompactBacktestResult {
        trades,
        wins: ((trades as f64) * win_rate).round() as usize,
        losses: trades - (((trades as f64) * win_rate).round() as usize),
        win_rate,
        profit_factor: pf,
        net,
        avg_trade: if trades == 0 { 0.0 } else { net / trades as f64 },
        max_drawdown_pct: dd,
    }
}

fn summary(hold_trades: usize, hold_pf: f64, hold_fixed_pf: f64, full_net: f64, full_fixed_net: f64) -> SplitBacktestSummary {
    SplitBacktestSummary {
        pair: "AAVEUSDT/ETHUSDT".into(),
        train: compact(40, 0.60, 1.4, 0.4, 20.0),
        holdout: compact(hold_trades, 0.55, hold_pf, 0.3, 12.0),
        full: compact(90, 0.58, 1.8, full_net, 22.0),
        fixed_notional_usdt: "25".into(),
        fixed_notional_train: compact(40, 0.60, 1.8, 15.0, 18.0),
        fixed_notional_holdout: compact(hold_trades, 0.55, hold_fixed_pf, 8.0, 10.0),
        fixed_notional_full: compact(90, 0.58, 2.1, full_fixed_net, 19.0),
        recent_trades: Vec::new(),
    }
}

#[test]
fn audit_rejects_candidates_with_shallow_holdout_trade_count() {
    let cfg = load_pairs_config(Profile::Testnet).expect("config");
    let params = cfg.bots.iter().find(|b| b.id == "aave_eth").expect("bot").params.clone();
    let candidate = evaluate_parameter_candidate("too_shallow", &params, summary(7, 2.0, 2.5, 5.0, 20.0), 8);
    assert!(!candidate.accepted);
    assert!(candidate.rejection_reasons.iter().any(|r| r.contains("holdout trades")));
}

#[test]
fn audit_ranking_prefers_balanced_candidates_over_compounding_only_mirages() {
    let cfg = load_pairs_config(Profile::Testnet).expect("config");
    let params = cfg.bots.iter().find(|b| b.id == "aave_eth").expect("bot").params.clone();
    let balanced = evaluate_parameter_candidate("balanced", &params, summary(14, 2.4, 3.1, 4.0, 18.0), 8);
    let mirage = evaluate_parameter_candidate("mirage", &params, summary(14, 1.05, 0.92, 80.0, 2.0), 8);
    let ranked = rank_parameter_candidates(vec![mirage.clone(), balanced.clone()]);
    assert_eq!(ranked[0].label, "balanced");
    assert!(ranked[0].accepted);
    assert!(!ranked[1].accepted);
}

#[test]
fn audit_text_report_calls_out_filters_and_top_candidate() {
    let cfg = load_pairs_config(Profile::Testnet).expect("config");
    let params = cfg.bots.iter().find(|b| b.id == "aave_eth").expect("bot").params.clone();
    let accepted = evaluate_parameter_candidate("current", &params, summary(12, 2.2, 2.8, 3.0, 16.0), 8);
    let rejected = evaluate_parameter_candidate("bad_holdout", &params, summary(5, 2.0, 2.2, 9.0, 12.0), 8);
    let text = render_parameter_audit_text("AAVE/ETH", &[accepted, rejected], 8, 1);
    assert!(text.contains("Parameter audit"));
    assert!(text.contains("Top accepted candidates"));
    assert!(text.contains("Rejected candidates"));
    assert!(text.contains("bad_holdout"));
}

#[test]
fn audit_can_keep_backtest_errors_as_rejected_candidates() {
    let cfg = load_pairs_config(Profile::Testnet).expect("config");
    let params = cfg
        .bots
        .iter()
        .find(|b| b.id == "aave_eth")
        .expect("bot")
        .params
        .clone();
    let candidate = rejected_parameter_candidate(
        "bad_combo",
        &params,
        "sizing: per-leg notional resolved to zero or less",
    );
    assert!(!candidate.accepted);
    assert!(candidate
        .rejection_reasons
        .iter()
        .any(|r| r.contains("zero or less")));
}
