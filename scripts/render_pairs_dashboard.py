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

    equity = 0.0
    equity_curve = []
    for trade in full['trades_detail']:
        equity += float(trade['net'])
        equity_curve.append(round(equity, 6))

    recent_trades = []
    for trade in full['trades_detail'][-8:]:
        recent_trades.append({
            'entry_ms': trade['entry_ms'],
            'exit_ms': trade['exit_ms'],
            'side': trade['side'],
            'reason': trade['reason'],
            'entry_z': round(float(trade['entry_z']), 4),
            'exit_z': round(float(trade['exit_z']), 4),
            'net': round(float(trade['net']), 6),
        })

    return {
        'train': compact(train),
        'holdout': compact(hold),
        'full': compact(full),
        'full_equity_curve': equity_curve,
        'recent_trades': recent_trades,
    }


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


def fetch_closed_pnl_by_symbol(client: BybitClient, symbols: list[str]) -> dict[str, list[dict[str, Any]]]:
    out = {}
    for sym in symbols:
        res = client.request('GET', '/v5/position/closed-pnl', {'category': 'linear', 'symbol': sym, 'limit': '100'})
        out[sym] = res['result'].get('list', [])
    return out


def build_live_trade_ledger(params: PairParams, executions_by_symbol: dict[str, list[dict[str, Any]]], closed_pnl_by_symbol: dict[str, list[dict[str, Any]]]) -> dict[str, Any]:
    slug = pair_slug(params)
    bot_exec_rows: list[dict[str, Any]] = []
    closed_pnl_map: dict[str, dict[str, Any]] = {}
    for sym in (params.leg_a, params.leg_b):
        bot_exec_rows.extend([r for r in executions_by_symbol.get(sym, []) if (r.get('orderLinkId') or '').startswith(f'{slug}-')])
        for row in closed_pnl_by_symbol.get(sym, []):
            order_id = row.get('orderId')
            if order_id:
                closed_pnl_map[order_id] = row

    close_groups: dict[str, dict[str, Any]] = {}
    fee_total = Decimal('0')
    for row in bot_exec_rows:
        fee_total += Decimal(str(row.get('execFee') or '0'))
        order_link_id = row.get('orderLinkId') or ''
        if f'{slug}-ca-' not in order_link_id and f'{slug}-cb-' not in order_link_id:
            continue
        group_id = order_link_id.rsplit('-', 1)[-1]
        group = close_groups.setdefault(group_id, {
            'group_id': group_id,
            'exit_time': None,
            'realized_pnl': Decimal('0'),
            'fees': Decimal('0'),
            'legs': [],
            'symbols': set(),
            'source': 'execution_fallback',
        })
        cp = closed_pnl_map.get(row.get('orderId') or '')
        exec_time = int(row.get('execTime') or 0)
        updated_time = int(cp.get('updatedTime') or 0) if cp else 0
        candidate_time = max(exec_time, updated_time)
        if candidate_time and (group['exit_time'] is None or candidate_time > group['exit_time']):
            group['exit_time'] = candidate_time
        group['symbols'].add(row.get('symbol'))
        if cp:
            group['source'] = 'closed_pnl'
            realized = Decimal(str(cp.get('closedPnl') or '0'))
            fees = Decimal(str(cp.get('openFee') or '0')) + Decimal(str(cp.get('closeFee') or '0'))
            leg = {
                'symbol': cp.get('symbol') or row.get('symbol'),
                'side': cp.get('side') or row.get('side'),
                'qty': cp.get('closedSize') or row.get('execQty'),
                'entry_price': cp.get('avgEntryPrice'),
                'exit_price': cp.get('avgExitPrice'),
                'pnl': str(realized),
                'order_id': cp.get('orderId') or row.get('orderId'),
            }
        else:
            realized = Decimal(str(row.get('execPnl') or '0'))
            fees = Decimal(str(row.get('execFee') or '0'))
            leg = {
                'symbol': row.get('symbol'),
                'side': row.get('side'),
                'qty': row.get('execQty'),
                'entry_price': None,
                'exit_price': row.get('execPrice'),
                'pnl': str(realized),
                'order_id': row.get('orderId'),
            }
        if not any(existing['order_id'] == leg['order_id'] for existing in group['legs']):
            group['legs'].append(leg)
            group['realized_pnl'] += realized
            group['fees'] += fees

    trades = []
    for group in close_groups.values():
        trades.append({
            'group_id': group['group_id'],
            'exit_time': group['exit_time'],
            'realized_pnl': float(group['realized_pnl']),
            'fees': float(group['fees']),
            'symbols': sorted(group['symbols']),
            'source': group['source'],
            'legs': group['legs'],
        })
    trades.sort(key=lambda x: (x['exit_time'] or 0, x['group_id']))

    cumulative = 0.0
    equity_curve = []
    wins = losses = 0
    for trade in trades:
        cumulative += trade['realized_pnl']
        equity_curve.append(round(cumulative, 6))
        if trade['realized_pnl'] > 0:
            wins += 1
        elif trade['realized_pnl'] < 0:
            losses += 1

    closed_trades = wins + losses
    return {
        'execution_rows': len(bot_exec_rows),
        'closed_trades': closed_trades,
        'wins': wins,
        'losses': losses,
        'win_rate': (wins / closed_trades) if closed_trades else None,
        'realized_pnl': round(cumulative, 6),
        'total_exec_fee': round(float(fee_total), 6),
        'last_exec_time': trades[-1]['exit_time'] if trades else None,
        'recent_closed_trades': trades[-8:],
        'live_equity_curve': equity_curve,
    }


