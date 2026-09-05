use bot::config::Profile;
use bot::pairs_config::load_pairs_config;

#[test]
fn live_pairs_config_matches_the_hardened_parameter_set() {
    let cfg = load_pairs_config(Profile::Testnet).expect("config");
    let aave = cfg.bots.iter().find(|b| b.id == "aave_eth").unwrap();
    assert_eq!(aave.params.rolling_window, 210);
    assert_eq!(aave.params.entry_z, 3.0);
    assert_eq!(aave.params.stop_z, 4.5);
    assert_eq!(aave.params.max_hold_bars, 48);

    let ena = cfg.bots.iter().find(|b| b.id == "ena_xrp").unwrap();
    assert_eq!(ena.params.entry_z, 3.25);
    assert_eq!(ena.params.stop_z, 4.5);
    assert_eq!(ena.params.max_hold_bars, 120);

    let bnb = cfg.bots.iter().find(|b| b.id == "bnb_xaut").unwrap();
    assert_eq!(bnb.params.rolling_window, 200);
    assert_eq!(bnb.params.entry_z, 3.0);
    assert_eq!(bnb.params.stop_z, 4.75);
    assert_eq!(bnb.params.max_hold_bars, 72);
}
