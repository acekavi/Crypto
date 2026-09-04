# Pairs Runtime — Rust Port Design

**Status:** design, not yet built
**Date:** 2026-09-05
**Supersedes (at cutover):** `scripts/pairs_bot.py`, `scripts/portfolio_status.py`

## Why

The live money-touching path is 1,049 lines of Python in `scripts/pairs_bot.py`,
running as three independent systemd processes. It works, and it has four
defects that a language port does *not* fix on its own and that this design
exists to fix deliberately:

1. **Leg orphaning.** `place_pair_entry` places leg A, then leg B. If B fails it
   cancels A inside `except Exception: pass`. A is an aggressive limit already
   through the book, so it is usually *filled*; the cancel fails, the failure is
   swallowed, and the account holds a naked directional position while local
   state says flat. The next loop detects the mismatch and halts "until human
   review" — safe-ish, but the unhedged leg sits there indefinitely.
2. **Non-converging close.** `close_pair_position` raises before clearing
   `state.position`. If leg A's close fills and B's does not, the next bar
   retries a reduce-only order on an already-flat symbol, Bybit rejects it, and
   the loop never converges.
3. **The priority guard is dead code.** `BOT_PROFILES[*]['higher_priority_peers']`
   is built as `[]` and never populated, so `should_defer_to_higher_priority`
   cannot fire in production. Verified: `{'aave_eth': [], 'ena_xrp': [],
   'bnb_xaut': []}`. The unit tests pass only because they call the pure
   function with hand-built peer dicts.
4. **The sizing cap silently becomes the size.** `risk_based_per_leg_notional`
   computes `equity * risk_pct / sigma`; at realistic sigma the numerator
   exceeds equity, so `min(desired, available_equity)` binds and each leg goes
   out at 100% of equity — 2x gross. Confirmed in backtest: per-leg notional
   tracks equity exactly once the cap binds, which is what produces the
   dashboard's `net=82.49` (8,249%) headline for AAVE/ETH.

Plus two durability holes: the state file is written with a non-atomic
`path.write_text`, so a crash mid-write corrupts it and `Restart=always` then
crash-loops forever; and there is no retry, backoff, or error classification
around any HTTP call.

The Rust workspace beside it already solves the plumbing — `crates/exchange`
has signing, rate limiting, clock-offset correction, retry with
`ErrorClass`-driven backoff, and most V5 endpoints; `crates/persistence` has a
durable journal; `crates/engine` has a reconciler, order tracker and stop
escalation ladder; `crates/backtest` has costs, fills, metrics, walk-forward
and a gate. It compiles clean and carries 540 tests. This design reuses it.

## Non-goals

- **Performance of the live loop.** It is a 60-second poll dominated by 4–8 HTTP
  round trips. Rust makes it no faster. The only real speed win is the
  backtest, where `rolling_mean_stddev`'s O(n·window) rescan costs 12.5 s of CPU
  per dashboard refresh; an O(1) rolling accumulator in Rust takes that under
  100 ms.
- **The dashboard.** `scripts/render_pairs_dashboard.py` stays in Python. It is
  a read-only reporting surface with no latency requirement, and porting the
  HTML template buys nothing. It will read the Rust journal instead of the JSON
  state files (one small change, covered in Task 11).
- **Strategy changes.** Entry, exit, and sizing arithmetic port at exact parity,
  proven by golden vectors. Any edge change is a separate, separately-measured
  piece of work.

## Hard constraints

Carried from the project's standing rules; none may be quietly relaxed.

- **Limit orders only. No market orders anywhere, including unwinds.** An
  orphaned leg is flattened with an aggressive reduce-only *limit* escalated up
  a price ladder, never a market order.
- Legs enter and exit as `GTC` limits priced 5 ticks through the book. Not
  `PostOnly` — that is what `place_limit_entry` sends and it would be rejected
  at a crossing price. Not `IOC` — a partial fill that cancels the remainder
  leaves the pair mismatched with no record of intent.
- Testnet is the default profile; mainnet additionally requires
  `BYBIT_ALLOW_MAINNET`, exactly as `bot::config::Profile::from_name` already
  enforces.
- `rust_decimal::Decimal` for money and quantities. `f64` only inside the
  z-score statistics, matching the Python.

## Architecture

One new crate plus additive changes to four existing ones. One process replaces
three.

