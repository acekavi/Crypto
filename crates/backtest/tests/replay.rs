use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use backtest::{BacktestConfig, BacktestError, CostModel, ExitReason, run_backtest};
use botcore::{Candle, Instrument, Side, Symbol, Timeframe};
use history::HistoryDb;
use risk::{RiskManager, RiskParams};
use rust_decimal_macros::dec;
use strategy::{MarketContext, Signal, Strategy};

const H1: i64 = 3_600_000;

fn instrument(sym: &Symbol) -> Instrument {
    Instrument {
        symbol: sym.clone(),
        tick_size: dec!(0.1),
        qty_step: dec!(0.001),
        min_order_qty: dec!(0.001),
        launch_time_ms: 0,
    }
}

/// Wide enough that a buy limit resting at 100 trades through on the candle
/// after it is placed (low 95 < 100), and a position opened at 100 stops out
/// at 97.7 without ever reaching a target of 110 (high 105 < 110) — one
/// clean, unambiguous outcome per position, by construction.
fn wide_candle(open_time_ms: i64) -> Candle {
    Candle {
        open_time_ms,
        open: dec!(100),
        high: dec!(105),
        low: dec!(95),
        close: dec!(100),
        volume: dec!(1),
        turnover: dec!(1),
    }
}

async fn temp_db() -> (HistoryDb, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = HistoryDb::open_local(dir.path().join("h.db").to_str().unwrap())
        .await
        .expect("opens");
    (db, dir)
}

fn risk() -> RiskManager {
    // Matches the offset used elsewhere in the workspace's own tests
    // (`bot/tests/engine_loop_behaviour.rs`), not a value this suite invented.
    RiskManager::new(RiskParams::defaults(), dec!(0.3))
}

/// Signals exactly once per symbol — the first candle it is ever called for
/// — then stays silent. `EngineLoop`'s `CandleStore` is what decides WHEN
/// that first call happens (only once `warmup_candles` have been accepted),
/// so this strategy's own state never confounds the warm-up test. Signalling
/// only once also keeps at most one resting order per symbol at a time,
/// which matters because `SimulatedExchange` picks a fillable resting order
/// by iterating a `HashMap` — two concurrent resting orders for the same
/// symbol would make the pick (and therefore determinism) depend on that
/// map's iteration order.
struct SignalOnceStrategy {
    timeframes: Vec<Timeframe>,
    signaled: HashSet<Symbol>,
}

impl SignalOnceStrategy {
    fn new() -> Self {
        SignalOnceStrategy {
            timeframes: vec![Timeframe::H1],
            signaled: HashSet::new(),
        }
    }
}

impl Strategy for SignalOnceStrategy {
    fn timeframes(&self) -> &[Timeframe] {
        &self.timeframes
    }

    fn warmup_candles(&self) -> usize {
        0
    }

    fn on_candle_close(&mut self, ctx: &MarketContext) -> Option<Signal> {
        if !self.signaled.insert(ctx.symbol.clone()) {
            return None;
        }
        Some(Signal {
            symbol: ctx.symbol.clone(),
            side: Side::Buy,
            entry_price: dec!(100),
            stop_price: dec!(98),
            target_price: dec!(110),
            atr: dec!(1),
            signal_candle_open_ms: ctx.candle.open_time_ms,
        })
    }
}

/// Never produces a signal — the negative control. Its only job is to prove
/// that replaying candles through the engine costs nothing when nothing ever
/// trades.
struct NeverSignalStrategy {
    timeframes: Vec<Timeframe>,
}

impl Strategy for NeverSignalStrategy {
    fn timeframes(&self) -> &[Timeframe] {
        &self.timeframes
    }

    fn warmup_candles(&self) -> usize {
        0
    }

    fn on_candle_close(&mut self, _ctx: &MarketContext) -> Option<Signal> {
        None
    }
}

/// Records the (symbol, open_time_ms) of every candle the engine actually
/// evaluated, in the order it evaluated them — the only way to observe the
/// merged stream's real order from outside `run_backtest`.
struct RecordingStrategy {
    timeframes: Vec<Timeframe>,
    seen: Arc<Mutex<Vec<(Symbol, i64)>>>,
}

impl Strategy for RecordingStrategy {
    fn timeframes(&self) -> &[Timeframe] {
        &self.timeframes
    }

    fn warmup_candles(&self) -> usize {
        0
    }

    fn on_candle_close(&mut self, ctx: &MarketContext) -> Option<Signal> {
        self.seen
            .lock()
            .expect("lock")
            .push((ctx.symbol.clone(), ctx.candle.open_time_ms));
        None
    }
}

fn costs() -> CostModel {
    CostModel {
        maker_fee_rate: dec!(0.0002),
    }
}

