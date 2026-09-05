use bot::config::Profile;
use bot::pairs_config::load_pairs_config;
use bot::pairs_report::build_portfolio_manifest;

#[test]
fn manifest_summarises_the_live_three_pair_portfolio_under_one_service() {
    let cfg = load_pairs_config(Profile::Testnet).expect("config loads");
    let manifest = build_portfolio_manifest(&cfg.bots);
    assert_eq!(manifest.pair_count, 3);
    assert_eq!(manifest.services, vec!["crypto-pairs.service"]);
    assert!(!manifest.has_symbol_overlap);
    assert_eq!(
        manifest.symbols,
        vec!["AAVEUSDT", "BNBUSDT", "ENAUSDT", "ETHUSDT", "XAUTUSDT", "XRPUSDT"]
    );
}
