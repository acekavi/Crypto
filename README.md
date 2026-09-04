# Crypto Portfolio Bot

This repository currently runs a **3-bot statistical-arbitrage testnet portfolio** on Bybit.

## Active live portfolio

- **AAVEUSDT / ETHUSDT**
  - window: `180`
  - entry z: `3.0`
  - stop z: `4.0`
  - target z: `0.0`
  - max hold: `48`
  - risk: `3%`
  - breakeven: `off`

- **ENAUSDT / XRPUSDT**
  - window: `336`
  - entry z: `3.25`
  - stop z: `4.5`
  - target z: `-0.5`
  - max hold: `96`
  - risk: `3%`
  - breakeven: `off`

- **BNBUSDT / XAUTUSDT**
  - window: `240`
  - entry z: `3.0`
  - stop z: `4.5`
  - target z: `-4.5`
  - max hold: `96`
  - risk: `3%`
  - breakeven: `off`

These three pairs are intentionally **non-overlapping** at the symbol level so they can share one account without cross-bot leg collisions.

## Active services

- `crypto-bot-aave-eth.service`
- `crypto-bot-ena-xrp.service`
- `crypto-bot-bnb-xaut.service`

## Monitoring jobs

- `pairs-bot-aave-eth-signal-watch`
- `pairs-bot-aave-eth-daily-summary`
- `pairs-bot-ena-xrp-signal-watch`
- `pairs-bot-ena-xrp-daily-summary`
- `pairs-bot-bnb-xaut-signal-watch`
- `pairs-bot-bnb-xaut-daily-summary`
- `pairs-dashboard-refresh`

## Important paths

- main bot logic: `scripts/pairs_bot.py`
- portfolio dashboard: `dashboard/pairs-dashboard.html`
- dashboard generator: `scripts/render_pairs_dashboard.py`
- portfolio status script: `scripts/portfolio_status.py`
- tests: `python_tests/test_pairs_bot.py`, `python_tests/test_portfolio_status.py`

## Status / inspection

### Portfolio-level status
```bash
cd /home/acekavi/Projects/Crypto
python scripts/portfolio_status.py
python scripts/portfolio_status.py --json
```

### Dashboard refresh
```bash
cd /home/acekavi/Projects/Crypto
python scripts/render_pairs_dashboard.py
```

### Systemd service status
```bash
systemctl --user list-units 'crypto-bot*' --no-pager
systemctl --user status crypto-bot-aave-eth.service --no-pager
systemctl --user status crypto-bot-ena-xrp.service --no-pager
systemctl --user status crypto-bot-bnb-xaut.service --no-pager
```

### Exchange/account sanity checks
```bash
cd /home/acekavi/Projects/Crypto
PYTHONPATH=/home/acekavi/Projects/Crypto python - <<'PY'
from scripts.pairs_bot import BybitClient
c = BybitClient('/home/acekavi/Projects/Crypto/.env')
print(c.request('GET', '/v5/position/list', {'category': 'linear', 'settleCoin': 'USDT'}))
print(c.request('GET', '/v5/order/realtime', {'category': 'linear', 'settleCoin': 'USDT'}))
PY
```

## Restart / rollout commands

### Restart all live portfolio bots
```bash
systemctl --user restart \
  crypto-bot-aave-eth.service \
  crypto-bot-ena-xrp.service \
  crypto-bot-bnb-xaut.service
```

### Start/stop individually
```bash
systemctl --user start crypto-bot-aave-eth.service
systemctl --user stop crypto-bot-aave-eth.service

systemctl --user start crypto-bot-ena-xrp.service
systemctl --user stop crypto-bot-ena-xrp.service

systemctl --user start crypto-bot-bnb-xaut.service
systemctl --user stop crypto-bot-bnb-xaut.service
```

## Test commands

```bash
cd /home/acekavi/Projects/Crypto
python -m unittest python_tests.test_pairs_bot
python -m unittest python_tests.test_portfolio_status
python -m py_compile scripts/pairs_bot.py scripts/render_pairs_dashboard.py scripts/portfolio_status.py
```

## Rollback note

This repo no longer keeps the old DOGE/XRP and LINK/XRP live stack in active service files or Hermes monitor wrappers. Rolling back to that prior live portfolio would require recreating those files from git history and re-installing the services/cron jobs.

## Reality check

The active portfolio is still a **testnet paper-trading setup**. Backtests looked strong, but live behavior can diverge because of slippage, two-leg execution risk, funding drift, and regime change. Use the dashboard and `portfolio_status.py` as routine sanity checks, not as proof the edge is permanent.