```
crates/pairs/            NEW. Pure strategy + the two-leg executor.
  spread.rs              O(1) rolling mean/sd/z. No I/O.
  signal.rs              PairParams, PairSide, entry/exit/breakeven rules.
  sizing.rs              per-leg notional, with the cap made observable.
  settle.rs              Pure classification of a two-leg placement outcome.
  executor.rs            open_pair / close_pair / unwind ladder. Async, trait-driven.
  supervisor.rs          N pairs in one process; the portfolio priority guard.

crates/exchange/         +LimitLeg placement, +order_by_link_id.
crates/botcore/          +LimitLeg type.
crates/persistence/      +pair_positions, +pair_events tables.
bot/                     +`pairs` binary, +config/pairs-*.toml.
```

### Why `ExchangeClient` grows rather than a parallel trait

`ExchangeClient`'s own doc comment states the reason: "Phase 2's
SimulatedExchange implements this same trait, which is what lets the backtester
drive the identical pipeline as live trading." Putting leg placement on a second
trait would give the pairs backtest a different execution path than the pairs
live bot — the exact divergence the trait exists to prevent. So `LimitLeg`
placement goes on `ExchangeClient`, and `SimulatedExchange` and `MockExchange`
both implement it.

### The two-leg protocol

Both legs are placed concurrently (`tokio::try_join!`), then both are polled to
a terminal state concurrently. A pure `settle` function classifies the pair of
outcomes into exactly three cases:

- `BothFilled` — the only success. Journal the position.
- `NeitherFilled` — cancel any resting remnant, journal a skipped entry, stay
  flat. No unwind needed.
- `Orphaned { filled, unfilled }` — cancel the unfilled leg, then run the unwind
  ladder on the filled one until it is flat.

Concurrent placement is chosen over the sequential alternative because it
minimises the price drift between legs, and because sequential placement does
not actually remove the need for an unwind path (leg A can fill and leg B can be
rejected for margin regardless of ordering). Given the unwind path must exist,
concurrency is free.

The unwind ladder mirrors `engine::EscalationLadder`: place an aggressive
reduce-only limit, wait for terminal state, and on each failed attempt step the
price further through the book. After `max_unwind_attempts` the bot journals
`PairUnwindFailed`, sets the journal halt flag, and logs at `error` — it does
not keep trying forever and it does not fall back to a market order.

### Close is position-aware and idempotent

`close_pair` reads `positions()` first and sends a reduce-only limit only for
legs that actually carry size. A leg already flat is a no-op, not an error. This
is what makes the retry-next-bar path converge instead of looping on rejections.

### State lives in the journal

The per-bot JSON file is replaced by two tables in the existing
`persistence::Journal` (Turso, local-first, already crash-safe and already
covered by `bot/tests/protection_persistence.rs`). On boot, `reconcile_pairs`
compares journal rows against `client.positions()`; a disagreement halts that
pair and reports it rather than guessing.

### The priority guard becomes real

Running all pairs in one process means peer state is an in-process
`Arc<RwLock<PortfolioGuard>>` rather than three processes trying to read each
other's files. The guard is a no-op for today's symbol-disjoint portfolio, which
is precisely why it must be correct before a future overlapping pair is added.

### Sizing: parity by default, visible always

`per_leg_notional` keeps the Python formula exactly. It returns
`Sizing { notional, capped_by }` so a capped size is journalled rather than
invisible, and gains a `max_notional_multiple_of_equity` knob that defaults to
`1.0` — reproducing current behaviour bit-for-bit while making the 2x-gross
exposure a decision someone typed rather than an accident of `min()`.

## Behaviour deliberately kept identical

- Exit-reason precedence stays breakeven → time → target → stop. The order only
  changes the *label*: every exit resolves on the same bar at the same price, so
  PnL is unaffected. Changing it would create a gratuitous backtest divergence.
- Population variance (`/ window`, not `/ (window - 1)`) and the `max(var, 1e-12)`
  floor.
- The z-window excludes the current bar (`values[i-window:i]`), which is what
  keeps the strategy free of lookahead.
- Age in bars is `(latest_ms - opened_at_ms) / TF_MS` in live. **The backtest
  currently disagrees** — it counts evaluated bars — so the port unifies both on
  the wall-clock formula and re-runs the backtest to quantify the difference.
  This is the one intentional strategy-affecting change, and Task 10 measures it.

## Acceptance

- Golden-vector parity with the Python for spread, rolling z, entry/exit
  decisions, and sizing.
- A fault-injecting exchange proves: entry rejection leaves the account flat;
  leg-B rejection after leg-A fill leaves the account flat via the unwind
  ladder; unwind exhaustion halts rather than escalating to a market order; a
  half-closed pair converges on the next attempt.
- A kill -9 between placement and journal write is reconciled correctly on the
  next boot.
- 7 days of shadow-mode running alongside the Python bots with zero signal
  disagreements before any cutover.
