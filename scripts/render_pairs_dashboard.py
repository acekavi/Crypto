from __future__ import annotations

import html
import json
import subprocess
import sys
import time
from collections import Counter, defaultdict
from dataclasses import asdict
from decimal import Decimal
from pathlib import Path
from typing import Any

PROJECT = Path('/home/acekavi/Projects/Crypto')
if str(PROJECT) not in sys.path:
    sys.path.insert(0, str(PROJECT))

from scripts.pairs_bot import (  # noqa: E402
    BybitClient,
    PairParams,
    RuntimeState,
    backtest,
    json_ready,
    load_pair_series_from_db,
    pair_slug,
    rolling_zscores,
    spread_series,
)

DB_PATH = PROJECT / 'data/history.db'
OUTPUT_PATH = PROJECT / 'dashboard/pairs-dashboard.html'
LOG_DIR = PROJECT / 'logs'
STATE_DIR = PROJECT / 'data'
ENV_PATH = PROJECT / '.env'
PAIRS = [
    {
        'name': 'DOGE/XRP',
        'bot_id': 'doge_xrp',
        'priority': 100,
        'service': 'crypto-bot.service',
        'params': PairParams(),
        'state_path': STATE_DIR / 'pairs_bot_state.json',
        'log_path': LOG_DIR / 'pairs-bot.log',
    },
    {
        'name': 'LINK/XRP',
        'bot_id': 'link_xrp',
        'priority': 50,
        'service': 'crypto-bot-link-xrp.service',
        'params': PairParams(leg_a='LINKUSDT', leg_b='XRPUSDT', timeframe='60', rolling_window=240, entry_z=3.5, stop_z=4.5, target_z=0.5),
        'state_path': STATE_DIR / 'pairs_bot_link_xrp_state.json',
        'log_path': LOG_DIR / 'pairs-bot-link-xrp.log',
    },
]


def run(cmd: list[str]) -> tuple[int, str]:
    proc = subprocess.run(cmd, capture_output=True, text=True)
    out = (proc.stdout or proc.stderr).strip()
    return proc.returncode, out


def tail_lines(path: Path, limit: int = 10) -> list[str]:
    if not path.exists():
        return []
    lines = path.read_text(errors='ignore').splitlines()
    return lines[-limit:]


def service_snapshot(service: str) -> dict[str, Any]:
    _, active = run(['systemctl', '--user', 'is-active', service])
    _, enabled = run(['systemctl', '--user', 'is-enabled', service])
    _, detail = run(['systemctl', '--user', 'show', service, '--property=ActiveEnterTimestamp,ExecMainPID,FragmentPath'])
    meta = {}
    for line in detail.splitlines():
        if '=' in line:
            k, v = line.split('=', 1)
            meta[k] = v
    return {
        'active': active == 'active',
        'active_raw': active,
        'enabled_raw': enabled,
        'started_at': meta.get('ActiveEnterTimestamp') or None,
        'main_pid': meta.get('ExecMainPID') or None,
        'fragment_path': meta.get('FragmentPath') or None,
    }


def fetch_klines(client: BybitClient, symbols: list[str], interval: str, limit: int) -> dict[str, list[dict]]:
    return {sym: client.get_closed_klines(sym, interval, limit) for sym in symbols}


def latest_signal_from_cache(klines: dict[str, list[dict]], params: PairParams) -> tuple[int | None, float | None, str | None]:
    by_a = {c['open_time_ms']: float(c['close']) for c in klines[params.leg_a]}
    by_b = {c['open_time_ms']: float(c['close']) for c in klines[params.leg_b]}
    common = sorted(set(by_a).intersection(by_b))
    if len(common) < params.rolling_window + 1:
        return None, None, None
    closes_a = [by_a[ms] for ms in common]
    closes_b = [by_b[ms] for ms in common]
    spreads = spread_series(closes_a, closes_b)
    zscores = rolling_zscores(spreads, params.rolling_window)
    latest_ms = common[-1]
    latest_z = zscores[-1]
    signal = None
    if latest_z is not None:
        if latest_z >= params.entry_z:
            signal = 'short_spread'
        elif latest_z <= -params.entry_z:
            signal = 'long_spread'
    return latest_ms, latest_z, signal


