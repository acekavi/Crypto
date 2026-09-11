use std::collections::HashSet;
use std::path::Path;
use std::str::FromStr;

use crate::config::Profile;
use botcore::{Symbol, Timeframe};
use pairs::{ExecutorConfig, PairParams, RiskGuardConfig};
use rust_decimal::Decimal;
use serde::Deserialize;
use tracing::warn;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeSettings {
    pub loop_seconds: u64,
    pub journal_path: String,
    pub kline_margin_bars: u16,
}

#[derive(Debug, Clone, PartialEq)]
pub struct BotConfig {
    pub id: String,
    pub name: String,
    pub priority: u32,
    pub params: PairParams,
}

impl BotConfig {
    pub fn symbols(&self) -> HashSet<Symbol> {
        [self.params.leg_a.clone(), self.params.leg_b.clone()]
            .into_iter()
            .collect()
    }
}

#[derive(Debug, Clone)]
pub struct PairsConfig {
    pub runtime: RuntimeSettings,
    pub executor: ExecutorConfig,
    pub risk: RiskGuardConfig,
    pub bots: Vec<BotConfig>,
}

#[derive(Debug, thiserror::Error)]
pub enum PairsConfigError {
    #[error("could not read config file {path}: {source}")]
    Io { path: String, source: std::io::Error },
    #[error("invalid config: {0}")]
    Parse(#[from] toml::de::Error),
    #[error("invalid config: {0}")]
    Invalid(String),
}

#[derive(Debug, Deserialize)]
struct RawConfig {
    runtime: RawRuntime,
    executor: RawExecutor,
    /// Optional so existing config files without a `[risk]` section keep
    /// loading, with the circuit breaker effectively disabled (thresholds
    /// high enough that ordinary drawdown never reaches them).
    #[serde(default)]
    risk: RawRisk,
    #[serde(rename = "bot")]
    bots: Vec<RawBot>,
}

#[derive(Debug, Deserialize)]
struct RawRisk {
    #[serde(default = "default_daily_loss_halt_pct")]
    daily_loss_halt_pct: f64,
    #[serde(default = "default_total_loss_halt_pct")]
    total_loss_halt_pct: f64,
}

impl Default for RawRisk {
    fn default() -> Self {
        RawRisk {
            daily_loss_halt_pct: default_daily_loss_halt_pct(),
            total_loss_halt_pct: default_total_loss_halt_pct(),
        }
    }
}

// 100% is not reachable by a drawdown percentage, so an omitted `[risk]`
// section is a no-op circuit breaker rather than a silent behavior change
// for any config file written before this existed.
fn default_daily_loss_halt_pct() -> f64 {
    100.0
}
fn default_total_loss_halt_pct() -> f64 {
    100.0
}

#[derive(Debug, Deserialize)]
struct RawRuntime {
    loop_seconds: u64,
    journal_path: String,
    kline_margin_bars: u16,
}

#[derive(Debug, Deserialize)]
struct RawExecutor {
    ticks_through: u64,
    fill_timeout_secs: u64,
    poll_interval_secs: u64,
    unwind_ladder: Vec<u64>,
    /// Optional so existing config files without this key keep loading and
    /// keep today's behavior. See `ExecutorConfig::unwind_on_partial_fill`.
    #[serde(default = "default_unwind_on_partial_fill")]
    unwind_on_partial_fill: bool,
}

fn default_unwind_on_partial_fill() -> bool {
    true
}

#[derive(Debug, Deserialize)]
struct RawBot {
    id: String,
    name: String,
    priority: u32,
    leg_a: String,
    leg_b: String,
    timeframe: String,
    rolling_window: usize,
    entry_z: f64,
    stop_z: f64,
    target_z: f64,
    max_hold_bars: i64,
    fee_per_leg: f64,
    per_leg_notional_usdt: u64,
    risk_pct_of_equity: f64,
    max_notional_multiple_of_equity: f64,
    enable_breakeven: bool,
    breakeven_r_multiple: f64,
}

fn dec_from_f64(value: f64, field: &str) -> Result<Decimal, PairsConfigError> {
    if !value.is_finite() {
        return Err(PairsConfigError::Invalid(format!("{field} must be finite")));
    }
    Decimal::from_str(&value.to_string())
        .map_err(|e| PairsConfigError::Invalid(format!("{field}: {e}")))
}

pub fn parse_pairs_config(src: &str) -> Result<PairsConfig, PairsConfigError> {
    let raw: RawConfig = toml::from_str(src)?;
    if raw.bots.is_empty() {
        return Err(PairsConfigError::Invalid("at least one bot is required".into()));
    }
    if raw.executor.unwind_ladder.is_empty() {
        return Err(PairsConfigError::Invalid("unwind_ladder must be non-empty".into()));
    }
    for pair in raw.executor.unwind_ladder.windows(2) {
        if pair[1] <= pair[0] {
            return Err(PairsConfigError::Invalid("unwind_ladder must be strictly increasing".into()));
        }
    }
    if !(raw.risk.daily_loss_halt_pct > 0.0 && raw.risk.daily_loss_halt_pct <= 100.0) {
        return Err(PairsConfigError::Invalid("daily_loss_halt_pct must be in (0, 100]".into()));
    }
    if !(raw.risk.total_loss_halt_pct > 0.0 && raw.risk.total_loss_halt_pct <= 100.0) {
        return Err(PairsConfigError::Invalid("total_loss_halt_pct must be in (0, 100]".into()));
    }
    if raw.risk.total_loss_halt_pct < raw.risk.daily_loss_halt_pct {
        return Err(PairsConfigError::Invalid(
            "total_loss_halt_pct must be >= daily_loss_halt_pct, or the total halt could never fire before the daily one already had".into(),
        ));
    }

    let mut ids = HashSet::new();
    let mut priorities = HashSet::new();
    let mut seen_symbols: HashSet<Symbol> = HashSet::new();
    let mut overlap = false;
    let mut bots = Vec::new();

    for raw_bot in raw.bots {
        if !ids.insert(raw_bot.id.clone()) {
            return Err(PairsConfigError::Invalid(format!("duplicate bot id {}", raw_bot.id)));
        }
        if !priorities.insert(raw_bot.priority) {
            return Err(PairsConfigError::Invalid(format!("duplicate priority {}", raw_bot.priority)));
        }
        if raw_bot.rolling_window < 2 {
            return Err(PairsConfigError::Invalid(format!("{} rolling_window must be >= 2", raw_bot.id)));
        }
        if !raw_bot.entry_z.is_finite() || raw_bot.entry_z <= 0.0 {
            return Err(PairsConfigError::Invalid(format!("{} entry_z must be > 0 and finite", raw_bot.id)));
        }
        if !raw_bot.stop_z.is_finite() || raw_bot.stop_z <= raw_bot.entry_z {
            return Err(PairsConfigError::Invalid(format!("{} stop_z must be > entry_z", raw_bot.id)));
        }
        if !raw_bot.target_z.is_finite() {
            return Err(PairsConfigError::Invalid(format!("{} target_z must be finite", raw_bot.id)));
        }
        if raw_bot.max_hold_bars < 1 {
            return Err(PairsConfigError::Invalid(format!("{} max_hold_bars must be >= 1", raw_bot.id)));
        }
        if !raw_bot.risk_pct_of_equity.is_finite()
            || raw_bot.risk_pct_of_equity <= 0.0
            || raw_bot.risk_pct_of_equity > 0.10
        {
            return Err(PairsConfigError::Invalid(format!("{} risk_pct_of_equity must be > 0 and <= 0.10", raw_bot.id)));
        }
        if !raw_bot.max_notional_multiple_of_equity.is_finite()
            || raw_bot.max_notional_multiple_of_equity <= 0.0
        {
            return Err(PairsConfigError::Invalid(format!("{} max_notional_multiple_of_equity must be > 0", raw_bot.id)));
        }
        if raw_bot.leg_a == raw_bot.leg_b {
            return Err(PairsConfigError::Invalid(format!("{} leg_a and leg_b must differ", raw_bot.id)));
        }

        let timeframe = Timeframe::from_bybit_interval(match raw_bot.timeframe.as_str() {
            "H1" => "60",
            "H4" => "240",
            "M15" => "15",
            "M5" => "5",
            "D1" => "D",
            other => other,
        })
        .ok_or_else(|| PairsConfigError::Invalid(format!("{} timeframe {} is invalid", raw_bot.id, raw_bot.timeframe)))?;

        let bot = BotConfig {
            id: raw_bot.id,
            name: raw_bot.name,
            priority: raw_bot.priority,
            params: PairParams {
                leg_a: Symbol::new(raw_bot.leg_a),
                leg_b: Symbol::new(raw_bot.leg_b),
                timeframe,
                rolling_window: raw_bot.rolling_window,
                entry_z: raw_bot.entry_z,
                stop_z: raw_bot.stop_z,
                target_z: raw_bot.target_z,
                max_hold_bars: raw_bot.max_hold_bars,
                fee_per_leg: dec_from_f64(raw_bot.fee_per_leg, "fee_per_leg")?,
                per_leg_notional_usdt: Decimal::from(raw_bot.per_leg_notional_usdt),
                risk_pct_of_equity: dec_from_f64(raw_bot.risk_pct_of_equity, "risk_pct_of_equity")?,
                max_notional_multiple_of_equity: dec_from_f64(raw_bot.max_notional_multiple_of_equity, "max_notional_multiple_of_equity")?,
                enable_breakeven: raw_bot.enable_breakeven,
                breakeven_r_multiple: dec_from_f64(raw_bot.breakeven_r_multiple, "breakeven_r_multiple")?,
            },
        };
        for sym in bot.symbols() {
            if !seen_symbols.insert(sym) {
                overlap = true;
            }
        }
        bots.push(bot);
    }

    if overlap {
        warn!("pairs config contains shared symbols across bots; PortfolioGuard will arbitrate them");
    }

    Ok(PairsConfig {
        runtime: RuntimeSettings {
            loop_seconds: raw.runtime.loop_seconds,
            journal_path: raw.runtime.journal_path,
            kline_margin_bars: raw.runtime.kline_margin_bars,
        },
        executor: ExecutorConfig {
            ticks_through: Decimal::from(raw.executor.ticks_through),
            fill_timeout: std::time::Duration::from_secs(raw.executor.fill_timeout_secs),
            poll_interval: std::time::Duration::from_secs(raw.executor.poll_interval_secs),
            unwind_ladder: raw.executor.unwind_ladder.into_iter().map(Decimal::from).collect(),
            unwind_on_partial_fill: raw.executor.unwind_on_partial_fill,
        },
        risk: RiskGuardConfig {
            daily_loss_halt_pct: dec_from_f64(raw.risk.daily_loss_halt_pct, "daily_loss_halt_pct")?,
            total_loss_halt_pct: dec_from_f64(raw.risk.total_loss_halt_pct, "total_loss_halt_pct")?,
        },
        bots,
    })
}

pub fn load_pairs_config(profile: Profile) -> Result<PairsConfig, PairsConfigError> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("workspace root exists")
        .join("config")
        .join(format!("pairs-{}.toml", profile.name()));
    let text = std::fs::read_to_string(&path).map_err(|source| PairsConfigError::Io {
        path: path.display().to_string(),
        source,
    })?;
    parse_pairs_config(&text)
}
