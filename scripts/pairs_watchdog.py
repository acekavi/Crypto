from __future__ import annotations

import json
import time
from pathlib import Path

from scripts.pairs_bot import BybitClient, PairParams, RuntimeState, compute_latest_live_signal

BOT_STATE = Path('/home/acekavi/Projects/Crypto/data/pairs_bot_state.json')
WATCH_STATE = Path('/home/acekavi/Projects/Crypto/data/pairs_watchdog_state.json')
LOG_FILE = Path('/home/acekavi/Projects/Crypto/logs/pairs-bot.log')
ENV_PATH = '/home/acekavi/Projects/Crypto/.env'
STALL_THRESHOLD_S = 10 * 60


def load_watch_state() -> dict:
    if not WATCH_STATE.exists():
        return {}
    return json.loads(WATCH_STATE.read_text())


def save_watch_state(data: dict) -> None:
    WATCH_STATE.parent.mkdir(parents=True, exist_ok=True)
    WATCH_STATE.write_text(json.dumps(data, indent=2, sort_keys=True))


def read_new_log_chunk(prior: dict) -> str:
    if not LOG_FILE.exists():
        prior['last_log_size'] = 0
        return ''
    last_size = int(prior.get('last_log_size', 0) or 0)
    current_size = LOG_FILE.stat().st_size
    if current_size < last_size:
        last_size = 0
    with LOG_FILE.open('r', encoding='utf-8', errors='ignore') as fh:
        fh.seek(last_size)
        chunk = fh.read()
    prior['last_log_size'] = current_size
    return chunk


def main() -> None:
    now = time.time()
    params = PairParams()
    runtime = RuntimeState.from_file(BOT_STATE, params)
    client = BybitClient(ENV_PATH)
    latest_ms, latest_z, signal = compute_latest_live_signal(client, params)
    prior = load_watch_state()
    alerts: list[str] = []

    last_signal_bar = prior.get('last_signal_bar_ms')
    if signal is not None and latest_ms != last_signal_bar:
        alerts.append(
            f"Pairs bot signal: {signal.value} on {params.leg_a}/{params.leg_b} at bar {latest_ms} with z={latest_z:.4f}."
        )
        prior['last_signal_bar_ms'] = latest_ms

    prev_had_position = bool(prior.get('position_opened_at_ms'))
    current_pos = runtime.position
    if current_pos and prior.get('position_opened_at_ms') != current_pos.opened_at_ms:
        alerts.append(
            f"Pairs bot position OPEN: {current_pos.side.value} on {params.leg_a}/{params.leg_b}, opened_at_ms={current_pos.opened_at_ms}, entry_z={current_pos.entry_z:.4f}."
        )
        prior['position_opened_at_ms'] = current_pos.opened_at_ms
    elif not current_pos and prev_had_position:
        alerts.append(f"Pairs bot position CLOSED on {params.leg_a}/{params.leg_b}.")
        prior['position_opened_at_ms'] = None

    heartbeat = runtime.last_loop_wall_time
    stale_now = heartbeat is None or (now - heartbeat) > STALL_THRESHOLD_S
    stale_before = bool(prior.get('stall_alert_active'))
    if stale_now and not stale_before:
        age = None if heartbeat is None else int(now - heartbeat)
        alerts.append(
            f"Pairs bot WARNING: stalled heartbeat for {params.leg_a}/{params.leg_b}. last_loop_age_s={age}."
        )
        prior['stall_alert_active'] = True
    elif not stale_now and stale_before:
        alerts.append(
            f"Pairs bot RECOVERED: heartbeat advancing again on {params.leg_a}/{params.leg_b}."
        )
        prior['stall_alert_active'] = False

    chunk = read_new_log_chunk(prior)
    if ' ERROR ' in chunk or 'Traceback' in chunk or 'loop failed:' in chunk:
        if chunk.strip():
            tail = '\n'.join(chunk.strip().splitlines()[-8:])
            alerts.append(f"Pairs bot ERROR detected in file log:\n{tail}")

    prior['last_seen_bar_ms'] = latest_ms
    prior['last_seen_z'] = latest_z
    prior['last_seen_signal'] = signal.value if signal else None
    prior['last_watchdog_wall_time'] = now
    save_watch_state(prior)
    if alerts:
        print('\n\n'.join(alerts))


if __name__ == '__main__':
    main()