def load_backtest_summary(params: PairParams) -> dict[str, Any]:
    times, _, _ = load_pair_series_from_db(str(DB_PATH), params)
    split_idx = int(len(times) * 0.70)
    split_ms = times[split_idx]
    train = backtest(str(DB_PATH), params, end_ms=split_ms)
    hold = backtest(str(DB_PATH), params, start_ms=split_ms)
    full = backtest(str(DB_PATH), params)
    def compact(d: dict[str, Any]) -> dict[str, Any]:
        return {k: d[k] for k in ['trades', 'wins', 'losses', 'win_rate', 'profit_factor', 'net', 'avg_trade', 'max_drawdown_pct']}
    return {'train': compact(train), 'holdout': compact(hold), 'full': compact(full)}


def fetch_positions(client: BybitClient) -> list[dict[str, Any]]:
    res = client.request('GET', '/v5/position/list', {'category': 'linear', 'settleCoin': 'USDT'})
    out = []
    for row in res['result'].get('list', []):
        size = Decimal(str(row.get('size', '0')))
        if size == 0:
            continue
        out.append({
            'symbol': row.get('symbol'),
            'side': row.get('side'),
            'size': str(size),
            'avgPrice': row.get('avgPrice'),
            'unrealisedPnl': row.get('unrealisedPnl'),
            'positionValue': row.get('positionValue'),
            'updatedTime': row.get('updatedTime'),
        })
    return out


def fetch_open_orders_by_symbol(client: BybitClient, symbols: list[str]) -> dict[str, list[dict[str, Any]]]:
    out = {}
    for sym in symbols:
        res = client.request('GET', '/v5/order/realtime', {'category': 'linear', 'symbol': sym, 'limit': '50'})
        out[sym] = res['result'].get('list', [])
    return out


def fetch_executions_by_symbol(client: BybitClient, symbols: list[str]) -> dict[str, list[dict[str, Any]]]:
    out = {}
    for sym in symbols:
        res = client.request('GET', '/v5/execution/list', {'category': 'linear', 'symbol': sym, 'limit': '100'})
        out[sym] = res['result'].get('list', [])
    return out


def live_metrics_for_pair(params: PairParams, executions_by_symbol: dict[str, list[dict[str, Any]]]) -> dict[str, Any]:
    slug = pair_slug(params)
    rows = []
    for sym in (params.leg_a, params.leg_b):
        rows.extend(executions_by_symbol.get(sym, []))
    rows = [r for r in rows if (r.get('orderLinkId') or '').startswith(f'{slug}-')]
    fee_total = Decimal('0')
    close_groups: dict[str, Decimal] = defaultdict(lambda: Decimal('0'))
    last_exec_time = None
    for row in rows:
        fee_total += Decimal(str(row.get('execFee') or '0'))
        exec_time = row.get('execTime')
        if exec_time and (last_exec_time is None or int(exec_time) > int(last_exec_time)):
            last_exec_time = exec_time
        order_link_id = row.get('orderLinkId') or ''
        exec_pnl = Decimal(str(row.get('execPnl') or '0'))
        if f'{slug}-ca-' in order_link_id or f'{slug}-cb-' in order_link_id:
            group_id = order_link_id.rsplit('-', 1)[-1]
            close_groups[group_id] += exec_pnl
    closed_trade_pnls = list(close_groups.values())
    wins = sum(1 for x in closed_trade_pnls if x > 0)
    losses = sum(1 for x in closed_trade_pnls if x < 0)
    closed_trades = wins + losses
    return {
        'execution_rows': len(rows),
        'closed_trades': closed_trades,
        'wins': wins,
        'losses': losses,
        'win_rate': (wins / closed_trades) if closed_trades else None,
        'realized_pnl': str(sum(closed_trade_pnls, Decimal('0'))),
        'total_exec_fee': str(fee_total),
        'last_exec_time': last_exec_time,
    }


