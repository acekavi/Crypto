# Durable Trade Journal and Restart Safety — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Every trade and every state change is written to the local database as it happens, and a restart rebuilds full protection state from that record rather than losing it.

**Architecture:** Two new tables. `stop_protections` mirrors `EngineLoop::protections` so the in-memory map can be reconstructed at startup; it is written on every mutation, so the database is authoritative rather than a periodic snapshot. `trade_events` is an append-only audit log — one row per lifecycle event, never updated or deleted. Startup restores protections after `reconcile`, so the exchange decides which positions exist and the journal supplies what the exchange does not report.

**Tech Stack:** Rust 2024, `rust_decimal::Decimal` (never `f64`), libSQL/Turso, `async_trait`, tokio.

## Global Constraints

- **Never `f64` for money.** All prices, quantities and PnL are `rust_decimal::Decimal`. A grep test enforces this.
- **Decimals persist as TEXT, never REAL.** Never `ORDER BY`, `MIN`, `MAX` or compare a Decimal column in SQL — `"9" > "10000"` lexicographically. Sort and compare in Rust after parsing. This has bitten this project three times.
- **Limit orders only, never market.** `crates/exchange/tests/no_market_orders.rs` must keep passing.
- **Never log a Turso error's `Display`** — it can embed `TURSO_DATABASE_URL`. Log the error kind only, as `bot/src/main.rs` already does.
- **The journal must never prevent the bot from trading.** Turso being unreachable already falls back to local-only. A failed *write* must be logged and surfaced, never silently swallowed and never a panic — but it also must not abort the trading loop.
- **`liquidity_sweep_v2` stays frozen.** Its tripwire tests must keep passing unchanged. This plan changes no strategy rule except the declared warm-up length in Task 4.
- Run `cargo fmt --all`, then `cargo clippy --workspace --all-targets -- -D warnings`, then `cargo test --workspace` — all clean **before** each commit, not after.
- Commit messages describe only what the commit does. No AI/Claude attribution.

## File Structure

| File | Responsibility |
|---|---|
| `crates/persistence/src/schema.rs` | The two new `CREATE TABLE` statements |
| `crates/persistence/src/journal.rs` | Upsert/delete/load protections; append/query trade events |
| `crates/persistence/src/records.rs` (or wherever `OrderRecord` lives) | `ProtectionRecord`, `TradeEvent`, `TradeEventKind` |
| `bot/src/engine_loop.rs` | Write through on every protection mutation; emit events; `restore_protections` |
| `bot/src/main.rs` | Call `restore_protections` after `reconcile` |
| `crates/strategy/src/ict.rs` | Declared warm-up 100 → 250 |

---

### Task 1: Persist stop protections

**Files:**
- Modify: `crates/persistence/src/schema.rs`
- Modify: `crates/persistence/src/journal.rs`
- Modify: the module defining `OrderRecord` (find it; add `ProtectionRecord` alongside)
- Test: `crates/persistence/tests/protections.rs` (create)

**Interfaces:**
- Produces:
  - `ProtectionRecord { symbol: Symbol, order_link_id: String, side: Side, trigger: Decimal, atr: Decimal, entry_price: Decimal, initial_risk: Decimal, stop_limit_offset: Decimal, breakeven_at_r: Option<Decimal>, moved_to_breakeven: bool, updated_at_ms: i64 }`
  - `Journal::upsert_protection(&ProtectionRecord) -> Result<(), JournalError>`
  - `Journal::delete_protection(&Symbol) -> Result<(), JournalError>`
  - `Journal::load_protections() -> Result<Vec<ProtectionRecord>, JournalError>`

Schema — note every Decimal is TEXT:

```sql
CREATE TABLE IF NOT EXISTS stop_protections (
    symbol             TEXT PRIMARY KEY,
    order_link_id      TEXT NOT NULL,
    side               TEXT NOT NULL,
    trigger            TEXT NOT NULL,
    atr                TEXT NOT NULL,
    entry_price        TEXT NOT NULL,
    initial_risk       TEXT NOT NULL,
    stop_limit_offset  TEXT NOT NULL,
    breakeven_at_r     TEXT,
    moved_to_breakeven INTEGER NOT NULL,
    updated_at_ms      INTEGER NOT NULL
)
```

