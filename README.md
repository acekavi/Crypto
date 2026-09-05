# Crypto Portfolio Bot

This repository implements a **Rust-native statistical arbitrage portfolio bot** for Bybit testnet. Its purpose is to trade **spread mean reversion** across a small set of curated two-leg relationships while preserving strict control over execution, state, and portfolio interactions.

The project is designed around a simple principle: a trading system is only as good as its ability to distinguish between

- a valid signal,
- a tradable signal,
- a filled position,
- a safely managed open position, and
- a backtest result that is robust rather than flattering.

This is therefore not just a signal generator. It is a full research-to-runtime system for running a small, coordinated pairs portfolio under explicit operational constraints.

## Conceptual model

The strategy trades **relative value dislocations** rather than outright directional forecasts.

For each configured pair, the system models a log spread:

- `spread = ln(price_a) - ln(price_b)`

A rolling mean and rolling standard deviation define the local equilibrium and dispersion of that spread. The current spread is then expressed as a **z-score**, which becomes the central state variable for the strategy.

The interpretation is standard mean reversion:

- a sufficiently positive z-score implies the spread is unusually wide and may justify a **short-spread** entry;
- a sufficiently negative z-score implies the spread is unusually compressed and may justify a **long-spread** entry;
- reversion toward the target closes the trade;
- further adverse expansion toward the stop exits the trade defensively;
- a time stop closes positions whose reversion thesis did not resolve quickly enough.

The system therefore seeks edge not from predicting absolute price direction, but from repeatedly exploiting temporary deviations in relative pricing between two instruments.

## Why pairs trading changes the engineering problem

A pairs strategy is not a single-symbol strategy with a different indicator. It changes the architecture materially.

The primary risks are different:

- **relationship risk**: the spread can stop behaving mean-revertively;
- **execution asymmetry**: one leg can fill while the other fails or lags;
- **portfolio interference**: multiple strategies can fight over the same symbol;
- **false research confidence**: attractive historical results can be artifacts of compounding, tiny samples, or isolated folds.

Because of that, this repository treats multi-leg execution, restart safety, and research hygiene as part of the core strategy design rather than auxiliary tooling.

## Portfolio architecture

The runtime is intentionally consolidated into **one coordinated process**:

- one live service,
- one active portfolio definition,
- one portfolio guard,
- one database-backed journal,
- one runtime snapshot,
- one alerting path.

This avoids the failure modes that usually appear when pairs are deployed as unrelated bots:

- duplicated exposure on shared symbols,
- conflicting signals on the same bar,
- inconsistent state across processes,
- fragmented monitoring,
- ambiguous ownership of open legs.

The portfolio guard is responsible for preventing lower-priority pairs from acting when a higher-priority pair already owns or is about to own a shared symbol. That turns the portfolio into a coordinated decision system rather than a loose collection of bots.

## Runtime philosophy

The runtime is conservative by design.

It assumes that the dangerous cases are normal cases:

- a valid signal may still be untradeable,
- exchange state may not match local state,
- an entry may partially fill,
- an unwind may fail,
- a restart may happen while positions exist,
- observability must not contend destructively with the live writer.

As a result, the bot is built around the following principles:

- **limit-order execution only**;
- **explicit two-leg open and unwind handling**;
- **persistent journaled state** rather than ad-hoc in-memory assumptions;
- **runtime heartbeats and snapshots** for safe inspection;
- **single-service deployment** rather than a swarm of helper daemons;
- **alerting only on meaningful trade lifecycle events**.

The implementation goal is not to look elegant in isolation. It is to make the system hard to lie about under live conditions.

## Research philosophy

The research layer exists to reduce false positives.

The repository now evaluates candidates using several distinct views:

- **train / holdout / full-sample separation**;
- **risk-sized and fixed-notional comparison**;
- **local neighborhood audits** around the active parameter set;
- **walk-forward selection** over rolling in-sample / out-of-sample folds;
- **promotion gating** that classifies candidates as `promote`, `watchlist`, or `reject`.

This matters because historical profitability alone is not a trustworthy selector. In pairs systems, misleading candidates often arise from:

- extreme compounding artifacts,
- infinite profit-factor behavior on tiny samples,
- weak out-of-sample depth,
- one strong fold surrounded by instability,
- parameter choices that are locally attractive but operationally fragile.

