pub mod executor;
pub mod pricing;
pub mod settle;
pub mod signal;
pub mod sizing;
pub mod spread;

pub use executor::{ExecutionError, ExecutorConfig, LegQuote, OpenedPair, open_pair};
pub use pricing::{aggressive_limit_price, ceil_step, floor_step, sized_qty};
pub use settle::{Leg, LegReport, Settlement, settle};
pub use signal::{ExitReason, PairParams, PairSide, SignalEngine, unrealized_pnl_fraction};
pub use sizing::{CapReason, Sizing, SizingError, per_leg_notional};
pub use spread::{RollingZ, Stats, log_spread};