`symbol` is the primary key because the engine holds at most one position per symbol, exactly as `protections: HashMap<Symbol, _>` does. An upsert on symbol therefore cannot create a duplicate.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn a_protection_round_trips_through_the_journal() {
    let j = Journal::open_local(":memory:").await.expect("journal opens");
    let p = sample_protection("BTCUSDT");
    j.upsert_protection(&p).await.expect("upsert");

    let loaded = j.load_protections().await.expect("load");
    assert_eq!(loaded, vec![p]);
}

#[tokio::test]
async fn upserting_the_same_symbol_replaces_rather_than_duplicates() {
    let j = Journal::open_local(":memory:").await.expect("journal opens");
    j.upsert_protection(&sample_protection("BTCUSDT")).await.expect("first");
    let moved = ProtectionRecord { moved_to_breakeven: true, trigger: dec!(100), ..sample_protection("BTCUSDT") };
    j.upsert_protection(&moved).await.expect("second");

    let loaded = j.load_protections().await.expect("load");
    assert_eq!(loaded.len(), 1, "one position per symbol means one row");
    assert!(loaded[0].moved_to_breakeven);
    assert_eq!(loaded[0].trigger, dec!(100));
}

#[tokio::test]
async fn deleting_a_protection_removes_only_that_symbol() {
    let j = Journal::open_local(":memory:").await.expect("journal opens");
    j.upsert_protection(&sample_protection("BTCUSDT")).await.expect("btc");
    j.upsert_protection(&sample_protection("ETHUSDT")).await.expect("eth");
    j.delete_protection(&Symbol::new("BTCUSDT")).await.expect("delete");

    let loaded = j.load_protections().await.expect("load");
    assert_eq!(loaded.len(), 1);
    assert_eq!(loaded[0].symbol, Symbol::new("ETHUSDT"));
}

#[tokio::test]
async fn a_protection_with_no_breakeven_round_trips_as_none_not_zero() {
    // Zero would mean "move the stop to entry immediately"; None means never.
    let j = Journal::open_local(":memory:").await.expect("journal opens");
    let p = ProtectionRecord { breakeven_at_r: None, ..sample_protection("BTCUSDT") };
    j.upsert_protection(&p).await.expect("upsert");

    let loaded = j.load_protections().await.expect("load");
    assert_eq!(loaded[0].breakeven_at_r, None);
}

#[tokio::test]
async fn decimal_values_survive_magnitudes_that_break_text_ordering() {
    // Decimals persist as TEXT. "9" sorts above "10000" lexicographically, so
    // any SQL comparison on these columns is wrong; this pins the round-trip
    // values rather than any ordering.
    let j = Journal::open_local(":memory:").await.expect("journal opens");
    let small = ProtectionRecord { symbol: Symbol::new("AAA"), trigger: dec!(9), ..sample_protection("AAA") };
    let large = ProtectionRecord { symbol: Symbol::new("BBB"), trigger: dec!(10000.00000001), ..sample_protection("BBB") };
    j.upsert_protection(&small).await.expect("small");
    j.upsert_protection(&large).await.expect("large");

    let loaded = j.load_protections().await.expect("load");
    let by = |s: &str| loaded.iter().find(|p| p.symbol.as_str() == s).expect("present").trigger;
    assert_eq!(by("AAA"), dec!(9));
    assert_eq!(by("BBB"), dec!(10000.00000001));
}
```

Write `sample_protection(symbol: &str) -> ProtectionRecord` as a local helper with plausible non-zero values. `ProtectionRecord` needs `Debug, Clone, PartialEq` for these assertions.

If `Journal::open_local(":memory:")` is not supported by the existing setup, use a `tempfile`-backed path and follow whatever convention `crates/persistence/tests/` already uses.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p persistence --test protections`
Expected: FAIL to compile — `ProtectionRecord` and the three methods do not exist.

- [ ] **Step 3: Implement the schema and the three methods**

