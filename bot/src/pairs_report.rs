use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::config::Profile;
use crate::pairs_backtest::{SplitBacktestSummary, split_summary};
use crate::pairs_config::{BotConfig, load_pairs_config};
use botcore::{OpenOrder, Position};
use exchange::ExchangeClient;
use exchange::bybit::rest::BybitRest;
use exchange::bybit::sign::Credentials;
use pairs::{RollingZ, SignalEngine};
use serde::{Deserialize, Serialize};

const HISTORY_DB: &str = "/home/acekavi/Projects/Crypto/data/history.db";
const DASHBOARD_HTML: &str = "/home/acekavi/Projects/Crypto/dashboard/pairs-dashboard.html";
const RUNTIME_SERVICE: &str = "crypto-pairs.service";

#[derive(Debug, thiserror::Error)]
pub enum ReportError {
    #[error(transparent)]
    Config(#[from] crate::pairs_config::PairsConfigError),
    #[error(transparent)]
    Sign(#[from] exchange::bybit::sign::SignError),
    #[error(transparent)]
    Exchange(#[from] exchange::bybit::transport::ExchangeError),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Backtest(#[from] crate::pairs_backtest::PairBacktestError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("report error: {0}")]
    Data(String),
}

#[derive(Debug, Clone, Serialize)]
pub struct Manifest {
    pub pair_count: usize,
    pub pairs: Vec<String>,
    pub services: Vec<String>,
    pub symbols: Vec<String>,
    pub has_symbol_overlap: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct ServiceStatus {
    pub active: bool,
    pub active_raw: String,
    pub enabled_raw: String,
    pub started_at: Option<String>,
    pub main_pid: Option<String>,
    pub fragment_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimePositionView {
    pub side: String,
    pub opened_at_ms: i64,
    pub entry_z: String,
    pub per_leg_notional_usdt: String,
    pub capped_by: Option<String>,
    pub breakeven_armed: bool,
}

pub const SNAPSHOT_STALE_AFTER_S: f64 = 600.0;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeStateView {
    pub last_bar_ms: Option<i64>,
    pub last_loop_wall_time: Option<f64>,
    pub last_seen_z: Option<f64>,
    pub last_seen_signal: Option<String>,
    pub last_guard_reason: Option<String>,
    pub position: Option<RuntimePositionView>,
    #[serde(default)]
    pub loaded_params: Option<LoadedParamsView>,
    #[serde(default)]
    pub snapshot_age_s: Option<f64>,
    #[serde(default)]
    pub snapshot_stale: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct PositionView {
    pub symbol: String,
    pub side: String,
    pub size: String,
    pub entry_price: String,
    pub unrealized_pnl: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct OrderView {
    pub symbol: String,
    pub side: String,
    pub qty: String,
    pub price: String,
    pub state: String,
    pub order_link_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct BotStatus {
    pub name: String,
    pub bot_id: String,
    pub pair: String,
    pub priority: u32,
    pub risk_pct_of_equity: String,
    pub loaded_params: LoadedParamsView,
    pub latest_bar_ms: Option<i64>,
    pub latest_z: Option<f64>,
    pub current_signal: Option<String>,
    pub signal_error: Option<String>,
    pub runtime_state: RuntimeStateView,
    pub backtest: Option<SplitBacktestSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LoadedParamsView {
    pub rolling_window: usize,
    pub entry_z: String,
    pub stop_z: String,
    pub target_z: String,
    pub max_hold_bars: i64,
    pub risk_pct_of_equity: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct PortfolioStatus {
    pub generated_at_epoch: f64,
    pub generated_at_human: String,
    pub profile: String,
    pub service_status: ServiceStatus,
    pub manifest: Manifest,
    pub account_positions: Vec<PositionView>,
    pub open_orders: Vec<OrderView>,
    pub bots: Vec<BotStatus>,
}

fn now_epoch() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

fn shell_line(cmd: &mut Command) -> String {
    match cmd.output() {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
            if !stdout.is_empty() { stdout } else { stderr }
        }
        Err(e) => format!("command failed: {e}"),
    }
}

pub fn service_snapshot(service: &str) -> ServiceStatus {
    let active_raw = shell_line(Command::new("systemctl").args(["--user", "is-active", service]));
    let enabled_raw = shell_line(Command::new("systemctl").args(["--user", "is-enabled", service]));
    let detail = shell_line(Command::new("systemctl").args([
        "--user",
        "show",
        service,
        "--property=ActiveEnterTimestamp,ExecMainPID,FragmentPath",
    ]));
    let mut started_at = None;
    let mut main_pid = None;
    let mut fragment_path = None;
    for line in detail.lines() {
        if let Some((k, v)) = line.split_once('=') {
            match k {
                "ActiveEnterTimestamp" if !v.is_empty() => started_at = Some(v.to_string()),
                "ExecMainPID" if !v.is_empty() => main_pid = Some(v.to_string()),
                "FragmentPath" if !v.is_empty() => fragment_path = Some(v.to_string()),
                _ => {}
            }
        }
    }
    ServiceStatus {
        active: active_raw == "active",
        active_raw,
        enabled_raw,
        started_at,
        main_pid,
        fragment_path,
    }
}

pub fn build_portfolio_manifest(bots: &[BotConfig]) -> Manifest {
    let mut symbols = Vec::new();
    let mut seen = HashSet::new();
    let mut overlap = false;
    let pairs = bots.iter().map(|b| b.name.clone()).collect::<Vec<_>>();
    for bot in bots {
        for sym in [&bot.params.leg_a, &bot.params.leg_b] {
            let s = sym.to_string();
            if !seen.insert(s.clone()) {
                overlap = true;
            }
            symbols.push(s);
        }
    }
    symbols.sort();
    symbols.dedup();
    Manifest {
        pair_count: bots.len(),
        pairs,
        services: vec![RUNTIME_SERVICE.to_string()],
        symbols,
        has_symbol_overlap: overlap,
    }
}

pub fn loaded_params_view(bot: &BotConfig) -> LoadedParamsView {
    LoadedParamsView {
        rolling_window: bot.params.rolling_window,
        entry_z: bot.params.entry_z.to_string(),
        stop_z: bot.params.stop_z.to_string(),
        target_z: bot.params.target_z.to_string(),
        max_hold_bars: bot.params.max_hold_bars,
        risk_pct_of_equity: bot.params.risk_pct_of_equity.to_string(),
    }
}

pub fn format_loaded_params(params: &LoadedParamsView) -> String {
    format!(
        "window={} entry={} stop={} target={} hold={} risk={}",
        params.rolling_window,
        params.entry_z,
        params.stop_z,
        params.target_z,
        params.max_hold_bars,
        params.risk_pct_of_equity,
    )
}

fn position_view(p: &Position) -> PositionView {
    PositionView {
        symbol: p.symbol.to_string(),
        side: format!("{:?}", p.side),
        size: p.size.to_string(),
        entry_price: p.entry_price.to_string(),
        unrealized_pnl: p.unrealized_pnl.to_string(),
    }
}

fn order_view(o: &OpenOrder) -> OrderView {
    OrderView {
        symbol: o.symbol.to_string(),
        side: format!("{:?}", o.side),
        qty: o.qty.to_string(),
        price: o.price.to_string(),
        state: format!("{:?}", o.state),
        order_link_id: o.order_link_id.clone(),
    }
}

pub fn runtime_snapshot_path(db_path: &str) -> PathBuf {
    let base = Path::new(db_path);
    base.parent()
        .unwrap_or_else(|| Path::new("."))
        .join("pairs-runtime-snapshot.json")
}

pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), std::io::Error> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name().and_then(|s| s.to_str()).unwrap_or("snapshot"),
        std::process::id()
    ));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn empty_runtime_state() -> RuntimeStateView {
    RuntimeStateView {
        last_bar_ms: None,
        last_loop_wall_time: None,
        last_seen_z: None,
        last_seen_signal: None,
        last_guard_reason: None,
        position: None,
        loaded_params: None,
        snapshot_age_s: None,
        snapshot_stale: true,
    }
}

pub fn read_runtime_state(
    db_path: &str,
    bot_id: &str,
    stale_after_s: f64,
) -> Result<RuntimeStateView, ReportError> {
    let path = runtime_snapshot_path(db_path);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(empty_runtime_state()),
        Err(e) => return Err(ReportError::Io(e)),
    };
    let age = std::fs::metadata(&path)
        .ok()
        .and_then(|m| m.modified().ok())
        .and_then(|m| SystemTime::now().duration_since(m).ok())
        .map(|d| d.as_secs_f64());
    let stale = age.is_none_or(|a| a > stale_after_s);
    let states: std::collections::HashMap<String, RuntimeStateView> = serde_json::from_str(&text)?;
    let mut state = states.get(bot_id).cloned().unwrap_or_else(empty_runtime_state);
    state.snapshot_age_s = age;
    state.snapshot_stale = stale;
    Ok(state)
}

async fn latest_signal(rest: &BybitRest, bot: &BotConfig, margin: u16) -> Result<(Option<i64>, Option<f64>, Option<String>), ReportError> {
    let limit = bot.params.rolling_window as u16 + margin;
    let a = rest.klines(&bot.params.leg_a, bot.params.timeframe, limit).await?;
    let b = rest.klines(&bot.params.leg_b, bot.params.timeframe, limit).await?;
    let by_a = a.into_iter().map(|c| (c.open_time_ms, c.close)).collect::<std::collections::HashMap<_, _>>();
    let by_b = b.into_iter().map(|c| (c.open_time_ms, c.close)).collect::<std::collections::HashMap<_, _>>();
    let mut common = by_a.keys().copied().filter(|ms| by_b.contains_key(ms)).collect::<Vec<_>>();
    common.sort_unstable();
    if common.len() < bot.params.rolling_window + 1 {
        return Ok((None, None, None));
    }
    let mut rz = RollingZ::new(bot.params.rolling_window);
    let mut latest_z = None;
    for ms in &common {
        let a_close = by_a[ms].to_string().parse::<f64>().map_err(|e| ReportError::Data(format!("{} close parse: {e}", bot.params.leg_a)))?;
        let b_close = by_b[ms].to_string().parse::<f64>().map_err(|e| ReportError::Data(format!("{} close parse: {e}", bot.params.leg_b)))?;
        latest_z = rz.push(a_close.ln() - b_close.ln()).map(|s| s.z);
    }
    let latest_ms = *common.last().ok_or_else(|| ReportError::Data("no common candles".into()))?;
    let signal = latest_z.and_then(|z| SignalEngine::new(bot.params.clone()).entry_signal(z).map(|s| s.as_str().to_string()));
    Ok((Some(latest_ms), latest_z, signal))
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

pub async fn collect_status(profile: Profile, with_backtests: bool) -> Result<PortfolioStatus, ReportError> {
    let cfg = load_pairs_config(profile)?;
    let service = service_snapshot(RUNTIME_SERVICE);
    let generated_at_human = shell_line(Command::new("date").args(["+%Y-%m-%d %H:%M:%S %z"]));
    let creds = Credentials::from_env()?;
    let rest = BybitRest::new(profile.rest_base_url().to_string(), creds);
    let account_positions = rest.positions().await?.into_iter().map(|p| position_view(&p)).collect();
    let open_orders = rest.open_orders().await?.into_iter().map(|o| order_view(&o)).collect();
    let manifest = build_portfolio_manifest(&cfg.bots);

    let mut bots = Vec::new();
    for bot in &cfg.bots {
        let (latest_bar_ms, latest_z, current_signal, signal_error) = match latest_signal(&rest, bot, cfg.runtime.kline_margin_bars).await {
            Ok((bar, z, sig)) => (bar, z, sig, None),
            Err(e) => (None, None, None, Some(e.to_string())),
        };
        let runtime_state = read_runtime_state(
            &cfg.runtime.journal_path,
            &bot.id,
            SNAPSHOT_STALE_AFTER_S,
        )?;
        let backtest = if with_backtests {
            Some(split_summary(HISTORY_DB, &bot.params, 0.70).await?)
        } else {
            None
        };
        bots.push(BotStatus {
            name: bot.name.clone(),
            bot_id: bot.id.clone(),
            pair: format!("{}/{}", bot.params.leg_a, bot.params.leg_b),
            priority: bot.priority,
            risk_pct_of_equity: bot.params.risk_pct_of_equity.to_string(),
            loaded_params: loaded_params_view(bot),
            latest_bar_ms,
            latest_z,
            current_signal,
            signal_error,
            runtime_state,
            backtest,
        });
    }

    Ok(PortfolioStatus {
        generated_at_epoch: now_epoch(),
        generated_at_human,
        profile: profile.name().to_string(),
        service_status: service,
        manifest,
        account_positions,
        open_orders,
        bots,
    })
}

pub fn render_text(status: &PortfolioStatus) -> String {
    let mut lines = Vec::new();
    lines.push("Portfolio status".to_string());
    lines.push(format!("Generated: {}", status.generated_at_human));
    lines.push(format!(
        "Profile: {} | Service: {} (enabled={})",
        status.profile, status.service_status.active_raw, status.service_status.enabled_raw
    ));
    lines.push(format!(
        "Pairs: {} | Overlap: {}",
        status.manifest.pair_count,
        if status.manifest.has_symbol_overlap { "yes" } else { "no" }
    ));
    lines.push(format!(
        "Account positions: {} | Open orders: {}",
        status.account_positions.len(),
        status.open_orders.len()
    ));
    for bot in &status.bots {
        lines.push(String::new());
        lines.push(format!("[{}] {}", bot.name, bot.pair));
        lines.push(format!(
            "  signal: {:?} | z: {:?} | bar: {:?}",
            bot.current_signal, bot.latest_z, bot.latest_bar_ms
        ));
        lines.push(format!(
            "  runtime last_seen_signal: {:?} | guard: {:?} | snapshot_age_s: {:?} | snapshot_stale: {}",
            bot.runtime_state.last_seen_signal,
            bot.runtime_state.last_guard_reason,
            bot.runtime_state.snapshot_age_s.map(|v| v.round() as i64),
            bot.runtime_state.snapshot_stale,
        ));
        lines.push(format!(
            "  loaded params: {}",
            format_loaded_params(&bot.loaded_params),
        ));
        lines.push(format!(
            "  runtime params: {}",
            bot.runtime_state
                .loaded_params
                .as_ref()
                .map(format_loaded_params)
                .unwrap_or_else(|| "unavailable".to_string()),
        ));
        lines.push(format!("  local position: {:?}", bot.runtime_state.position.as_ref().map(|p| &p.side)));
        if let Some(err) = &bot.signal_error {
            lines.push(format!("  signal_error: {}", err));
        }
    }
    lines.join("\n")
}

pub fn render_dashboard_html(status: &PortfolioStatus) -> String {
    let bot_cards = status
        .bots
        .iter()
        .map(|bot| {
            let bt = bot.backtest.as_ref().map(|b| format!(
                "<div class=\"metrics\"><div><strong>full trades</strong><span>{}</span></div><div><strong>win rate</strong><span>{:.2}%</span></div><div><strong>PF</strong><span>{:.3}</span></div><div><strong>net</strong><span>{:.4}</span></div><div><strong>DD</strong><span>{:.2}%</span></div></div>",
                b.full.trades,
                b.full.win_rate * 100.0,
                b.full.profit_factor,
                b.full.net,
                b.full.max_drawdown_pct
            )).unwrap_or_default();
            let pos = bot.runtime_state.position.as_ref().map(|p| format!("{} @ z={} notional={}", p.side, p.entry_z, p.per_leg_notional_usdt)).unwrap_or_else(|| "flat".to_string());
            let snap_age = bot.runtime_state.snapshot_age_s.map(|v| format!("{v:.0}s")).unwrap_or_else(|| "n/a".to_string());
            format!(
                "<section class=\"card\"><h2>{}</h2><p class=\"sub\">{}</p><div class=\"metrics\"><div><strong>signal</strong><span>{}</span></div><div><strong>z</strong><span>{}</span></div><div><strong>bar</strong><span>{}</span></div><div><strong>risk</strong><span>{}</span></div><div><strong>snapshot age</strong><span>{}</span></div><div><strong>snapshot stale</strong><span>{}</span></div></div><p><strong>runtime:</strong> {}</p><p><strong>guard:</strong> {}</p>{}</section>",
                html_escape(&bot.name),
                html_escape(&bot.pair),
                html_escape(bot.current_signal.as_deref().unwrap_or("none")),
                bot.latest_z.map(|z| format!("{z:.4}")).unwrap_or_else(|| "n/a".into()),
                bot.latest_bar_ms.map(|ms| ms.to_string()).unwrap_or_else(|| "n/a".into()),
                html_escape(&bot.risk_pct_of_equity),
                html_escape(&snap_age),
                if bot.runtime_state.snapshot_stale { "yes" } else { "no" },
                html_escape(&pos),
                html_escape(bot.runtime_state.last_guard_reason.as_deref().unwrap_or("none")),
                bt,
            )
        })
        .collect::<Vec<_>>()
        .join("\n");

    let positions = if status.account_positions.is_empty() {
        "<p class=\"muted\">No open account positions.</p>".to_string()
    } else {
        let items = status.account_positions.iter().map(|p| format!("<li>{} {} size={} upnl={}</li>", html_escape(&p.symbol), html_escape(&p.side), html_escape(&p.size), html_escape(&p.unrealized_pnl))).collect::<Vec<_>>().join("");
        format!("<ul>{items}</ul>")
    };
    let orders = if status.open_orders.is_empty() {
        "<p class=\"muted\">No open orders.</p>".to_string()
    } else {
        let items = status.open_orders.iter().map(|o| format!("<li>{} {} qty={} price={} state={}</li>", html_escape(&o.symbol), html_escape(&o.side), html_escape(&o.qty), html_escape(&o.price), html_escape(&o.state))).collect::<Vec<_>>().join("");
        format!("<ul>{items}</ul>")
    };

    format!("<!doctype html><html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"><title>Pairs Portfolio Dashboard</title><style>body{{font-family:Inter,system-ui,sans-serif;background:#0b1020;color:#e8eefc;margin:0;padding:32px}}h1,h2{{margin:0 0 8px}}.sub,.muted{{color:#9fb0d1}}.grid{{display:grid;grid-template-columns:repeat(auto-fit,minmax(320px,1fr));gap:16px;margin-top:20px}}.card{{background:#121a2f;border:1px solid #22304f;border-radius:16px;padding:18px;box-shadow:0 8px 24px rgba(0,0,0,.25)}}.metrics{{display:grid;grid-template-columns:repeat(2,minmax(0,1fr));gap:12px;margin:14px 0}}.metrics div{{background:#0f1730;border-radius:12px;padding:10px 12px;border:1px solid #22304f}}strong{{display:block;color:#8fa6d8;font-size:12px;text-transform:uppercase;letter-spacing:.06em}}span{{font-size:16px;font-weight:700}}.top{{display:grid;grid-template-columns:repeat(auto-fit,minmax(220px,1fr));gap:14px}}ul{{padding-left:18px}}</style></head><body><h1>Pairs portfolio dashboard</h1><p class=\"sub\">Generated {} · profile {} · service {} (enabled={})</p><div class=\"top\"><section class=\"card\"><h2>Portfolio</h2><p>Pairs: {}<br>Overlap: {}<br>Symbols: {}</p></section><section class=\"card\"><h2>Account positions</h2>{}</section><section class=\"card\"><h2>Open orders</h2>{}</section></div><div class=\"grid\">{}</div></body></html>",
        html_escape(&status.generated_at_human),
        html_escape(&status.profile),
        html_escape(&status.service_status.active_raw),
        html_escape(&status.service_status.enabled_raw),
        status.manifest.pair_count,
        if status.manifest.has_symbol_overlap { "yes" } else { "no" },
        html_escape(&status.manifest.symbols.join(", ")),
        positions,
        orders,
        bot_cards,
    )
}

pub async fn write_dashboard(profile: Profile) -> Result<String, ReportError> {
    let status = collect_status(profile, true).await?;
    let html = render_dashboard_html(&status);
    std::fs::create_dir_all(Path::new(DASHBOARD_HTML).parent().expect("dashboard dir"))?;
    atomic_write(Path::new(DASHBOARD_HTML), html.as_bytes())?;
    Ok(DASHBOARD_HTML.to_string())
}