#[tokio::test]
async fn the_same_inputs_replayed_twice_produce_identical_results() {
    let (db, _dir) = temp_db().await;
    let aaa = Symbol::new("AAAUSDT");
    let bbb = Symbol::new("BBBUSDT");
    let candles: Vec<Candle> = (0..8).map(|i| wide_candle(i * H1)).collect();
    db.insert_candles(&aaa, Timeframe::H1, &candles)
        .await
        .expect("insert AAA");
    db.insert_candles(&bbb, Timeframe::H1, &candles)
        .await
        .expect("insert BBB");

    let cfg = BacktestConfig {
        start_ms: 0,
        end_ms: 7 * H1,
        starting_equity: dec!(10000),
        symbols: vec![aaa.clone(), bbb.clone()],
        instruments: vec![instrument(&aaa), instrument(&bbb)],
        costs: costs(),
        warmup_candles: 5,
        entry_expiry_candles: 3,
    };

    let first = run_backtest(&db, &cfg, Box::new(SignalOnceStrategy::new()), risk())
        .await
        .expect("first run");
    let second = run_backtest(&db, &cfg, Box::new(SignalOnceStrategy::new()), risk())
        .await
        .expect("second run");

    assert_eq!(first, second);
    // Not a vacuous pass: two symbols really did trade.
    assert_eq!(first.trades.len(), 2);
}

#[tokio::test]
async fn candles_from_two_symbols_are_replayed_in_global_time_order_not_grouped_by_symbol() {
    let (db, _dir) = temp_db().await;
    let aaa = Symbol::new("AAAUSDT");
    let bbb = Symbol::new("BBBUSDT");
    // Both symbols trade on the same H1 grid, as real markets do — the
    // property under test is the tie-break, not artificial time gaps. A
    // symbol-by-symbol replay would produce AAA's whole run before BBB's
    // first candle; this asserts the actual interleaved order instead.
    let candles: Vec<Candle> = (0..4).map(|i| wide_candle(i * H1)).collect();
    db.insert_candles(&aaa, Timeframe::H1, &candles)
        .await
        .expect("insert AAA");
    db.insert_candles(&bbb, Timeframe::H1, &candles)
        .await
        .expect("insert BBB");

    let cfg = BacktestConfig {
        start_ms: 0,
        end_ms: 3 * H1,
        starting_equity: dec!(10000),
        symbols: vec![aaa.clone(), bbb.clone()],
        instruments: vec![instrument(&aaa), instrument(&bbb)],
        costs: costs(),
        warmup_candles: 1,
        entry_expiry_candles: 3,
    };

    let seen = Arc::new(Mutex::new(Vec::new()));
    let strategy = RecordingStrategy {
        timeframes: vec![Timeframe::H1],
        seen: seen.clone(),
    };

    run_backtest(&db, &cfg, Box::new(strategy), risk())
        .await
        .expect("run");

    let order = seen.lock().expect("lock").clone();
    let expected: Vec<(Symbol, i64)> = (0..4)
        .flat_map(|i| [(aaa.clone(), i * H1), (bbb.clone(), i * H1)])
        .collect();
    assert_eq!(
        order, expected,
        "expected time-major order with symbol-name ties, not symbol-major order"
    );
}

#[tokio::test]
async fn no_trade_opens_before_warmup_candles_have_been_fed() {
    let (db, _dir) = temp_db().await;
    let sym = Symbol::new("BTCUSDT");
    // 8 candles; the store only becomes warm on the 5th (index 4).
    let candles: Vec<Candle> = (0..8).map(|i| wide_candle(i * H1)).collect();
    db.insert_candles(&sym, Timeframe::H1, &candles)
        .await
        .expect("insert");

    let cfg = BacktestConfig {
        start_ms: 0,
        end_ms: 7 * H1,
        starting_equity: dec!(10000),
        symbols: vec![sym.clone()],
        instruments: vec![instrument(&sym)],
        costs: costs(),
        warmup_candles: 5,
        entry_expiry_candles: 3,
    };

    let result = run_backtest(&db, &cfg, Box::new(SignalOnceStrategy::new()), risk())
        .await
        .expect("run");

    // The strategy is only ever invoked from candle index 4 onward (the
    // first `is_warm` tick), so the earliest a resting entry can exist is
    // index 4, the earliest it can fill is index 5 (never same-bar), and the
    // earliest it can close is index 6 — never earlier, by construction.
    assert_eq!(result.trades.len(), 1);
    let t = &result.trades[0];
    assert_eq!(
        t.entry_ms,
        5 * H1,
        "fills the candle after the signal, not before warm-up"
    );
    assert_eq!(t.exit_ms, 6 * H1);
    assert_eq!(t.exit_reason, ExitReason::Stop);
}