Add the `CREATE TABLE` to `schema.rs`'s statement list. Implement `upsert_protection` with `INSERT ... ON CONFLICT(symbol) DO UPDATE SET ...`, `delete_protection`, and `load_protections`. Follow the exact parameter-binding and Decimal-as-TEXT conventions already used by `record_order` and `order_by_link_id`.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p persistence`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
git add -A && git commit -m "feat(persistence): persist stop protections so a restart can rebuild them"
```

---

### Task 2: Append-only trade event log

**Files:**
- Modify: `crates/persistence/src/schema.rs`
- Modify: `crates/persistence/src/journal.rs`
- Modify: the module defining `OrderRecord`
- Test: `crates/persistence/tests/trade_events.rs` (create)

**Interfaces:**
- Produces:
  - `TradeEventKind` — an enum with `as_str()` and `from_str()`, variants: `EntryPlaced`, `EntryFilled`, `EntryExpired`, `EntryCancelled`, `StopPlaced`, `StopMovedToBreakeven`, `StopAmendFailed`, `StopEscalated`, `StopLadderExhausted`, `PositionClosed`, `ProtectionRestored`, `HaltSet`, `HaltCleared`
  - `TradeEvent { at_ms: i64, symbol: Symbol, order_link_id: Option<String>, kind: TradeEventKind, detail: String, config_hash: String }`
  - `Journal::record_event(&TradeEvent) -> Result<(), JournalError>`
  - `Journal::events_for(&Symbol) -> Result<Vec<TradeEvent>, JournalError>` — chronological
  - `Journal::event_count() -> Result<i64, JournalError>`

```sql
CREATE TABLE IF NOT EXISTS trade_events (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    at_ms         INTEGER NOT NULL,
    symbol        TEXT NOT NULL,
    order_link_id TEXT,
    kind          TEXT NOT NULL,
    detail        TEXT NOT NULL,
    config_hash   TEXT NOT NULL
)
```

`at_ms` is INTEGER so it can be ordered in SQL safely — it is a timestamp, not a Decimal. Order by `at_ms, id` so events written in the same millisecond keep insertion order.

`detail` is free text for humans, e.g. `"stop 61234.5 -> 62000.0 (entry)"`. Do not parse it back; anything a machine needs belongs in its own column.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn events_come_back_in_chronological_order() {
    let j = Journal::open_local(":memory:").await.expect("journal opens");
    for (at, kind) in [
        (300, TradeEventKind::StopMovedToBreakeven),
        (100, TradeEventKind::EntryPlaced),
        (200, TradeEventKind::EntryFilled),
    ] {
        j.record_event(&event("BTCUSDT", at, kind)).await.expect("record");
    }
    let got = j.events_for(&Symbol::new("BTCUSDT")).await.expect("load");
    let kinds: Vec<_> = got.iter().map(|e| e.kind).collect();
    assert_eq!(kinds, vec![
        TradeEventKind::EntryPlaced,
        TradeEventKind::EntryFilled,
        TradeEventKind::StopMovedToBreakeven,
    ]);
}

#[tokio::test]
async fn two_events_in_the_same_millisecond_keep_insertion_order() {
    let j = Journal::open_local(":memory:").await.expect("journal opens");
    j.record_event(&event("BTCUSDT", 100, TradeEventKind::EntryFilled)).await.expect("first");
    j.record_event(&event("BTCUSDT", 100, TradeEventKind::StopPlaced)).await.expect("second");
    let got = j.events_for(&Symbol::new("BTCUSDT")).await.expect("load");
    assert_eq!(got[0].kind, TradeEventKind::EntryFilled);
    assert_eq!(got[1].kind, TradeEventKind::StopPlaced);
}

#[tokio::test]
async fn events_are_scoped_to_their_symbol() {
    let j = Journal::open_local(":memory:").await.expect("journal opens");
    j.record_event(&event("BTCUSDT", 100, TradeEventKind::EntryPlaced)).await.expect("btc");
    j.record_event(&event("ETHUSDT", 100, TradeEventKind::EntryPlaced)).await.expect("eth");
    assert_eq!(j.events_for(&Symbol::new("BTCUSDT")).await.expect("load").len(), 1);
    assert_eq!(j.event_count().await.expect("count"), 2);
}

#[tokio::test]
async fn every_event_kind_round_trips_through_its_string_form() {
    // A kind that fails to parse back would silently vanish from the audit
    // trail, which is the one thing this table exists to prevent.
    for kind in TradeEventKind::ALL {
        assert_eq!(TradeEventKind::from_str(kind.as_str()), Some(kind));
    }
}

#[tokio::test]
async fn an_event_without_an_order_link_id_round_trips_as_none() {
    let j = Journal::open_local(":memory:").await.expect("journal opens");
    let e = TradeEvent { order_link_id: None, ..event("BTCUSDT", 100, TradeEventKind::HaltSet) };
    j.record_event(&e).await.expect("record");
    assert_eq!(j.events_for(&Symbol::new("BTCUSDT")).await.expect("load")[0].order_link_id, None);
}
```

