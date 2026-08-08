pub mod ict;
pub mod ict_params_from_config;
pub mod params_from_config;
pub mod pullback;
pub mod reversion;
pub mod signal;
pub mod traits;

pub use ict::{Funnel, IctParams, IctStrategy};
pub use ict_params_from_config::ict_params_from_config;
pub use params_from_config::{ParamError, params_from_f64_config};
pub use pullback::PullbackStrategy;
pub use reversion::{ReversionParams, ReversionStrategy};
pub use signal::{MarketContext, Signal};
pub use traits::Strategy;
