pub mod executor;
pub mod pricing;
pub mod settle;
pub mod signal;
pub mod sizing;
pub mod spread;
pub mod state;
pub mod supervisor;

pub use executor::{ExecutionError, ExecutorConfig, LegQuote, OpenedPair, open_pair, open_position_size, poll_to_terminal, unwind_legs};
pub use pricing::{aggressive_limit_price, ceil_step, floor_step, sized_qty};
pub use settle::{Leg, LegReport, Settlement, settle};
pub use signal::{ExitReason, PairParams, PairSide, SignalEngine, unrealized_pnl_fraction};
pub use sizing::{CapReason, Sizing, SizingError, per_leg_notional};
pub use spread::{RollingZ, Stats, log_spread};
pub use state::{Reconciliation, reconcile_pair};
pub use supervisor::{
    BarOutcome, BotSnapshot, PairContext, PortfolioGuard, PreparedBar, RiskGuardConfig,
    evaluate_bar, evaluate_prepared_bar, prepare_bar, prepare_bar_with_state, run_pair,
};