Add `TradeEventKind::ALL: [TradeEventKind; N]` so the round-trip test cannot silently miss a variant added later. Write `event(...)` as a local helper.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p persistence --test trade_events`
Expected: FAIL to compile.

- [ ] **Step 3: Implement**

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p persistence`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
git add -A && git commit -m "feat(persistence): append-only trade event log"
```

---

### Task 3: Write through from the engine, and restore on startup

**Files:**
- Modify: `bot/src/engine_loop.rs`
- Modify: `bot/src/main.rs`
- Test: `bot/tests/protection_persistence.rs` (create)

**This is the task that delivers the goal.** Tasks 1 and 2 only build the storage.

**Design notes:**

- `EngineLoop` already holds an `Arc<Journal>`. Every site that mutates `self.protections` must write through in the same operation:
  - insert (entry filled, stop recorded) → `upsert_protection` + `EntryFilled` / `StopPlaced` events
  - breakeven amend accepted → `upsert_protection` (with `moved_to_breakeven: true` and the rewritten trigger) + `StopMovedToBreakeven` event
  - breakeven amend rejected → `StopAmendFailed` event, no protection write (the in-memory state did not change either)
  - escalation rung advanced → `upsert_protection` + `StopEscalated`
  - ladder exhausted → `StopLadderExhausted`
  - position gone / pruned → `delete_protection` + `PositionClosed`
- **A journal write failure must not abort the trading loop.** Log at `error!` and continue — losing an audit row is bad, refusing to manage an open position is worse. Do not `?` these into the caller's error type.
- `restore_protections(&mut self, open: &[Position])` loads from the journal and inserts a protection for every symbol that is **both** in the journal **and** currently open on the exchange. The exchange is authoritative about which positions exist; the journal supplies only what the exchange does not report. A journal row for a symbol with no open position is stale — delete it and record `PositionClosed`.
- Emit `ProtectionRestored` for each one adopted, so the audit trail shows the restart.
- Call it from `main.rs` immediately **after** `reconcile` and **before** the loop starts.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn a_filled_entry_writes_its_protection_to_the_journal() {
    let (engine, journal) = engine_with_journal().await;
    // ... drive the engine so an entry fills and a stop is recorded ...
    let saved = journal.load_protections().await.expect("load");
    assert_eq!(saved.len(), 1);
    assert_eq!(saved[0].symbol, Symbol::new("BTCUSDT"));
    assert!(!saved[0].moved_to_breakeven);
}

#[tokio::test]
async fn moving_a_stop_to_breakeven_is_persisted_before_the_next_tick() {
    let (mut engine, journal) = engine_with_journal().await;
    // ... arrange a position at 2R, run drive_breakeven_stops ...
    let saved = journal.load_protections().await.expect("load");
    assert!(saved[0].moved_to_breakeven, "the amend must be durable, not in-memory only");
    assert_eq!(saved[0].trigger, dec!(100), "the rewritten trigger must persist too");
}

#[tokio::test]
async fn a_restart_rebuilds_protections_for_positions_still_open() {
    let (mut engine, journal) = engine_with_journal().await;
    // ... establish a protection, then drop the engine entirely ...
    let mut restarted = engine_sharing(&journal).await;
    restarted.restore_protections(&[open_position("BTCUSDT", Side::Buy, dec!(100))]).await.expect("restore");
    assert_eq!(restarted.protection_count_for_test(), 1);
    assert_eq!(restarted.recorded_trigger_for_test(&Symbol::new("BTCUSDT")), Some(dec!(90)));
}

#[tokio::test]
async fn a_restored_position_still_moves_to_breakeven() {
    // The whole point: after a restart the trade must still be managed.
    let (_, journal) = engine_with_journal().await;
    let mut restarted = engine_sharing(&journal).await;
    restarted.restore_protections(&[open_position("BTCUSDT", Side::Buy, dec!(100))]).await.expect("restore");
    // price at 2R
    restarted.drive_breakeven_stops().await.expect("breakeven");
    assert_eq!(mock_amends().len(), 1);
}

#[tokio::test]
async fn a_journal_row_with_no_open_position_is_dropped_not_adopted() {
    // The exchange is authoritative about what is open. Adopting a stale row
    // would have the engine managing a position that does not exist.
    let (_, journal) = engine_with_journal().await;
    let mut restarted = engine_sharing(&journal).await;
    restarted.restore_protections(&[]).await.expect("restore");
    assert_eq!(restarted.protection_count_for_test(), 0);
    assert!(journal.load_protections().await.expect("load").is_empty(), "the stale row must be deleted");
}

#[tokio::test]
async fn a_closed_position_deletes_its_protection_row() {
    let (mut engine, journal) = engine_with_journal().await;
    // ... establish a protection, then have the exchange report no positions ...
    assert!(journal.load_protections().await.expect("load").is_empty());
}

#[tokio::test]
async fn a_journal_write_failure_does_not_stop_the_engine_managing_the_position() {
    // Losing an audit row is bad; refusing to manage an open trade is worse.
    let (mut engine, _) = engine_with_failing_journal().await;
    engine.drive_breakeven_stops().await.expect("a journal failure must not propagate");
    assert_eq!(mock_amends().len(), 1, "the stop still moved");
}

#[tokio::test]
async fn the_trade_event_log_records_the_whole_lifecycle() {
    let (mut engine, journal) = engine_with_journal().await;
    // ... fill an entry, move to breakeven, close the position ...
    let kinds: Vec<_> = journal.events_for(&Symbol::new("BTCUSDT")).await
        .expect("load").iter().map(|e| e.kind).collect();
    assert!(kinds.contains(&TradeEventKind::EntryFilled));
    assert!(kinds.contains(&TradeEventKind::StopMovedToBreakeven));
    assert!(kinds.contains(&TradeEventKind::PositionClosed));
}
```

