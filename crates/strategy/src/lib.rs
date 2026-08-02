pub mod params_from_config;
pub mod pullback;
pub mod signal;
pub mod traits;

pub use params_from_config::{ParamError, params_from_f64_config};
pub use pullback::PullbackStrategy;
pub use signal::{MarketContext, Signal};
pub use traits::Strategy;
