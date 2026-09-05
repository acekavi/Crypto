use std::collections::HashSet;
use std::sync::{Arc, Mutex};

use backtest::{BacktestConfig, BacktestError, CostModel, ExitReason, run_backtest};
use botcore::{Candle, Instrument, Side, Symbol, Timeframe};
use history::HistoryDb;
use risk::{RiskManager, RiskParams};
use rust_decimal_macros::dec;
use strategy::{MarketContext, Signal, Strategy};

const H1: i64 = 3_600_000;
const H4: i64 = 4 * H1;

fn instrument(sym: &Symbol) -> Instrument {
    Instrument {
        symbol: sym.clone(),
        tick_size: dec!(0.1),
        qty_step: dec!(0.001),
        min_order_qty: dec!(0.001),
        min_notional: dec!(5),
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

/// Per-candle high/low control, open/close fixed at 100 — the same
/// convention `crates/backtest/tests/sim_exchange.rs`'s `candle_at` uses.
/// `wide_candle`'s fixed shape cannot express "narrow everywhere except the
/// one H4 candle that must not be allowed to settle anything," which is
/// exactly what the finest-timeframe tests below need to construct.
fn candle_at(open_time_ms: i64, high: rust_decimal::Decimal, low: rust_decimal::Decimal) -> Candle {
    Candle {
        open_time_ms,
        open: dec!(100),
        high,
        low,
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
        // Matches `PullbackStrategy`: entries are always timed on 1h, H4
        // (when declared) only ever advances a bias filter. Without this
        // gate, a multi-timeframe test's very first call would be an H4
        // candle (H1's own first candle is skipped by `is_stale` until H4
        // has reported at least once — see `CandleStore::is_stale`), and
        // this strategy would signal off it instead of a real H1 close.
        if ctx.timeframe != Timeframe::H1 {
            return None;
        }
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
            breakeven_at_r: None,
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

/// Records the (symbol, timeframe, open_time_ms) of every candle the engine
/// actually evaluated, in the order it evaluated them — the only way to
/// observe the merged stream's real order from outside `run_backtest`.
/// Timeframe is part of the tuple (not just symbol and open time) because an
/// H1 and an H4 candle can share an `open_time_ms`; without it, two records
/// from that shared instant would be indistinguishable from one record seen
/// twice, which would hide a dropped timeframe instead of proving both
/// arrived.
struct RecordingStrategy {
    timeframes: Vec<Timeframe>,
    seen: Arc<Mutex<Vec<(Symbol, Timeframe, i64)>>>,
}

impl Strategy for RecordingStrategy {
    fn timeframes(&self) -> &[Timeframe] {
        &self.timeframes
    }

    fn warmup_candles(&self) -> usize {
        0
    }

    fn on_candle_close(&mut self, ctx: &MarketContext) -> Option<Signal> {
        self.seen.lock().expect("lock").push((
            ctx.symbol.clone(),
            ctx.timeframe,
            ctx.candle.open_time_ms,
        ));
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
    let expected: Vec<(Symbol, Timeframe, i64)> = (0..4)
        .flat_map(|i| {
            [
                (aaa.clone(), Timeframe::H1, i * H1),
                (bbb.clone(), Timeframe::H1, i * H1),
            ]
        })
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

// --- Multi-timeframe coverage -----------------------------------------
//
// `PullbackStrategy` — the strategy Plan 2c actually runs — declares BOTH
// H1 and H4 (see `crates/strategy/src/pullback.rs`), yet every test above
// this point declares H1 alone. The tests below exercise the path the
// module doc's "Settlement only on the finest timeframe" section describes:
// merged H1+H4 replay, with `SimulatedExchange::advance` firing only for H1.

#[tokio::test]
async fn a_position_settles_only_against_h1_candles_when_the_strategy_also_declares_h4() {
    // The core claim: `SimulatedExchange::advance` must never see the H4
    // candle. Prove it by making the wrong behaviour visibly wrong. An H4
    // candle's high/low span its whole 4h window, so at the one instant it
    // shares an `open_time_ms` with an H1 candle, it can show a much wider
    // range than any single H1 candle in that window. If `advance` ever
    // settled against it, a stop no real H1 candle reaches for three more
    // bars would trigger immediately — exactly the look-ahead bias the
    // module doc warns against.
    let (db, _dir) = temp_db().await;
    let sym = Symbol::new("BTCUSDT");

    // H1 grid. Index 0 never reaches the strategy (H4 has not reported yet,
    // so `is_stale` refuses it — see the next test for why); the signal
    // fires on index 1, the resting buy limit (entry 100) fills on index 2
    // (low 95 < 100), then every candle up to index 6 stays inside the
    // 97.7 stop / 110 target band. Only index 7 genuinely trades through
    // the stop (low 97 < 97.7).
    let h1: Vec<Candle> = vec![
        candle_at(0, dec!(101), dec!(99)),
        candle_at(H1, dec!(101), dec!(99)),
        candle_at(2 * H1, dec!(105), dec!(95)), // fills the resting entry
        candle_at(3 * H1, dec!(101), dec!(99)),
        candle_at(4 * H1, dec!(101), dec!(99)), // shares open_time with H4[1]
        candle_at(5 * H1, dec!(101), dec!(99)),
        candle_at(6 * H1, dec!(101), dec!(99)),
        candle_at(7 * H1, dec!(100), dec!(97)), // the real stop-out
    ];
    db.insert_candles(&sym, Timeframe::H1, &h1)
        .await
        .expect("insert H1");

    // H4[0] arrives before any position exists, so its data can never
    // matter regardless of the finest-timeframe rule — it is here only so
    // H4 has "reported" by the time H1's signal candle (index 1) is
    // evaluated. H4[1], at 4*H1, is the one H4 candle that lands while the
    // position is open: its low (90) is far past the 97.7 stop that no real
    // H1 candle reaches before index 7.
    let h4 = vec![
        candle_at(0, dec!(101), dec!(99)),
        candle_at(4 * H1, dec!(101), dec!(90)),
    ];
    db.insert_candles(&sym, Timeframe::H4, &h4)
        .await
        .expect("insert H4");

    let cfg = BacktestConfig {
        start_ms: 0,
        end_ms: 7 * H1,
        starting_equity: dec!(10000),
        symbols: vec![sym.clone()],
        instruments: vec![instrument(&sym)],
        costs: costs(),
        warmup_candles: 0,
        entry_expiry_candles: 3,
    };

    let strategy = SignalOnceStrategy {
        timeframes: vec![Timeframe::H1, Timeframe::H4],
        signaled: HashSet::new(),
    };
    let result = run_backtest(&db, &cfg, Box::new(strategy), risk())
        .await
        .expect("run");

    assert_eq!(result.trades.len(), 1);
    let t = &result.trades[0];
    assert_eq!(t.entry_ms, 2 * H1, "fills the candle after the signal");
    assert_eq!(
        t.exit_ms,
        7 * H1,
        "must resolve on the H1 candle that actually trades through the stop, \
         not on H4[1]'s wide range three candles earlier"
    );
    assert_eq!(t.exit_price, dec!(97.7));
    assert_eq!(t.exit_reason, ExitReason::Stop);
}

#[tokio::test]
async fn h1_and_h4_both_reach_the_strategy_and_h1_is_delivered_first_at_a_shared_open_time() {
    // `engine.on_candle_closed` runs for every declared timeframe — only
    // `SimulatedExchange::advance` is finest-gated (see the module doc). A
    // rule that silently dropped H4 candles before the strategy would leave
    // `PullbackStrategy`'s bias filter permanently blind, yet every other
    // test in this file would still pass, since nothing else observes what
    // the strategy itself was handed.
    let (db, _dir) = temp_db().await;
    let sym = Symbol::new("BTCUSDT");

    let h1: Vec<Candle> = (0..9).map(|i| wide_candle(i * H1)).collect();
    db.insert_candles(&sym, Timeframe::H1, &h1)
        .await
        .expect("insert H1");
    let h4: Vec<Candle> = vec![wide_candle(0), wide_candle(H4), wide_candle(2 * H4)];
    db.insert_candles(&sym, Timeframe::H4, &h4)
        .await
        .expect("insert H4");

    let cfg = BacktestConfig {
        start_ms: 0,
        end_ms: 8 * H1,
        starting_equity: dec!(10000),
        symbols: vec![sym.clone()],
        instruments: vec![instrument(&sym)],
        costs: costs(),
        warmup_candles: 0,
        entry_expiry_candles: 3,
    };

    let seen = Arc::new(Mutex::new(Vec::new()));
    let strategy = RecordingStrategy {
        timeframes: vec![Timeframe::H1, Timeframe::H4],
        seen: seen.clone(),
    };

    run_backtest(&db, &cfg, Box::new(strategy), risk())
        .await
        .expect("run");

    let order = seen.lock().expect("lock").clone();
    // H1[0] (open_time 0) never reaches the strategy: at that instant H4 has
    // never produced a candle, and `CandleStore::is_stale` treats a stream
    // that has never reported as stale by definition, so the whole candle is
    // skipped before `on_candle_close` is called. H4[0], arriving right
    // after at the same open_time_ms, is what first makes H4 "reported" —
    // from then on both streams are fresh enough to pass, so every later
    // shared open_time_ms (H4 at 1*H4 and 2*H4) delivers both candles, H1
    // immediately before H4 per `timeframe_rank`.
    let expected = vec![
        (sym.clone(), Timeframe::H4, 0),
        (sym.clone(), Timeframe::H1, H1),
        (sym.clone(), Timeframe::H1, 2 * H1),
        (sym.clone(), Timeframe::H1, 3 * H1),
        (sym.clone(), Timeframe::H1, H4),
        (sym.clone(), Timeframe::H4, H4),
        (sym.clone(), Timeframe::H1, 5 * H1),
        (sym.clone(), Timeframe::H1, 6 * H1),
        (sym.clone(), Timeframe::H1, 7 * H1),
        (sym.clone(), Timeframe::H1, 2 * H4),
        (sym.clone(), Timeframe::H4, 2 * H4),
    ];
    assert_eq!(order, expected);
}

#[tokio::test]
async fn multi_timeframe_symbol_order_in_the_config_does_not_change_the_result() {
    // Same property as `the_symbol_order_in_the_config_does_not_change_the_result`,
    // but for the shape Plan 2c actually runs: a strategy declaring H1 AND
    // H4. The tie-break is the same code regardless of how many timeframes
    // are declared, but every other determinism test in this file drives it
    // with H1 alone — this is the only one that would catch a tie-break that
    // silently stopped covering H4 ticks (which double the number of
    // same-timestamp collisions the merge has to resolve consistently).
    let (db, _dir) = temp_db().await;
    let aaa = Symbol::new("AAAUSDT");
    let bbb = Symbol::new("BBBUSDT");
    let h1: Vec<Candle> = (0..8).map(|i| wide_candle(i * H1)).collect();
    db.insert_candles(&aaa, Timeframe::H1, &h1)
        .await
        .expect("insert AAA H1");
    db.insert_candles(&bbb, Timeframe::H1, &h1)
        .await
        .expect("insert BBB H1");
    let h4: Vec<Candle> = vec![wide_candle(0), wide_candle(H4)];
    db.insert_candles(&aaa, Timeframe::H4, &h4)
        .await
        .expect("insert AAA H4");
    db.insert_candles(&bbb, Timeframe::H4, &h4)
        .await
        .expect("insert BBB H4");

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

    let make_strategy = || {
        Box::new(SignalOnceStrategy {
            timeframes: vec![Timeframe::H1, Timeframe::H4],
            signaled: HashSet::new(),
        })
    };

    let forward = run_backtest(&db, &base, make_strategy(), risk())
        .await
        .expect("forward run");
    let backward = run_backtest(&db, &reversed, make_strategy(), risk())
        .await
        .expect("reversed run");

    assert_eq!(
        forward, backward,
        "symbol order in the config must not change a multi-timeframe backtest's outcome"
    );
    // Not a vacuous pass: two symbols really did trade.
    assert_eq!(forward.trades.len(), 2);
}