def render_html(data: dict[str, Any]) -> str:
    payload = json.dumps(data)
    template = '''<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8" />
  <meta name="viewport" content="width=device-width, initial-scale=1" />
  <meta http-equiv="refresh" content="300" />
  <title>Pairs Bot Dashboard</title>
  <style>
    :root {
      --bg: #0b1020;
      --panel: #121936;
      --panel-2: #182247;
      --border: #2d3a73;
      --text: #e9eeff;
      --muted: #98a6d6;
      --good: #35d49a;
      --warn: #ffcc66;
      --bad: #ff6b7a;
      --accent: #7cb2ff;
      --shadow: 0 18px 48px rgba(0,0,0,.28);
      --radius: 18px;
    }
    * { box-sizing: border-box; }
    body {
      margin: 0;
      font-family: ui-sans-serif, system-ui, -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif;
      background: linear-gradient(180deg, #0b1020 0%, #0d1430 100%);
      color: var(--text);
      line-height: 1.4;
    }
    .wrap { max-width: 1400px; margin: 0 auto; padding: 24px; }
    .header { display: grid; gap: 14px; margin-bottom: 22px; }
    .title { font-size: 30px; font-weight: 800; letter-spacing: -.03em; }
    .sub { color: var(--muted); font-size: 14px; }
    .warning { background: rgba(255, 204, 102, .08); border: 1px solid rgba(255, 204, 102, .35); color: #ffe09b; border-radius: 16px; padding: 14px 16px; }
    .grid { display: grid; grid-template-columns: repeat(12, 1fr); gap: 18px; }
    .card { background: rgba(18, 25, 54, .95); border: 1px solid var(--border); border-radius: var(--radius); box-shadow: var(--shadow); }
    .card h2, .card h3 { margin: 0; }
    .card-head { padding: 18px 18px 0; display: flex; justify-content: space-between; align-items: start; gap: 12px; }
    .card-body { padding: 18px; }
    .span-6 { grid-column: span 6; }
    .span-12 { grid-column: span 12; }
    .chips { display: flex; flex-wrap: wrap; gap: 8px; }
    .chip { border: 1px solid var(--border); background: var(--panel-2); border-radius: 999px; padding: 6px 10px; font-size: 12px; color: var(--muted); }
    .chip.good { color: var(--good); border-color: rgba(53,212,154,.35); }
    .chip.warn { color: var(--warn); border-color: rgba(255,204,102,.35); }
    .chip.bad { color: var(--bad); border-color: rgba(255,107,122,.35); }
    .stats { display: grid; grid-template-columns: repeat(4, minmax(0,1fr)); gap: 12px; margin-top: 16px; }
    .stat { background: rgba(255,255,255,.02); border: 1px solid rgba(124,178,255,.12); border-radius: 14px; padding: 14px; }
    .label { font-size: 12px; color: var(--muted); text-transform: uppercase; letter-spacing: .08em; }
    .value { margin-top: 8px; font-size: 24px; font-weight: 800; letter-spacing: -.03em; }
    .small { font-size: 13px; color: var(--muted); }
    table { width: 100%; border-collapse: collapse; margin-top: 14px; font-size: 13px; }
    th, td { text-align: left; padding: 10px 8px; border-bottom: 1px solid rgba(124,178,255,.12); vertical-align: top; }
    th { color: var(--muted); font-weight: 600; font-size: 12px; text-transform: uppercase; letter-spacing: .06em; }
    pre { margin: 0; white-space: pre-wrap; word-break: break-word; font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 12px; color: #c6d2ff; }
    .footer { margin-top: 18px; color: var(--muted); font-size: 12px; }
    @media (max-width: 1100px) { .span-6 { grid-column: span 12; } .stats { grid-template-columns: repeat(2, minmax(0,1fr)); } }
    @media (max-width: 640px) { .stats { grid-template-columns: 1fr; } .wrap { padding: 14px; } .title { font-size: 24px; } }
  </style>
</head>
<body>
  <div class="wrap" id="app"></div>
  <script>
    const data = __PAYLOAD__;
    const fmtPct = (v) => v === null || v === undefined ? '—' : `${(Number(v) * 100).toFixed(2)}%`;
    const fmtNum = (v, d = 4) => v === null || v === undefined ? '—' : Number(v).toFixed(d);
    const fmtRaw = (v) => v === null || v === undefined || v === '' ? '—' : String(v);
    const badge = (kind, text) => `<span class="chip ${kind}">${text}</span>`;
    const btRow = (label, s) => `<tr><td>${label}</td><td>${s.trades}</td><td>${fmtPct(s.win_rate)}</td><td>${fmtNum(s.profit_factor, 3)}</td><td>${fmtNum(s.net, 4)}</td><td>${fmtNum(s.max_drawdown_pct, 2)}%</td></tr>`;
    const positionTable = (rows) => rows.length ? `
      <table>
        <thead><tr><th>Symbol</th><th>Side</th><th>Size</th><th>Avg Price</th><th>Unrealized PnL</th><th>Updated</th></tr></thead>
        <tbody>${rows.map(r => `<tr><td>${r.symbol}</td><td>${fmtRaw(r.side)}</td><td>${fmtRaw(r.size)}</td><td>${fmtRaw(r.avgPrice)}</td><td>${fmtRaw(r.unrealisedPnl)}</td><td>${fmtRaw(r.updatedTime)}</td></tr>`).join('')}</tbody>
      </table>` : `<div class="small">No open account-level positions on the tracked symbols.</div>`;

    const botCard = (bot) => {
      const statePos = bot.runtime_state.position;
      return `
      <section class="card span-6">
        <div class="card-head">
          <div>
            <h2>${bot.name}</h2>
            <div class="sub">Service: ${bot.service} · Pair: ${bot.params.leg_a} / ${bot.params.leg_b} · Priority ${bot.priority}</div>
          </div>
          <div class="chips">
            ${bot.service_status.active ? badge('good', 'service active') : badge('bad', `service ${bot.service_status.active_raw}`)}
            ${bot.current_signal ? badge('warn', `signal ${bot.current_signal}`) : badge('good', 'no entry signal')}
            ${statePos ? badge('warn', `local state open: ${statePos.side}`) : badge('good', 'local state flat')}
          </div>
        </div>
        <div class="card-body">
          <div class="stats">
            <div class="stat"><div class="label">Current z-score</div><div class="value">${fmtNum(bot.latest_z, 4)}</div></div>
            <div class="stat"><div class="label">Recent live realized PnL</div><div class="value">${fmtNum(bot.live_metrics.realized_pnl, 4)}</div></div>
            <div class="stat"><div class="label">Live closed trades</div><div class="value">${fmtRaw(bot.live_metrics.closed_trades)}</div></div>
            <div class="stat"><div class="label">Backtest full win rate</div><div class="value">${fmtPct(bot.backtest.full.win_rate)}</div></div>
          </div>
          <table>
            <thead><tr><th>Metric</th><th>Value</th><th>Metric</th><th>Value</th></tr></thead>
            <tbody>
              <tr><td>Last closed bar</td><td>${fmtRaw(bot.latest_bar_ms)}</td><td>Last loop heartbeat</td><td>${fmtRaw(bot.runtime_state.last_loop_wall_time)}</td></tr>
              <tr><td>Live win rate</td><td>${bot.live_metrics.win_rate === null ? '—' : fmtPct(bot.live_metrics.win_rate)}</td><td>Execution fees</td><td>${fmtNum(bot.live_metrics.total_exec_fee, 4)}</td></tr>
              <tr><td>Open orders for this bot</td><td>${fmtRaw(bot.open_orders.length)}</td><td>Last execution time</td><td>${fmtRaw(bot.live_metrics.last_exec_time)}</td></tr>
              <tr><td>Guard / defer reason</td><td>${fmtRaw(bot.runtime_state.last_guard_reason)}</td><td>Started at</td><td>${fmtRaw(bot.service_status.started_at)}</td></tr>
            </tbody>
          </table>
          <table>
            <thead><tr><th>Backtest slice</th><th>Trades</th><th>Win rate</th><th>Profit factor</th><th>Net</th><th>Max DD</th></tr></thead>
            <tbody>
              ${btRow('Train', bot.backtest.train)}
              ${btRow('Holdout', bot.backtest.holdout)}
              ${btRow('Full', bot.backtest.full)}
            </tbody>
          </table>
          <div class="small" style="margin:14px 0 8px;">Recent log lines</div>
          <pre>${bot.recent_logs.map(x => x.replace(/[<>&]/g, s => ({'<':'&lt;','>':'&gt;','&':'&amp;'}[s]))).join('\n') || 'No log lines yet.'}</pre>
        </div>
      </section>`;
    };

    document.getElementById('app').innerHTML = `
      <header class="header">
        <div class="title">Pairs Bot Dashboard</div>
        <div class="sub">Monitor surface. Auto-regenerated every 5 minutes and auto-reloads in-browser every 5 minutes. Generated at ${data.generated_at_human}.</div>
        ${data.shared_symbol_risk ? `<div class="warning"><strong>Shared-symbol risk:</strong> both bots use <code>XRPUSDT</code> on the same Bybit testnet account. Bybit nets positions by symbol, so account-level XRP exposure can interfere across bots. <strong>Priority policy:</strong> DOGE/XRP has priority 100 and LINK/XRP has priority 50. LINK/XRP defers when DOGE/XRP already has a local XRP position or a same-bar entry signal. Local bot state and account net positions are shown separately on purpose.</div>` : ''}
      </header>
      <main class="grid">
        <section class="card span-12">
          <div class="card-head"><div><h2>Account-level net positions</h2><div class="sub">These come from Bybit testnet, not from local bot state files.</div></div></div>
          <div class="card-body">${positionTable(data.account_positions)}</div>
        </section>
        ${data.bots.map(botCard).join('')}
      </main>
      <div class="footer">File: ${data.output_path} · Refresh cadence: every 5 minutes · Values labeled “live” are recent exchange/account data; values labeled “backtest” come from the local history database.</div>
    `;
  </script>
</body>
</html>'''
    return template.replace('__PAYLOAD__', payload)


