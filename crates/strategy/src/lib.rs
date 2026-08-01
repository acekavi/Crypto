pub mod pullback;
pub mod signal;
pub mod traits;

pub use pullback::PullbackStrategy;
pub use signal::{MarketContext, Signal};
pub use traits::Strategy;
