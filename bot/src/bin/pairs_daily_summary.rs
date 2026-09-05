use bot::config::Profile;
use bot::envfile::load_env_file;
use bot::pairs_report::collect_status;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let profile = Profile::from_name(&args.next().unwrap_or_else(|| "testnet".into()))?;
    load_env_file("/home/acekavi/Projects/Crypto/.env")?;
    let mut bot_id = None::<String>;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--bot-id" => bot_id = args.next(),
            other => return Err(format!("unknown arg {other}").into()),
        }
    }
    let status = collect_status(profile, false).await?;
    let bot = bot_id
        .as_deref()
        .and_then(|id| status.bots.iter().find(|b| b.bot_id == id))
        .or_else(|| status.bots.first())
        .ok_or("no bots in status")?;
    let summary = serde_json::json!({
        "pair": bot.name,
        "last_bar_ms": bot.latest_bar_ms,
        "latest_z": bot.latest_z.map(|z| (z * 10000.0).round() / 10000.0),
        "current_signal": bot.current_signal,
        "state_position": bot.runtime_state.position,
        "exchange_open_positions": status.account_positions.len(),
        "service": status.service_status.active_raw,
    });
    println!("{} bot daily summary\n{}", bot.name, serde_json::to_string_pretty(&summary)?);
    Ok(())
}