def main() -> None:
    client = BybitClient(str(ENV_PATH))
    unique_symbols = sorted({sym for p in PAIRS for sym in (p['params'].leg_a, p['params'].leg_b)})
    max_window = max(p['params'].rolling_window for p in PAIRS) + 5
    klines = fetch_klines(client, unique_symbols, '60', max_window)
    positions = fetch_positions(client)
    open_orders = fetch_open_orders_by_symbol(client, unique_symbols)
    executions = fetch_executions_by_symbol(client, unique_symbols)
    symbol_counts = Counter(sym for p in PAIRS for sym in (p['params'].leg_a, p['params'].leg_b))
    data = {
        'generated_at_epoch': time.time(),
        'generated_at_human': time.strftime('%Y-%m-%d %H:%M:%S %z'),
        'output_path': str(OUTPUT_PATH),
        'shared_symbol_risk': any(count > 1 for count in symbol_counts.values()),
        'account_positions': positions,
        'bots': [],
    }
    for item in PAIRS:
        params = item['params']
        latest_bar_ms, latest_z, current_signal = latest_signal_from_cache(klines, params)
        runtime = RuntimeState.from_file(item['state_path'], params)
        slug = pair_slug(params)
        pair_orders = []
        for sym in (params.leg_a, params.leg_b):
            pair_orders.extend([row for row in open_orders.get(sym, []) if (row.get('orderLinkId') or '').startswith(f'{slug}-')])
        data['bots'].append({
            'name': item['name'],
            'bot_id': item['bot_id'],
            'priority': item['priority'],
            'service': item['service'],
            'params': json_ready(asdict(params)),
            'service_status': service_snapshot(item['service']),
            'latest_bar_ms': latest_bar_ms,
            'latest_z': latest_z,
            'current_signal': current_signal,
            'runtime_state': json_ready(asdict(runtime)),
            'open_orders': pair_orders,
            'recent_logs': tail_lines(item['log_path'], 10),
            'backtest': json_ready(load_backtest_summary(params)),
            'live_metrics': live_metrics_for_pair(params, executions),
        })
    OUTPUT_PATH.parent.mkdir(parents=True, exist_ok=True)
    OUTPUT_PATH.write_text(render_html(data))
    print(str(OUTPUT_PATH))


if __name__ == '__main__':
    main()
