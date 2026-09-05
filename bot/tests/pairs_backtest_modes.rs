use bot::config::Profile;
use bot::pairs_backtest::split_summary;
use bot::pairs_config::load_pairs_config;

#[tokio::test]
async fn split_summary_reports_both_risk_sized_and_fixed_notional_views() {
    let cfg = load_pairs_config(Profile::Testnet).expect("config");
    let bot = cfg.bots.iter().find(|b| b.id == "aave_eth").expect("aave");
    let summary = split_summary("/home/acekavi/Projects/Crypto/data/history.db", &bot.params, 0.70)
        .await
        .expect("summary");
    assert!(summary.full.trades > 0);
    assert!(summary.fixed_notional_full.trades > 0);
    assert_eq!(summary.fixed_notional_usdt, "25");
}