The current research path is therefore structured to reward candidates that remain coherent across multiple lenses, not just candidates that print the largest historical number.

## Persistence and observability

The bot persists operationally meaningful state, including:

- pair positions,
- pair events,
- heartbeats,
- orders,
- equity snapshots,
- halt state.

This supports three independent verification paths:

1. **configuration view** — what the configured portfolio says should run;
2. **status/report view** — what the reporting layer loaded from config;
3. **runtime view** — what the live process logged at startup and wrote into the runtime snapshot.

The dashboard and status tooling are intentionally read-oriented. They exist to inspect the running portfolio without requiring a parallel monitor process to remain alive in the background.

## Operational footprint

The project is intentionally lean.

Current background footprint:

- **one live service**: `crypto-pairs.service`
- **zero cron monitor jobs**
- **in-process Telegram alerts** for real trade lifecycle events only

That means:

- no minute-by-minute watchdog spam,
- no scheduled dashboard refresher,
- no daily-summary daemons,
- no duplicated monitoring logic outside the bot.

The always-on system is the bot itself. Everything else can be generated on demand from persisted state and exchange readback.

## Active live testnet portfolio

The current testnet portfolio is intentionally non-overlapping at the symbol level:

- **AAVEUSDT / ETHUSDT**
- **ENAUSDT / XRPUSDT**
- **BNBUSDT / XAUTUSDT**

These are chosen to fit inside one shared account without cross-pair leg collisions.

### AAVEUSDT / ETHUSDT
- rolling window: `210`
- entry z: `3.0`
- stop z: `4.5`
- target z: `0.0`
- max hold bars: `48`
- risk: `3%`
- breakeven: `off`

### ENAUSDT / XRPUSDT
- rolling window: `336`
- entry z: `3.25`
- stop z: `4.5`
- target z: `-0.5`
- max hold bars: `120`
- risk: `3%`
- breakeven: `off`

### BNBUSDT / XAUTUSDT
- rolling window: `200`
- entry z: `3.0`
- stop z: `4.75`
- target z: `-4.5`
- max hold bars: `72`
- risk: `3%`
- breakeven: `off`

## Limits of the system

This remains a **testnet paper-trading system**.

That is not a disclaimer to ignore; it is an active constraint on how results should be interpreted. Strong historical metrics do not eliminate live risk. The unresolved live uncertainties remain the usual ones for multi-leg statistical trading:

- slippage,
- maker/taker behavior different from assumptions,
- spread regime drift,
- funding effects,
- partial fill and stranded-leg paths,
- exchange-side behavior under stress,
- decaying predictive value in the underlying relationship.

In other words, this repository can support disciplined research and disciplined execution, but it cannot prove the edge is permanent.

## Minimal practical ops

Important paths:

- live runtime: `bot/src/bin/pairs.rs`
- status tool: `bot/src/bin/pairs_status.rs`
- dashboard tool: `bot/src/bin/pairs_dashboard.rs`
- backtest / audit tool: `bot/src/bin/pairs_backtest.rs`
- active testnet config: `config/pairs-testnet.toml`
- service file: `deploy/crypto-pairs.service`
- generated dashboard: `dashboard/pairs-dashboard.html`

Useful commands:

```bash
# Status
cargo run -q -p bot --bin pairs_status -- testnet
cargo run -q -p bot --bin pairs_status -- testnet --json

# Dashboard on demand
cargo run -q -p bot --bin pairs_dashboard -- testnet

# Backtest / audit
cargo run -q -p bot --bin pairs_backtest -- testnet --bot-id aave_eth --json
cargo run -q -p bot --bin pairs_backtest -- testnet --bot-id aave_eth --audit-current
cargo run -q -p bot --bin pairs_backtest -- testnet --bot-id aave_eth --audit-walk-forward

# Live service
systemctl --user status crypto-pairs.service --no-pager
journalctl --user -u crypto-pairs.service -n 50 --no-pager
```

## Summary

This project is a **Rust, database-backed, single-service statistical arbitrage portfolio bot** for Bybit testnet. Its value is not just in producing spread signals, but in combining research discipline, execution discipline, portfolio coordination, persistent state, and runtime verifiability into one coherent system.