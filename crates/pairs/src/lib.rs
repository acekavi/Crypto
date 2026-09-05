pub mod signal;
pub mod sizing;
pub mod spread;

pub use signal::{ExitReason, PairParams, PairSide, SignalEngine, unrealized_pnl_fraction};
pub use sizing::{CapReason, Sizing, SizingError, per_leg_notional};
pub use spread::{RollingZ, Stats, log_spread};
