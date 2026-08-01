use bot::config::{Config, ConfigError, Profile};

#[test]
fn testnet_profile_points_at_testnet_hosts() {
    assert_eq!(Profile::Testnet.rest_base_url(), "https://api-testnet.bybit.com");
    assert_eq!(
        Profile::Testnet.ws_public_url(),
        "wss://stream-testnet.bybit.com/v5/public/linear"
    );
    assert_eq!(Profile::Testnet.ws_private_url(), "wss://stream-testnet.bybit.com/v5/private");
}

#[test]
fn mainnet_profile_points_at_mainnet_hosts() {
    assert_eq!(Profile::Mainnet.rest_base_url(), "https://api.bybit.com");
    assert_eq!(Profile::Mainnet.ws_public_url(), "wss://stream.bybit.com/v5/public/linear");
}

#[test]
fn mainnet_requires_an_explicit_confirmation_variable() {
    // Safety gate: selecting mainnet must take two independent actions, so no
    // single mistake can route orders at real money.
    temp_env::with_var_unset("BYBIT_ALLOW_MAINNET", || {
        let err = Profile::from_name("mainnet").expect_err("must refuse without confirmation");
        assert!(matches!(err, ConfigError::MainnetNotConfirmed));
    });
}

#[test]
fn mainnet_is_allowed_once_confirmed() {
    temp_env::with_var("BYBIT_ALLOW_MAINNET", Some("yes"), || {
        assert_eq!(Profile::from_name("mainnet").expect("confirmed"), Profile::Mainnet);
    });
}

#[test]
fn testnet_never_requires_confirmation() {
    temp_env::with_var_unset("BYBIT_ALLOW_MAINNET", || {
        assert_eq!(Profile::from_name("testnet").expect("testnet is always allowed"), Profile::Testnet);
    });
}

#[test]
fn config_hash_is_stable_and_parameter_sensitive() {
    let a = Config::from_toml_str(SAMPLE).expect("parses");
    let b = Config::from_toml_str(SAMPLE).expect("parses");
    assert_eq!(a.hash(), b.hash(), "same config must hash identically");

    let changed = Config::from_toml_str(&SAMPLE.replace("risk_pct = 0.01", "risk_pct = 0.02"))
        .expect("parses");
    assert_ne!(a.hash(), changed.hash(), "changing a rule must change the hash");
    assert_eq!(a.hash().len(), 64, "hash is a SHA-256 hex digest");
}

const SAMPLE: &str = r#"
[risk]
risk_pct = 0.01
max_concurrent_positions = 4
max_daily_entries = 5
daily_drawdown_halt_pct = 0.05
total_drawdown_halt_pct = 0.15
liq_buffer_multiple = 3.0
leverage = 5

[strategy]
ema_fast = 50
ema_slow = 200
ema_entry = 20
rsi_period = 14
rsi_long_trigger = 40
rsi_short_trigger = 60
atr_period = 14
atr_band_min_pct = 0.003
atr_band_max_pct = 0.05
swing_lookback = 10
atr_stop_multiple = 1.5
reward_multiple = 2.0
entry_expiry_candles = 3
stop_limit_offset_atr = 0.3
max_stop_escalations = 3
stop_fill_timeout_secs = 30

[universe]
size = 20
min_turnover_24h = 50000000
min_listing_age_days = 30
"#;