def live_metrics_for_pair(params: PairParams, executions_by_symbol: dict[str, list[dict[str, Any]]], closed_pnl_by_symbol: dict[str, list[dict[str, Any]]]) -> dict[str, Any]:
    return build_live_trade_ledger(params, executions_by_symbol, closed_pnl_by_symbol)


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
    .charts { display: grid; grid-template-columns: repeat(2, minmax(0,1fr)); gap: 12px; margin-top: 14px; }
    .chart { background: rgba(255,255,255,.02); border: 1px solid rgba(124,178,255,.12); border-radius: 14px; padding: 12px; }
    .chart svg { width: 100%; height: 90px; display: block; }
    .stat { background: rgba(255,255,255,.02); border: 1px solid rgba(124,178,255,.12); border-radius: 14px; padding: 14px; }
    .label { font-size: 12px; color: var(--muted); text-transform: uppercase; letter-spacing: .08em; }
    .value { margin-top: 8px; font-size: 24px; font-weight: 800; letter-spacing: -.03em; }
    .small { font-size: 13px; color: var(--muted); }
    .muted-box { background: rgba(255,255,255,.02); border: 1px solid rgba(124,178,255,.12); border-radius: 14px; padding: 12px; }
    table { width: 100%; border-collapse: collapse; margin-top: 14px; font-size: 13px; }
    th, td { text-align: left; padding: 10px 8px; border-bottom: 1px solid rgba(124,178,255,.12); vertical-align: top; }
    th { color: var(--muted); font-weight: 600; font-size: 12px; text-transform: uppercase; letter-spacing: .06em; }
    pre { margin: 0; white-space: pre-wrap; word-break: break-word; font-family: ui-monospace, SFMono-Regular, Menlo, monospace; font-size: 12px; color: #c6d2ff; }
    .footer { margin-top: 18px; color: var(--muted); font-size: 12px; }
    @media (max-width: 1100px) { .span-6 { grid-column: span 12; } .stats { grid-template-columns: repeat(2, minmax(0,1fr)); } .charts { grid-template-columns: 1fr; } }
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
    const sparkline = (values, stroke) => {
      if (!values || !values.length) return `<div class="small">No data yet.</div>`;
      const width = 320, height = 90, pad = 6;
      const min = Math.min(...values), max = Math.max(...values);
      const span = Math.max(max - min, 1e-9);
      const pts = values.map((v, i) => {
        const x = pad + (i * (width - pad * 2)) / Math.max(values.length - 1, 1);
        const y = height - pad - ((v - min) / span) * (height - pad * 2);
        return `${x.toFixed(2)},${y.toFixed(2)}`;
      }).join(' ');
      const zeroY = min <= 0 && max >= 0 ? height - pad - ((0 - min) / span) * (height - pad * 2) : null;
      return `<svg viewBox="0 0 ${width} ${height}" preserveAspectRatio="none">${zeroY === null ? '' : `<line x1="0" y1="${zeroY.toFixed(2)}" x2="${width}" y2="${zeroY.toFixed(2)}" stroke="rgba(152,166,214,.25)" stroke-dasharray="4 4" />`}<polyline fill="none" stroke="${stroke}" stroke-width="3" points="${pts}" stroke-linecap="round" stroke-linejoin="round" /></svg>`;
    };
    const closedTradesTable = (rows) => rows.length ? `
      <table>
        <thead><tr><th>Exit Time</th><th>PnL</th><th>Fees</th><th>Source</th><th>Legs</th></tr></thead>
        <tbody>${rows.slice().reverse().map(r => `<tr><td>${fmtRaw(r.exit_time)}</td><td>${fmtNum(r.realized_pnl, 4)}</td><td>${fmtNum(r.fees, 4)}</td><td>${fmtRaw(r.source)}</td><td>${r.legs.map(leg => `${leg.symbol} ${leg.side} qty=${fmtRaw(leg.qty)} pnl=${fmtRaw(leg.pnl)}`).join('<br>')}</td></tr>`).join('')}</tbody>
      </table>` : `<div class="muted-box small">No closed live trades yet for this bot. The explicit ledger is wired up, but there are no real close rows to render yet.</div>`;
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
          <div class="charts">
            <div class="chart">
              <div class="label">Backtest equity curve</div>
              ${sparkline(bot.backtest.full_equity_curve, '#7cb2ff')}
            </div>
            <div class="chart">
              <div class="label">Live realized PnL curve</div>
              ${sparkline(bot.live_metrics.live_equity_curve, '#35d49a')}
            </div>
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
          <div class="small" style="margin:14px 0 8px;">Recent live closed trades</div>
          ${closedTradesTable(bot.live_metrics.recent_closed_trades)}
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
    closed_pnl = fetch_closed_pnl_by_symbol(client, unique_symbols)
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
            'live_metrics': live_metrics_for_pair(params, executions, closed_pnl),
        })
    OUTPUT_PATH.parent.mkdir(parents=True, exist_ok=True)
    OUTPUT_PATH.write_text(render_html(data))
    print(str(OUTPUT_PATH))


if __name__ == '__main__':
    main()
