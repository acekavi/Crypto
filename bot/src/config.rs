use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("unknown profile {0}; expected 'testnet' or 'mainnet'")]
    UnknownProfile(String),

    #[error(
        "mainnet profile requires BYBIT_ALLOW_MAINNET to be set. \
         This is a deliberate second gate so real money is never reached by accident."
    )]
    MainnetNotConfirmed,

    #[error("could not read config file {path}: {source}")]
    Io { path: String, source: std::io::Error },

    #[error("invalid config: {0}")]
    Parse(#[from] toml::de::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Testnet,
    Mainnet,
}

impl Profile {
    /// Parse a profile name. Mainnet additionally requires the
    /// `BYBIT_ALLOW_MAINNET` environment variable to be present.
    pub fn from_name(name: &str) -> Result<Self, ConfigError> {
        match name {
            "testnet" => Ok(Profile::Testnet),
            "mainnet" => {
                if std::env::var("BYBIT_ALLOW_MAINNET").is_ok() {
                    Ok(Profile::Mainnet)
                } else {
                    Err(ConfigError::MainnetNotConfirmed)
                }
            }
            other => Err(ConfigError::UnknownProfile(other.to_string())),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Profile::Testnet => "testnet",
            Profile::Mainnet => "mainnet",
        }
    }

    pub fn rest_base_url(self) -> &'static str {
        match self {
            Profile::Testnet => "https://api-testnet.bybit.com",
            Profile::Mainnet => "https://api.bybit.com",
        }
    }

    pub fn ws_public_url(self) -> &'static str {
        match self {
            Profile::Testnet => "wss://stream-testnet.bybit.com/v5/public/linear",
            Profile::Mainnet => "wss://stream.bybit.com/v5/public/linear",
        }
    }

    pub fn ws_private_url(self) -> &'static str {
        match self {
            Profile::Testnet => "wss://stream-testnet.bybit.com/v5/private",
            Profile::Mainnet => "wss://stream.bybit.com/v5/private",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RiskConfig {
    pub risk_pct: f64,
    pub max_concurrent_positions: u32,
    pub max_daily_entries: u32,
    pub daily_drawdown_halt_pct: f64,
    pub total_drawdown_halt_pct: f64,
    pub liq_buffer_multiple: f64,
    pub leverage: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct StrategyConfig {
    pub ema_fast: usize,
    pub ema_slow: usize,
    pub ema_entry: usize,
    pub rsi_period: usize,
    pub rsi_long_trigger: f64,
    pub rsi_short_trigger: f64,
    pub atr_period: usize,
    pub atr_band_min_pct: f64,
    pub atr_band_max_pct: f64,
    pub swing_lookback: usize,
    pub atr_stop_multiple: f64,
    pub reward_multiple: f64,
    pub entry_expiry_candles: u32,
    pub stop_limit_offset_atr: f64,
    pub max_stop_escalations: u32,
    pub stop_fill_timeout_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct UniverseConfig {
    pub size: usize,
    pub min_turnover_24h: u64,
    pub min_listing_age_days: i64,
}

/// The complete rule set. Immutable for the process lifetime — changing a
/// parameter requires an edit and a restart, which produces a new hash and a
/// visible discontinuity in the journal.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Config {
    pub risk: RiskConfig,
    pub strategy: StrategyConfig,
    pub universe: UniverseConfig,
}

impl Config {
    pub fn load(profile: Profile) -> Result<Self, ConfigError> {
        let path = format!("config/{}.toml", profile.name());
        let text = std::fs::read_to_string(&path)
            .map_err(|source| ConfigError::Io { path: path.clone(), source })?;
        Self::from_toml_str(&text)
    }

    pub fn from_toml_str(s: &str) -> Result<Self, ConfigError> {
        Ok(toml::from_str(s)?)
    }

    /// SHA-256 over a canonical serialisation. Written to every order row so
    /// each trade is attributable to an exact ruleset.
    pub fn hash(&self) -> String {
        let canonical = toml::to_string(self).expect("config always serialises");
        let mut hasher = Sha256::new();
        hasher.update(canonical.as_bytes());
        hex::encode(hasher.finalize())
    }
}
