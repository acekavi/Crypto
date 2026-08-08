//! The config files and the frozen constructor must not drift.
//!
//! They already had: `config/*.toml` drove `PullbackStrategy` at 1:2 while the
//! measured strategy was ICT. Nothing caught it, because nothing compared the
//! shipped configuration against the strategy that was actually measured.

use std::path::{Path, PathBuf};

use bot::config::Config;
use strategy::{IctParams, ict_params_from_config};

/// `CARGO_MANIFEST_DIR` for the `bot` crate is the workspace root's `bot/`
/// subdirectory, one level down from the workspace root itself — unlike
/// `Config::load`'s own `config/{profile}.toml`, which is only correct
/// relative to a process launched from the workspace root (as `cargo run`
/// is), not relative to `cargo test`'s per-package working directory.
fn load(profile: &str) -> Config {
    let path: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root exists")
        .join("config")
        .join(format!("{profile}.toml"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{path:?} reads: {e}"));
    Config::from_toml_str(&text).unwrap_or_else(|e| panic!("{path:?} parses: {e}"))
}

/// Exactly the call `bot/src/main.rs` makes, so this test fails if the config
/// stops producing the frozen parameters for any reason main.rs would hit.
fn params_from(cfg: &Config) -> IctParams {
    ict_params_from_config(
        cfg.strategy.bias_ema,
        cfg.strategy.swing_lookback,
        cfg.strategy.atr_period,
        cfg.strategy.ob_lookback,
        cfg.strategy.fvg_entry_fraction,
        cfg.strategy.stop_buffer_atr,
        cfg.strategy.stop_widen_multiple,
        cfg.strategy.reward_multiple,
        cfg.strategy.breakeven_at_r,
        cfg.strategy.use_pdh_pdl,
        cfg.strategy.use_session_levels,
        cfg.strategy.use_order_block,
        cfg.strategy.require_mss,
        cfg.strategy.session_filter,
        cfg.strategy.allow_long,
        cfg.strategy.allow_short,
    )
    .expect("config produces valid params")
}

/// The 8 symbols the strategy was measured on, sorted.
const MEASURED_SYMBOLS: [&str; 8] = [
    "ADAUSDT", "BNBUSDT", "BTCUSDT", "DOGEUSDT", "ETHUSDT", "LINKUSDT", "SOLUSDT", "XRPUSDT",
];

#[test]
fn testnet_config_reproduces_the_frozen_strategy() {
    assert_eq!(
        params_from(&load("testnet")),
        IctParams::liquidity_sweep_v2()
    );
}

#[test]
fn testnet_risk_envelope_matches_what_was_measured() {
    let cfg = load("testnet");
    assert_eq!(cfg.risk.risk_pct, 0.01);
    assert_eq!(cfg.risk.max_concurrent_positions, 8);
    assert_eq!(cfg.risk.max_daily_entries, 5, "owner's 3-5 trades/day rule");
    assert_eq!(cfg.risk.total_drawdown_halt_pct, 0.20);
    assert_eq!(cfg.risk.daily_drawdown_halt_pct, 0.05);
}

#[test]
fn the_live_universe_is_pinned_to_the_measured_symbols() {
    // 8 concurrent positions across a 20-symbol universe was never measured,
    // and 20 correlated positions at 1% each is 20% at risk at once.
    let cfg = load("testnet");
    let mut symbols = cfg.universe.symbols.expect("universe must be pinned");
    symbols.sort();
    assert_eq!(symbols, MEASURED_SYMBOLS);
}

#[test]
fn the_entry_expiry_is_the_measured_twelve_execution_candles() {
    assert_eq!(load("testnet").strategy.entry_expiry_candles, 12);
}

#[test]
fn mainnet_config_reproduces_the_frozen_strategy_too() {
    let cfg = load("mainnet");
    assert_eq!(params_from(&cfg), IctParams::liquidity_sweep_v2());
    assert_eq!(cfg.risk.max_daily_entries, 5);
    assert_eq!(cfg.risk.total_drawdown_halt_pct, 0.20);

    let mut symbols = cfg
        .universe
        .symbols
        .expect("mainnet universe must be pinned too");
    symbols.sort();
    assert_eq!(symbols, MEASURED_SYMBOLS);
}
