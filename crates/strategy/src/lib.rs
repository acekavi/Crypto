pub mod params_from_config;
pub mod pullback;
pub mod signal;
pub mod traits;

pub use pullback::PullbackStrategy;
pub use params_from_config::{params_from_f64_config, ParamError};
pub use signal::{MarketContext, Signal};
pub use traits::Strategy;
