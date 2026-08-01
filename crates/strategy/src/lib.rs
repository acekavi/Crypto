pub mod pullback;
pub mod signal;
pub mod traits;

// pub use pullback::PullbackStrategy;  // Restored in Task 2
pub use signal::{MarketContext, Signal};
pub use traits::Strategy;
