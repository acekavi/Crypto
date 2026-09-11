//! `pairs_clear_halt`: read (and optionally clear) the portfolio's
//! `halt_state` — the sticky flag `crates/pairs::supervisor`'s total-drawdown
//! circuit breaker sets, and the only thing standing between it and new
//! entries resuming automatically.
//!
//! Deliberately a separate binary from `pairs_status`, which is documented as
//! read-only tooling — this one mutates, and keeping that out of the
//! read-oriented tool is the point.
use bot::config::Profile;
use bot::envfile::load_env_file;
use bot::pairs_config::load_pairs_config;
use persistence::Journal;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let profile = Profile::from_name(&args.next().unwrap_or_else(|| "testnet".into()))?;
    let clear = args.any(|a| a == "--clear");
    load_env_file("/home/acekavi/Projects/Crypto/.env")?;
    let cfg = load_pairs_config(profile)?;
    let journal = Journal::open_local(&cfg.runtime.journal_path).await?;

    match journal.halt_reason().await? {
        Some(reason) => {
            println!("HALTED: {reason}");
            if clear {
                journal.clear_halt().await?;
                println!("cleared — trading resumes on the next loop tick for any bot currently flat");
            } else {
                println!("re-run with --clear to resume trading");
            }
        }
        None => println!("not halted"),
    }
    Ok(())
}
