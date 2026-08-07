pub mod costs;
pub mod fills;
pub mod metrics;
pub mod replay;
pub mod report;
pub mod sim_exchange;

pub use costs::{CostModel, funding_charge, funding_timestamps_in};
pub use fills::{ExitOutcome, FillOutcome, exit_was_ambiguous, limit_fill, resolve_exit};
pub use metrics::{Metrics, compute, equity_curve};
pub use replay::{BacktestConfig, BacktestError, BacktestResult, run_backtest};
pub use report::{RunSummary, summarise};
pub use sim_exchange::{ClosedTrade, ExitReason, SimulatedExchange};
