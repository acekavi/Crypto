# Crypto Portfolio Bot

This repository runs a **single-process Rust statistical-arbitrage testnet portfolio** on Bybit.

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

## Active service

- `crypto-pairs.service`

## Monitoring jobs

- `pairs-bot-aave-eth-signal-watch`
- `pairs-bot-aave-eth-daily-summary`
- `pairs-bot-ena-xrp-signal-watch`
- `pairs-bot-ena-xrp-daily-summary`
- `pairs-bot-bnb-xaut-signal-watch`
- `pairs-bot-bnb-xaut-daily-summary`
- `pairs-dashboard-refresh`

## Important paths

- live runtime: `bot/src/bin/pairs.rs`
- status tool: `bot/src/bin/pairs_status.rs`
- dashboard tool: `bot/src/bin/pairs_dashboard.rs`
- backtest tool: `bot/src/bin/pairs_backtest.rs`
- watchdog tool: `bot/src/bin/pairs_watchdog.rs`
- daily summary tool: `bot/src/bin/pairs_daily_summary.rs`
- active config: `config/pairs-testnet.toml`
- portfolio dashboard: `dashboard/pairs-dashboard.html`
- repo service file: `deploy/crypto-pairs.service`

## Status / inspection

### Portfolio-level status
```bash
cd /home/acekavi/Projects/Crypto
cargo run -q -p bot --bin pairs_status -- testnet
cargo run -q -p bot --bin pairs_status -- testnet --json
```

### Dashboard refresh
```bash
cd /home/acekavi/Projects/Crypto
cargo run -q -p bot --bin pairs_dashboard -- testnet
```

### Backtest snapshot
```bash
cd /home/acekavi/Projects/Crypto
cargo run -q -p bot --bin pairs_backtest -- testnet --bot-id aave_eth --json
```

### Systemd service status
```bash
systemctl --user status crypto-pairs.service --no-pager
journalctl --user -u crypto-pairs -n 50 --no-pager
```

### Exchange/account sanity checks
```bash
cd /home/acekavi/Projects/Crypto
cargo run -q -p bot --bin pairs_status -- testnet --json
```

## Restart / rollout commands

### Build release runtime
```bash
cd /home/acekavi/Projects/Crypto
cargo build --release -p bot --bin crypto-pairs --bin pairs_status --bin pairs_dashboard --bin pairs_backtest --bin pairs_watchdog --bin pairs_daily_summary
```

### Restart the live portfolio runtime
```bash
systemctl --user restart crypto-pairs.service
```

### Start/stop the live runtime
```bash
systemctl --user start crypto-pairs.service
systemctl --user stop crypto-pairs.service
```

## Test commands

```bash
cd /home/acekavi/Projects/Crypto
cargo test --workspace
cargo build --release -p bot --bin crypto-pairs --bin pairs_status --bin pairs_dashboard --bin pairs_backtest --bin pairs_watchdog --bin pairs_daily_summary
```

## Rollback note

This repo no longer keeps the old multi-service Python live path as the primary runtime. Rolling back means restoring the retired Python files and per-pair systemd units from git history, then reinstalling them.

## Reality check

The active portfolio is still a **testnet paper-trading setup**. Backtests looked strong, but live behavior can diverge because of slippage, two-leg execution risk, funding drift, and regime change. Use the dashboard and `pairs_status` as routine sanity checks, not as proof the edge is permanent.
