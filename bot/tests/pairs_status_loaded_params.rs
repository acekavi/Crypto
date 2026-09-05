use bot::config::Profile;
use bot::pairs_config::load_pairs_config;
use bot::pairs_report::{loaded_params_view, render_text, BotStatus, LoadedParamsView, RuntimeStateView};

#[test]
fn loaded_params_view_matches_the_live_testnet_config() {
    let cfg = load_pairs_config(Profile::Testnet).expect("config");
    let aave = cfg.bots.iter().find(|b| b.id == "aave_eth").expect("aave");
    let got = loaded_params_view(aave);
    assert_eq!(
        got,
        LoadedParamsView {
            rolling_window: 210,
            entry_z: "3".into(),
            stop_z: "4.5".into(),
            target_z: "0".into(),
            max_hold_bars: 48,
            risk_pct_of_equity: "0.03".into(),
        }
    );
}

#[test]
fn render_text_includes_loaded_params_block() {
    let bot = BotStatus {
        name: "AAVE/ETH".into(),
        bot_id: "aave_eth".into(),
        pair: "AAVEUSDT/ETHUSDT".into(),
        priority: 100,
        risk_pct_of_equity: "0.03".into(),
        latest_bar_ms: Some(123),
        latest_z: Some(1.23),
        current_signal: None,
        signal_error: None,
        runtime_state: RuntimeStateView {
            last_bar_ms: Some(123),
            last_loop_wall_time: Some(1.0),
            last_seen_z: Some(1.23),
            last_seen_signal: None,
            last_guard_reason: None,
            position: None,
            loaded_params: Some(LoadedParamsView {
                rolling_window: 210,
                entry_z: "3".into(),
                stop_z: "4.5".into(),
                target_z: "0".into(),
                max_hold_bars: 48,
                risk_pct_of_equity: "0.03".into(),
            }),
            snapshot_age_s: Some(5.0),
            snapshot_stale: false,
        },
        loaded_params: LoadedParamsView {
            rolling_window: 210,
            entry_z: "3".into(),
            stop_z: "4.5".into(),
            target_z: "0".into(),
            max_hold_bars: 48,
            risk_pct_of_equity: "0.03".into(),
        },
        backtest: None,
    };
    let status = bot::pairs_report::PortfolioStatus {
        generated_at_epoch: 1.0,
        generated_at_human: "now".into(),
        profile: "testnet".into(),
        service_status: bot::pairs_report::ServiceStatus {
            active: true,
            active_raw: "active".into(),
            enabled_raw: "enabled".into(),
            started_at: None,
            main_pid: None,
            fragment_path: None,
        },
        manifest: bot::pairs_report::Manifest {
            pair_count: 1,
            pairs: vec!["AAVE/ETH".into()],
            services: vec!["crypto-pairs.service".into()],
            symbols: vec!["AAVEUSDT".into(), "ETHUSDT".into()],
            has_symbol_overlap: false,
        },
        account_positions: vec![],
        open_orders: vec![],
        bots: vec![bot],
    };
    let text = render_text(&status);
    assert!(text.contains("loaded params:"));
    assert!(text.contains("window=210"));
    assert!(text.contains("entry=3"));
    assert!(text.contains("stop=4.5"));
    assert!(text.contains("hold=48"));
    assert!(text.contains("runtime params:"));
}