Adapt names to the crate's real API. `bot/tests/breakeven_behaviour.rs` already builds an `EngineLoop` over `MockExchange` — reuse its helpers rather than inventing new ones. Add a journal double or a temp-file journal, whichever matches existing convention.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p bot --test protection_persistence`
Expected: FAIL — `restore_protections` does not exist and nothing writes through.

- [ ] **Step 3: Implement write-through at every mutation site**

`bot/src/engine_loop.rs` mutates `protections` at roughly lines 256, 350, 402, 521, 553, 779 — verify each against current code rather than trusting these numbers.

- [ ] **Step 4: Implement `restore_protections`**

- [ ] **Step 5: Call it from `main.rs` after `reconcile`**

Add a test proving startup actually calls it — this project has shipped three features that were implemented, tested and never invoked (`299bf40`, `3a725d5`, and the warm-up bug `4ce2f44`). If `main.rs` is not testable, extract the smallest seam.

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test --workspace`
Expected: PASS.

- [ ] **Step 7: Verify against testnet**

```bash
set -a; . ./.env; set +a
cargo run -p bot --release --bin crypto-bot -- testnet
```

Confirm in the logs that startup reports restored protections (zero on a clean start is correct), then:

```bash
sqlite3 data/bot.db "SELECT kind, symbol, detail FROM trade_events ORDER BY at_ms, id LIMIT 20;"
sqlite3 data/bot.db "SELECT symbol, trigger, moved_to_breakeven FROM stop_protections;"
```

- [ ] **Step 8: Commit**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
git add -A && git commit -m "feat(bot): durable stop protections and a full trade audit trail"
```

---

### Task 4: Close the two backtest/live divergences

**Files:**
- Modify: `bot/src/engine_loop.rs`
- Modify: `crates/strategy/src/ict.rs`
- Test: `bot/tests/breakeven_behaviour.rs` (extend)

**Divergence 1 — breakeven trigger sampling.** The simulator applies breakeven using a closed candle's `high`/`low`, after that candle's exits resolve. The live engine samples the ticker last price on a 10s timer. A spike that crosses 2R and retraces between polls moves the stop in the backtest but not live.

**First, read `crates/backtest/src/sim_exchange.rs` and establish which timeframe's candles drive that check.** Match it exactly. Do not guess — if the simulator advances on execution-timeframe (M15) candles, the live check belongs on M15 close using that candle's high/low.

Move the breakeven check to run on execution-candle close using the closed candle's high/low. Keep the escalation ladder on its timer — it reacts to a stop that fired and did not fill, which is a genuinely time-sensitive condition, unlike the breakeven threshold.

**Divergence 2 — warm-up length.** `IctStrategy::warmup_candles()` returns `bias_ema + 50` = 100; the backtest ran with 250. Since `replay.rs` uses `cfg.warmup_candles.max(strategy_warmup)`, raising the declared value to 250 aligns live with the validated run and leaves the backtest at 250 unchanged.

