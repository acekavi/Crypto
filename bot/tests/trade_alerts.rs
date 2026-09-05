use bot::trade_alerts::{
    format_trade_alert, should_alert_for_outcome, AlertEvent, AlertOutcome,
};

#[test]
fn open_and_close_outcomes_are_alertable_but_idle_ones_are_not() {
    assert!(should_alert_for_outcome(AlertOutcome::Opened));
    assert!(should_alert_for_outcome(AlertOutcome::Closed));
    assert!(!should_alert_for_outcome(AlertOutcome::NoSignal));
    assert!(!should_alert_for_outcome(AlertOutcome::Unchanged));
    assert!(!should_alert_for_outcome(AlertOutcome::Deferred));
}

#[test]
fn formatted_trade_alert_contains_pair_side_and_reason() {
    let msg = format_trade_alert(&AlertEvent {
        bot_id: "aave_eth".into(),
        display_name: "AAVE/ETH".into(),
        at_ms: 1788627600000,
        outcome: AlertOutcome::Closed,
        side: Some("short_spread".into()),
        detail: "exit reason=time".into(),
    });
    assert!(msg.contains("AAVE/ETH"));
    assert!(msg.contains("closed"));
    assert!(msg.contains("short_spread"));
    assert!(msg.contains("exit reason=time"));
}
