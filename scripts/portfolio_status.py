from __future__ import annotations

import argparse
import json
import subprocess
import sys
import time
from collections import Counter
from pathlib import Path
from typing import Any

PROJECT = Path('/home/acekavi/Projects/Crypto')
if str(PROJECT) not in sys.path:
    sys.path.insert(0, str(PROJECT))

from scripts.pairs_bot import BybitClient, RuntimeState, active_bot_profiles, compute_latest_live_signal, json_ready  # noqa: E402

ENV_PATH = PROJECT / '.env'


def run(cmd: list[str]) -> tuple[int, str]:
    proc = subprocess.run(cmd, capture_output=True, text=True)
    out = (proc.stdout or proc.stderr).strip()
    return proc.returncode, out


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


def build_portfolio_manifest(profiles: list[dict]) -> dict[str, Any]:
    symbols = []
    services = []
    pairs = []
    for profile in profiles:
        pairs.append(profile['name'])
        services.append(profile['service'])
        symbols.extend([profile['params'].leg_a, profile['params'].leg_b])
    counts = Counter(symbols)
    return {
        'pair_count': len(profiles),
        'pairs': pairs,
        'services': sorted(services),
        'symbols': sorted(set(symbols)),
        'has_symbol_overlap': any(v > 1 for v in counts.values()),
    }


def fetch_account_positions(client: BybitClient) -> list[dict[str, Any]]:
    res = client.request('GET', '/v5/position/list', {'category': 'linear', 'settleCoin': 'USDT'})
    rows = []
    for row in res['result'].get('list', []):
        if abs(float(row.get('size') or 0)) > 0:
            rows.append({
                'symbol': row.get('symbol'),
                'side': row.get('side'),
                'size': row.get('size'),
                'avgPrice': row.get('avgPrice'),
                'positionValue': row.get('positionValue'),
                'unrealisedPnl': row.get('unrealisedPnl'),
            })
    return rows


def fetch_open_orders(client: BybitClient) -> list[dict[str, Any]]:
    res = client.request('GET', '/v5/order/realtime', {'category': 'linear', 'settleCoin': 'USDT'})
    rows = []
    for row in res['result'].get('list', []):
        if row.get('orderStatus') not in {'Cancelled', 'Filled', 'Deactivated'}:
            rows.append({
                'symbol': row.get('symbol'),
                'side': row.get('side'),
                'qty': row.get('qty'),
                'price': row.get('price'),
                'orderStatus': row.get('orderStatus'),
                'orderLinkId': row.get('orderLinkId'),
            })
    return rows


def collect_status() -> dict[str, Any]:
    profiles = active_bot_profiles()
    manifest = build_portfolio_manifest(profiles)
    client = BybitClient(str(ENV_PATH))
    account_positions = fetch_account_positions(client)
    open_orders = fetch_open_orders(client)
    bots = []
    for profile in profiles:
        params = profile['params']
        runtime = RuntimeState.from_file(profile['state_path'], params)
        latest_ms = None
        latest_z = None
        current_signal = None
        signal_error = None
        try:
            latest_ms, latest_z, sig = compute_latest_live_signal(client, params)
            current_signal = None if sig is None else sig.value
        except Exception as exc:  # pragma: no cover - defensive read-only path
            signal_error = str(exc)
        bots.append({
            'name': profile['name'],
            'bot_id': profile['bot_id'],
            'service': profile['service'],
            'pair': params.display_pair(),
            'risk_pct_of_equity': str(params.risk_pct_of_equity),
            'service_status': service_snapshot(profile['service']),
            'latest_bar_ms': latest_ms,
            'latest_z': latest_z,
            'current_signal': current_signal,
            'signal_error': signal_error,
            'runtime_state': json_ready({
                'last_bar_ms': runtime.last_bar_ms,
                'last_loop_wall_time': runtime.last_loop_wall_time,
                'last_seen_z': runtime.last_seen_z,
                'last_seen_signal': runtime.last_seen_signal,
                'last_guard_reason': runtime.last_guard_reason,
                'position': None if runtime.position is None else {
                    'side': runtime.position.side.value,
                    'opened_at_ms': runtime.position.opened_at_ms,
                    'entry_z': runtime.position.entry_z,
                    'per_leg_notional_usdt': runtime.position.per_leg_notional_usdt,
                },
            }),
        })
    return {
        'generated_at_epoch': time.time(),
        'generated_at_human': time.strftime('%Y-%m-%d %H:%M:%S %z'),
        'manifest': manifest,
        'account_positions': account_positions,
        'open_orders': open_orders,
        'bots': bots,
    }


def render_text(status: dict[str, Any]) -> str:
    lines = []
    manifest = status['manifest']
    lines.append('Portfolio status')
    lines.append(f"Generated: {status['generated_at_human']}")
    lines.append(f"Pairs: {manifest['pair_count']} | Overlap: {'yes' if manifest['has_symbol_overlap'] else 'no'}")
    lines.append(f"Services: {', '.join(manifest['services'])}")
    lines.append(f"Account positions: {len(status['account_positions'])} | Open orders: {len(status['open_orders'])}")
    for bot in status['bots']:
        svc = bot['service_status']
        lines.append('')
        lines.append(f"[{bot['name']}] {bot['pair']}")
        lines.append(f"  service: {bot['service']} ({svc['active_raw']}, enabled={svc['enabled_raw']})")
        lines.append(f"  signal: {bot['current_signal']} | z: {bot['latest_z']} | bar: {bot['latest_bar_ms']}")
        lines.append(f"  runtime last_seen_signal: {bot['runtime_state']['last_seen_signal']} | guard: {bot['runtime_state']['last_guard_reason']}")
        lines.append(f"  local position: {bot['runtime_state']['position']}")
        if bot['signal_error']:
            lines.append(f"  signal_error: {bot['signal_error']}")
    return '\n'.join(lines)


def main() -> None:
    parser = argparse.ArgumentParser(description='Show portfolio-level status for the active pairs bots')
    parser.add_argument('--json', action='store_true', help='print machine-readable JSON')
    args = parser.parse_args()
    status = collect_status()
    if args.json:
        print(json.dumps(json_ready(status), indent=2, sort_keys=True))
    else:
        print(render_text(status))


if __name__ == '__main__':
    main()