- [ ] **Step 1: Write the failing tests**

```rust
#[tokio::test]
async fn breakeven_arms_from_the_closed_candles_high_not_the_ticker() {
    // Backtest parity: the simulator reads candle.high. A wick that crosses
    // 2R and retraces must still arm, exactly as it does in the backtest.
    let mock = MockExchange::new();
    // last price back at 105 — below 2R — but the candle wicked to 125.
    mock.with_tickers(&[("BTCUSDT", dec!(105))]);
    let mut engine = engine_with_recorded_stop("BTCUSDT", dec!(100), dec!(90), Some(dec!(2)));

    engine.on_candle_closed(&Symbol::new("BTCUSDT"), Timeframe::M15,
        &candle(dec!(105), dec!(125), dec!(104), dec!(105))).await.expect("candle");

    assert_eq!(mock.amended_stops().len(), 1, "the wick crossed 2R");
}

#[tokio::test]
async fn a_candle_that_never_reaches_the_threshold_does_not_arm() {
    let mock = MockExchange::new();
    let mut engine = engine_with_recorded_stop("BTCUSDT", dec!(100), dec!(90), Some(dec!(2)));
    engine.on_candle_closed(&Symbol::new("BTCUSDT"), Timeframe::M15,
        &candle(dec!(105), dec!(119), dec!(104), dec!(105))).await.expect("candle");
    assert!(mock.amended_stops().is_empty());
}

#[test]
fn the_declared_warmup_matches_what_the_strategy_was_validated_with() {
    // replay.rs uses cfg.warmup_candles.max(strategy_warmup); the validated
    // run used 250, so a live bot asking for 100 warms on less history than
    // the numbers were measured with.
    assert_eq!(IctStrategy::new(IctParams::liquidity_sweep_v2()).warmup_candles(), 250);
}
```

Adapt names and the candle helper to the real API. Verify the exact structure of `on_candle_closed` before writing against it.

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p bot breakeven && cargo test -p strategy warmup`
Expected: FAIL.

- [ ] **Step 3: Move the breakeven check to candle close**

- [ ] **Step 4: Raise the declared warm-up**

In `crates/strategy/src/ict.rs`, change `warmup_candles()` to return 250 and update its comment to say why: the daily bias EMA needs margin, and 250 is what the validated backtest used, so live must not warm on less.

- [ ] **Step 5: Confirm the backtest is unchanged**

Run: `cargo test -p backtest --release --test diag_v2_baseline -- --ignored --nocapture`
Expected: **n=298, win 24.2%, PF 1.457, maxDD 17.1%, net 18008** — identical.

If it changed, STOP and report. The declared warm-up must not alter the measured result.

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test --workspace`
Expected: PASS.

- [ ] **Step 7: Commit**

```bash
cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace
git add -A && git commit -m "fix(bot): arm breakeven on candle close and warm on the validated history length"
```

---

### Task 5: Document the durability guarantees

**Files:**
- Modify: `docs/strategies/ict-liquidity-sweep-v2.md`

- [ ] **Step 1: Rewrite the "Known live/backtest divergences" section**

Both divergences are closed by Task 4 — say so, and say what replaced them. Add a "Durability" section covering: what is written when, that the exchange is authoritative about open positions while the journal supplies protection state, how to inspect `trade_events` and `stop_protections` with `sqlite3`, and that a journal write failure is logged but never blocks trading.

State what is still **not** guaranteed: a crash between placing an order and journalling it leaves the exchange ahead of the journal — `reconcile` is what closes that gap, and it adopts from the exchange, not the journal.

- [ ] **Step 2: Commit**

```bash
git add -A && git commit -m "docs: durability guarantees and the closed divergences"
```

---

## Definition of done

- [ ] `cargo test --workspace` green; `cargo clippy --workspace --all-targets -- -D warnings` clean
- [ ] Every protection mutation is written through to the journal as it happens
- [ ] A restart rebuilds protections for positions still open, and drops rows for positions that are not
- [ ] A restored position still moves to breakeven
- [ ] A journal write failure is logged and never stops the engine managing a position
- [ ] `trade_events` records the full lifecycle: placed, filled, stop placed, breakeven, escalation, closed
- [ ] `diag_v2_baseline` still reports n=298 / PF 1.457 / maxDD 17.1% / net 18008
- [ ] The bot starts against testnet and reports restored protections
- [ ] `no_market_orders.rs` still passes
