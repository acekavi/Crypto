use bot::config::Profile;
use bot::pairs_backtest::{
    choose_walk_forward_winner, evaluate_parameter_candidate, render_walk_forward_audit_text,
    walk_forward_folds_for_range, CompactBacktestResult, SplitBacktestSummary,
    WalkForwardAuditFoldSummary, WalkForwardFoldWindow,
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

fn summary(tag: &str, is_pf: f64, is_fixed_pf: f64) -> SplitBacktestSummary {
    let _ = tag;
    SplitBacktestSummary {
        pair: "AAVEUSDT/ETHUSDT".into(),
        train: compact(40, 0.60, is_pf, 2.0, 15.0),
        holdout: compact(12, 0.58, 1.4, 0.6, 12.0),
        full: compact(52, 0.55, 1.6, 2.5, 22.0),
        fixed_notional_usdt: "25".into(),
        fixed_notional_train: compact(40, 0.60, is_fixed_pf, 9.0, 12.0),
        fixed_notional_holdout: compact(12, 0.58, 1.5, 1.1, 11.0),
        fixed_notional_full: compact(52, 0.55, 1.8, 11.0, 15.0),
        recent_trades: Vec::new(),
    }
}

#[test]
fn walk_forward_folds_tile_without_overlap() {
    let folds = walk_forward_folds_for_range(0, 360 * 86_400_000, 180 * 86_400_000, 60 * 86_400_000);
    assert_eq!(folds.len(), 3);
    assert_eq!(folds[0].is_start_ms, 0);
    assert_eq!(folds[0].oos_start_ms, 180 * 86_400_000);
    assert_eq!(folds[1].is_start_ms, 60 * 86_400_000);
    assert_eq!(folds[0].oos_end_ms, folds[1].oos_start_ms);
}

#[test]
fn walk_forward_winner_uses_the_prepared_in_sample_scores() {
    let cfg = load_pairs_config(Profile::Testnet).expect("config");
    let params = cfg.bots.iter().find(|b| b.id == "aave_eth").expect("bot").params.clone();
    let mut better_is = evaluate_parameter_candidate("better_is", &params, summary("a", 2.5, 3.0), 8);
    let mut worse_is = evaluate_parameter_candidate("worse_is", &params, summary("b", 1.2, 1.3), 8);
    better_is.audit_score = 20.0;
    worse_is.audit_score = 5.0;
    let winner = choose_walk_forward_winner(&[worse_is.clone(), better_is.clone()]).expect("winner");
    assert_eq!(winner.label, "better_is");
}

#[test]
fn walk_forward_report_lists_fold_choices_and_scoreboard() {
    let report = render_walk_forward_audit_text(
        "AAVE/ETH",
        5,
        &[
            WalkForwardAuditFoldSummary {
                fold: WalkForwardFoldWindow { index: 0, is_start_ms: 0, is_end_ms: 10, oos_start_ms: 10, oos_end_ms: 20 },
                chosen_label: "w180-e3.00-s4.25-t0.00-h72".into(),
                in_sample_score: 12.3,
                oos_trades: 9,
                oos_profit_factor: 1.8,
                oos_fixed_profit_factor: 2.1,
            },
        ],
        &[("w180-e3.00-s4.25-t0.00-h72".into(), 2usize, 1.8, 2.1)],
    );
    assert!(report.contains("Walk-forward audit"));
    assert!(report.contains("Fold winners"));
    assert!(report.contains("Candidate scoreboard"));
    assert!(report.contains("w180-e3.00-s4.25-t0.00-h72"));
}
