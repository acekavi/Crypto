use std::collections::HashSet;
use std::path::{Path, PathBuf};

use bot::pairs_config::parse_pairs_config;

fn config_path(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root exists")
        .join("config")
        .join(name)
}

#[test]
fn testnet_pairs_config_matches_the_live_three_bot_portfolio() {
    let src = std::fs::read_to_string(config_path("pairs-testnet.toml")).expect("config exists");
    let cfg = parse_pairs_config(&src).expect("config parses");

    let ids: Vec<_> = cfg.bots.iter().map(|b| b.id.as_str()).collect();
    assert_eq!(ids, vec!["aave_eth", "ena_xrp", "bnb_xaut"]);

    let syms: Vec<(String, String)> = cfg
        .bots
        .iter()
        .map(|b| (b.params.leg_a.to_string(), b.params.leg_b.to_string()))
        .collect();
    assert_eq!(
        syms,
        vec![
            ("AAVEUSDT".into(), "ETHUSDT".into()),
            ("ENAUSDT".into(), "XRPUSDT".into()),
            ("BNBUSDT".into(), "XAUTUSDT".into()),
        ]
    );
}

#[test]
fn duplicate_priorities_are_rejected_at_load() {
    let err = parse_pairs_config(
        r#"
        [runtime]
        loop_seconds = 60
        journal_path = "x.db"
        kline_margin_bars = 25
        [executor]
        ticks_through = 5
        fill_timeout_secs = 20
        poll_interval_secs = 1
        unwind_ladder = [10]
        [[bot]]
        id = "a"
        name = "A"
        priority = 100
        leg_a = "AAUSDT"
        leg_b = "BBUSDT"
        timeframe = "H1"
        rolling_window = 10
        entry_z = 3.0
        stop_z = 4.0
        target_z = 0.0
        max_hold_bars = 10
        fee_per_leg = 0.0002
        per_leg_notional_usdt = 25
        risk_pct_of_equity = 0.03
        max_notional_multiple_of_equity = 1.0
        enable_breakeven = false
        breakeven_r_multiple = 2.0
        [[bot]]
        id = "b"
        name = "B"
        priority = 100
        leg_a = "CCUSDT"
        leg_b = "DDUSDT"
        timeframe = "H1"
        rolling_window = 10
        entry_z = 3.0
        stop_z = 4.0
        target_z = 0.0
        max_hold_bars = 10
        fee_per_leg = 0.0002
        per_leg_notional_usdt = 25
        risk_pct_of_equity = 0.03
        max_notional_multiple_of_equity = 1.0
        enable_breakeven = false
        breakeven_r_multiple = 2.0
        "#,
    )
    .expect_err("duplicate priorities must be refused");
    assert!(format!("{err}").contains("priority"));
}

#[test]
fn a_stop_inside_the_entry_band_is_rejected_at_load() {
    let src = std::fs::read_to_string(config_path("pairs-testnet.toml")).unwrap();
    let broken = src.replace("stop_z = 4.0", "stop_z = 2.0");
    let err = parse_pairs_config(&broken).expect_err("an inverted band must be refused");
    assert!(format!("{err}").contains("stop_z"));
}

#[test]
fn loaded_symbols_do_not_overlap_for_the_live_portfolio() {
    let src = std::fs::read_to_string(config_path("pairs-testnet.toml")).expect("config exists");
    let cfg = parse_pairs_config(&src).expect("config parses");
    let mut all = Vec::new();
    for bot in &cfg.bots {
        all.extend(bot.symbols());
    }
    let unique: HashSet<_> = all.iter().cloned().collect();
    assert_eq!(all.len(), unique.len(), "symbols overlap: {all:?}");
}
