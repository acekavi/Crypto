use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use bot::config::Profile;
use bot::envfile::load_env_file;
use bot::pairs_report::collect_status;
use serde::{Deserialize, Serialize};

const STALL_THRESHOLD_S: f64 = 10.0 * 60.0;

#[derive(Debug, Default, Serialize, Deserialize)]
struct WatchState {
    last_signal_bar_ms: Option<i64>,
    position_opened_at_ms: Option<i64>,
    stall_alert_active: bool,
    service_alert_active: bool,
    last_seen_bar_ms: Option<i64>,
    last_seen_z: Option<f64>,
    last_seen_signal: Option<String>,
    last_watchdog_wall_time: Option<f64>,
}

fn now_epoch() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs_f64()
}

fn load_state(path: &PathBuf) -> WatchState {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_state(path: &PathBuf, state: &WatchState) -> Result<(), std::io::Error> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(state)?)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let profile = Profile::from_name(&args.next().unwrap_or_else(|| "testnet".into()))?;
    load_env_file("/home/acekavi/Projects/Crypto/.env")?;
    let mut bot_id = None::<String>;
    let mut state_path = None::<PathBuf>;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--bot-id" => bot_id = args.next(),
            "--state" => state_path = args.next().map(PathBuf::from),
            other => return Err(format!("unknown arg {other}").into()),
        }
    }
    let status = collect_status(profile, false).await?;
    let bot = bot_id
        .as_deref()
        .and_then(|id| status.bots.iter().find(|b| b.bot_id == id))
        .or_else(|| status.bots.first())
        .ok_or("no bots in status")?;
    let state_path = state_path.unwrap_or_else(|| PathBuf::from(format!("/home/acekavi/Projects/Crypto/data/{}_watchdog_state.json", bot.bot_id)));
    let mut prior = load_state(&state_path);
    let mut alerts = Vec::new();
    let now = now_epoch();

    if let (Some(sig), Some(bar)) = (&bot.current_signal, bot.latest_bar_ms)
        && prior.last_signal_bar_ms != Some(bar)
    {
        let z = bot.latest_z.unwrap_or(0.0);
        alerts.push(format!("{} bot signal: {} at bar {} with z={:.4}.", bot.name, sig, bar, z));
        prior.last_signal_bar_ms = Some(bar);
    }

    let current_pos_open = bot.runtime_state.position.as_ref().map(|p| p.opened_at_ms);
    if let Some(pos) = &bot.runtime_state.position {
        if prior.position_opened_at_ms != Some(pos.opened_at_ms) {
            alerts.push(format!("{} bot position OPEN: {}, opened_at_ms={}, entry_z={}", bot.name, pos.side, pos.opened_at_ms, pos.entry_z));
            prior.position_opened_at_ms = Some(pos.opened_at_ms);
        }
    } else if prior.position_opened_at_ms.is_some() {
        alerts.push(format!("{} bot position CLOSED.", bot.name));
        prior.position_opened_at_ms = None;
    }

    let heartbeat = bot.runtime_state.last_loop_wall_time;
    let stale_now = heartbeat.is_none_or(|ts| (now - ts) > STALL_THRESHOLD_S);
    if stale_now && !prior.stall_alert_active {
        let age = heartbeat.map(|ts| (now - ts) as i64);
        alerts.push(format!("{} bot WARNING: stalled heartbeat. last_loop_age_s={:?}.", bot.name, age));
        prior.stall_alert_active = true;
    } else if !stale_now && prior.stall_alert_active {
        alerts.push(format!("{} bot RECOVERED: heartbeat advancing again.", bot.name));
        prior.stall_alert_active = false;
    }

    let service_down = status.service_status.active_raw != "active";
    if service_down && !prior.service_alert_active {
        alerts.push(format!("{} service WARNING: crypto-pairs.service is {}.", bot.name, status.service_status.active_raw));
        prior.service_alert_active = true;
    } else if !service_down && prior.service_alert_active {
        alerts.push(format!("{} service RECOVERED: crypto-pairs.service is active again.", bot.name));
        prior.service_alert_active = false;
    }

    prior.last_seen_bar_ms = bot.latest_bar_ms;
    prior.last_seen_z = bot.latest_z;
    prior.last_seen_signal = bot.current_signal.clone();
    prior.last_watchdog_wall_time = Some(now);
    let _ = current_pos_open;
    save_state(&state_path, &prior)?;
    if !alerts.is_empty() {
        println!("{}", alerts.join("\n\n"));
    }
    Ok(())
}