#[tokio::test]
async fn a_strategy_that_never_signals_produces_zero_trades_and_exactly_starting_equity() {
    let (db, _dir) = temp_db().await;
    let sym = Symbol::new("BTCUSDT");
    let candles: Vec<Candle> = (0..10).map(|i| wide_candle(i * H1)).collect();
    db.insert_candles(&sym, Timeframe::H1, &candles)
        .await
        .expect("insert");

    let cfg = BacktestConfig {
        start_ms: 0,
        end_ms: 9 * H1,
        starting_equity: dec!(10000),
        symbols: vec![sym.clone()],
        instruments: vec![instrument(&sym)],
        costs: costs(),
        warmup_candles: 3,
        entry_expiry_candles: 3,
    };

    let strategy = NeverSignalStrategy {
        timeframes: vec![Timeframe::H1],
    };
    let result = run_backtest(&db, &cfg, Box::new(strategy), risk())
        .await
        .expect("run");

    assert!(result.trades.is_empty());
    assert_eq!(result.ambiguous_exits, 0);
    // Exactly, not approximately: no fill, no fee, no funding ever touched
    // equity, so this must hold bit-for-bit against Decimal equality.
    assert_eq!(result.final_equity, dec!(10000));
    assert_eq!(result.candles_replayed, 10);
}

#[tokio::test]
async fn a_gap_in_stored_data_refuses_the_run_instead_of_replaying_across_it() {
    let (db, _dir) = temp_db().await;
    let sym = Symbol::new("BTCUSDT");
    // Candle at 2*H1 is missing.
    let candles = vec![wide_candle(0), wide_candle(H1), wide_candle(3 * H1)];
    db.insert_candles(&sym, Timeframe::H1, &candles)
        .await
        .expect("insert");

    let cfg = BacktestConfig {
        start_ms: 0,
        end_ms: 3 * H1,
        starting_equity: dec!(10000),
        symbols: vec![sym.clone()],
        instruments: vec![instrument(&sym)],
        costs: costs(),
        warmup_candles: 1,
        entry_expiry_candles: 3,
    };

    let strategy = NeverSignalStrategy {
        timeframes: vec![Timeframe::H1],
    };
    let err = run_backtest(&db, &cfg, Box::new(strategy), risk())
        .await
        .expect_err("a gap must refuse the run");

    match err {
        BacktestError::GapInData {
            symbol,
            from_ms,
            to_ms,
        } => {
            assert_eq!(symbol, sym);
            assert_eq!(from_ms, H1);
            assert_eq!(to_ms, 3 * H1);
        }
        other => panic!("expected GapInData, got {other:?}"),
    }
}

#[tokio::test]
async fn the_symbol_order_in_the_config_does_not_change_the_result() {
    // The existing determinism test runs the SAME config twice, which passes
    // even without a tie-break: ticks are built by walking `cfg.symbols` (a
    // Vec) and Rust's sort is stable, so the input order survives untouched.
    // That makes the tie-break defensive rather than load-bearing, and an
    // untested guard is one a later refactor can quietly delete.
    //
    // Listing the same two symbols in the opposite order is what actually
    // exercises it: both trade on the same H1 grid, so every candle is a tie,
    // and only an explicit symbol-name comparison can put them back in the
    // same sequence. Without it the two runs interleave differently and the
    // global daily-entry and position caps see a different timeline.
    let (db, _dir) = temp_db().await;
    let aaa = Symbol::new("AAAUSDT");
    let bbb = Symbol::new("BBBUSDT");
    let candles: Vec<Candle> = (0..8).map(|i| wide_candle(i * H1)).collect();
    db.insert_candles(&aaa, Timeframe::H1, &candles)
        .await
        .expect("insert AAA");
    db.insert_candles(&bbb, Timeframe::H1, &candles)
        .await
        .expect("insert BBB");

    let base = BacktestConfig {
        start_ms: 0,
        end_ms: 7 * H1,
        starting_equity: dec!(10000),
        symbols: vec![aaa.clone(), bbb.clone()],
        instruments: vec![instrument(&aaa), instrument(&bbb)],
        costs: costs(),
        warmup_candles: 5,
        entry_expiry_candles: 3,
    };
    let reversed = BacktestConfig {
        symbols: vec![bbb.clone(), aaa.clone()],
        instruments: vec![instrument(&bbb), instrument(&aaa)],
        ..base.clone()
    };

    let forward = run_backtest(&db, &base, Box::new(SignalOnceStrategy::new()), risk())
        .await
        .expect("forward run");
    let backward = run_backtest(&db, &reversed, Box::new(SignalOnceStrategy::new()), risk())
        .await
        .expect("reversed run");

    assert_eq!(
        forward, backward,
        "symbol order in the config must not change a backtest's outcome"
    );
    // Not a vacuous pass: two symbols really did trade.
    assert_eq!(forward.trades.len(), 2);
}
