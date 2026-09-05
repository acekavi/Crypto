use bot::config::Profile;
use bot::envfile::load_env_file;
use bot::pairs_backtest::{run_backtest, split_summary};
use bot::pairs_config::load_pairs_config;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let profile = Profile::from_name(&args.next().unwrap_or_else(|| "testnet".into()))?;
    load_env_file("/home/acekavi/Projects/Crypto/.env")?;
    let mut bot_id = None::<String>;
    let mut json = false;
    let mut split_pct = 0.70f64;
    let mut db_path = "/home/acekavi/Projects/Crypto/data/history.db".to_string();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--bot-id" => bot_id = args.next(),
            "--json" => json = true,
            "--split-pct" => split_pct = args.next().unwrap_or_else(|| "0.70".into()).parse()?,
            "--db" => db_path = args.next().unwrap_or(db_path),
            other => return Err(format!("unknown arg {other}").into()),
        }
    }
    let cfg = load_pairs_config(profile)?;
    let bot = bot_id
        .as_deref()
        .map(|id| cfg.bots.iter().find(|b| b.id == id))
        .unwrap_or_else(|| cfg.bots.first())
        .ok_or("no bots configured")?;
    let summary = split_summary(&db_path, &bot.params, split_pct).await?;
    if json {
        println!("{}", serde_json::to_string_pretty(&summary)?);
    } else {
        let full = run_backtest(&db_path, &bot.params, None, None).await?;
        println!("pair={} trades={} win_rate={:.2}% pf={:.3} net={:.4} dd={:.2}%", full.pair, full.trades, full.win_rate * 100.0, full.profit_factor, full.net, full.max_drawdown_pct);
    }
    Ok(())
}
