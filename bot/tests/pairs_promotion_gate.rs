use bot::pairs_backtest::{
    classify_walk_forward_candidate, render_promotion_gate_text, PromotionDecision,
    PromotionVerdict, WalkForwardCandidateScoreboardRow,
};

fn row(
    label: &str,
    folds_selected: usize,
    oos_trades: usize,
    oos_profit_factor: f64,
    oos_fixed_profit_factor: f64,
    oos_net: f64,
    oos_fixed_net: f64,
) -> WalkForwardCandidateScoreboardRow {
    WalkForwardCandidateScoreboardRow {
        label: label.into(),
        folds_selected,
        oos_trades,
        oos_profit_factor,
        oos_fixed_profit_factor,
        oos_net,
        oos_fixed_net,
    }
}

#[test]
fn promotion_gate_marks_balanced_candidate_as_promote() {
    let decision = classify_walk_forward_candidate(&row("steady", 3, 28, 1.48, 1.62, 0.84, 9.4));
    assert_eq!(decision.verdict, PromotionVerdict::Promote);
    assert!(decision.reasons.is_empty());
}

#[test]
fn promotion_gate_marks_thin_but_promising_candidate_as_watchlist() {
    let decision = classify_walk_forward_candidate(&row("thin", 1, 7, 4.8, 5.1, 0.31, 3.2));
    assert_eq!(decision.verdict, PromotionVerdict::Watchlist);
    assert!(decision.reasons.iter().any(|r| r.contains("folds selected")));
}

#[test]
fn promotion_gate_rejects_candidate_with_weak_oos_profit_factor() {
    let decision = classify_walk_forward_candidate(&row("weak", 3, 26, 0.92, 0.97, -0.14, -1.1));
    assert_eq!(decision.verdict, PromotionVerdict::Reject);
    assert!(decision.reasons.iter().any(|r| r.contains("OOS PF")));
}

#[test]
fn promotion_text_lists_all_three_verdict_buckets() {
    let text = render_promotion_gate_text(
        "AAVE/ETH",
        &[
            PromotionDecision {
                verdict: PromotionVerdict::Promote,
                candidate: row("steady", 3, 28, 1.48, 1.62, 0.84, 9.4),
                reasons: vec![],
            },
            PromotionDecision {
                verdict: PromotionVerdict::Watchlist,
                candidate: row("thin", 1, 7, 4.8, 5.1, 0.31, 3.2),
                reasons: vec!["folds selected 1 below promote minimum 2".into()],
            },
            PromotionDecision {
                verdict: PromotionVerdict::Reject,
                candidate: row("weak", 3, 26, 0.92, 0.97, -0.14, -1.1),
                reasons: vec!["OOS PF 0.920 must be > 1.0".into()],
            },
        ],
    );
    assert!(text.contains("Promotion gate"));
    assert!(text.contains("Promote"));
    assert!(text.contains("Watchlist"));
    assert!(text.contains("Reject"));
}
