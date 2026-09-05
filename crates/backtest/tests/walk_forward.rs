use std::collections::HashSet;

use backtest::walk_forward::{WalkForwardConfig, folds, run_walk_forward};
use backtest::{BacktestConfig, CostModel};
use botcore::{Candle, Instrument, Side, Symbol, Timeframe};
use history::HistoryDb;
use risk::RiskParams;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use strategy::pullback::StrategyParams;
use strategy::{MarketContext, Signal, Strategy};

const H1: i64 = 3_600_000;
const DAY: i64 = 86_400_000;

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

async fn temp_db() -> (HistoryDb, tempfile::TempDir) {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = HistoryDb::open_local(dir.path().join("h.db").to_str().unwrap())
        .await
        .expect("opens");
    (db, dir)
}

/// A candle that both fills a buy limit resting at 100 and then stops it out
/// at 97.7 — a losing round trip, by construction.
fn losing_candle(open_time_ms: i64) -> Candle {
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

/// Fills a buy limit at 100 and runs to a target of 110 without touching the
/// stop — a winning round trip.
fn winning_candle(open_time_ms: i64) -> Candle {
    Candle {
        open_time_ms,
        open: dec!(100),
        high: dec!(115),
        low: dec!(99),
        close: dec!(100),
        volume: dec!(1),
        turnover: dec!(1),
    }
}

fn params(tag: usize) -> StrategyParams {
    // `ema_fast` is used purely as an identity tag here: the harness is being
    // tested on WHICH grid entry it selects, not on what any parameter means.
    StrategyParams {
        ema_fast: tag,
        ..StrategyParams::defaults()
    }
}

/// Signals once per symbol, but only inside a time window keyed to the
/// params it was built with.
///
/// This is what lets a test make one grid entry the clear in-sample winner
/// and a DIFFERENT one the clear out-of-sample winner — the only way to prove
/// the harness tunes on in-sample data and does not peek ahead.
struct TaggedStrategy {
    timeframes: Vec<Timeframe>,
    signaled: HashSet<Symbol>,
    /// Signals only when the candle's open time falls in `[from, to)`.
    from_ms: i64,
    to_ms: i64,
}

impl TaggedStrategy {
    fn for_params(p: &StrategyParams, windows: &[(usize, i64, i64)]) -> Self {
        let (from_ms, to_ms) = windows
            .iter()
            .find(|(tag, _, _)| *tag == p.ema_fast)
            .map(|(_, f, t)| (*f, *t))
            // A tag with no declared window never signals.
            .unwrap_or((i64::MAX, i64::MAX));
        TaggedStrategy {
            timeframes: vec![Timeframe::H1],
            signaled: HashSet::new(),
            from_ms,
            to_ms,
        }
    }
}

impl Strategy for TaggedStrategy {
    fn timeframes(&self) -> &[Timeframe] {
        &self.timeframes
    }

    fn warmup_candles(&self) -> usize {
        0
    }

    fn on_candle_close(&mut self, ctx: &MarketContext) -> Option<Signal> {
        if ctx.timeframe != Timeframe::H1 {
            return None;
        }
        let t = ctx.candle.open_time_ms;
        if t < self.from_ms || t >= self.to_ms {
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
            signal_candle_open_ms: t,
            breakeven_at_r: None,
        })
    }
}

fn base_cfg(sym: &Symbol, start_ms: i64, end_ms: i64) -> BacktestConfig {
    BacktestConfig {
        start_ms,
        end_ms,
        starting_equity: dec!(10000),
        symbols: vec![sym.clone()],
        instruments: vec![instrument(sym)],
        costs: CostModel {
            maker_fee_rate: dec!(0.0002),
        },
        warmup_candles: 0,
        entry_expiry_candles: 3,
    }
}

// ---------------------------------------------------------------- folds ----

#[test]
fn eight_months_with_six_two_windows_produces_exactly_one_fold() {
    let wf = WalkForwardConfig::defaults();
    let f = folds(0, 240 * DAY, &wf);
    assert_eq!(f.len(), 1);
    assert_eq!(f[0].is_start_ms, 0);
    assert_eq!(f[0].is_end_ms, 180 * DAY);
    assert_eq!(f[0].oos_start_ms, 180 * DAY);
    assert_eq!(f[0].oos_end_ms, 240 * DAY);
}

#[test]
fn folds_roll_by_the_out_of_sample_length_so_oos_windows_are_contiguous() {
    // Overlapping out-of-sample windows would count the same trades more than
    // once and inflate the very trade count the gate checks.
    let wf = WalkForwardConfig::defaults();
    // 12 months of 30-day months. The first fold consumes 6+2=8 months; each
    // further fold needs only 2 more, so the remaining 4 months yield 2 more
    // folds. Three in total, not four — the fourth would need its full
    // in-sample AND out-of-sample to fit, and it does not.
    let f = folds(0, 360 * DAY, &wf);
    assert_eq!(f.len(), 3, "12 months of 30-day months, rolling by 2");

    for pair in f.windows(2) {
        assert_eq!(
            pair[0].oos_end_ms, pair[1].oos_start_ms,
            "out-of-sample windows must tile without gaps or overlap"
        );
    }
}

#[test]
fn a_range_too_short_for_a_full_fold_produces_none() {
    // A truncated final fold is a shorter, noisier sample masquerading as a
    // complete one, so it is not emitted at all.
    let wf = WalkForwardConfig::defaults();
    assert!(folds(0, 239 * DAY, &wf).is_empty());
}

#[test]
fn fold_indices_are_sequential_from_zero() {
    let wf = WalkForwardConfig::defaults();
    let f = folds(0, 360 * DAY, &wf);
    let idx: Vec<usize> = f.iter().map(|x| x.index).collect();
    assert_eq!(idx, vec![0, 1, 2]);
}

// -------------------------------------------------------- run_walk_forward --

/// A tiny 6h/4h walk-forward so a test database stays small. The harness does
/// not care what the window lengths mean, only that it rolls by the
/// out-of-sample length.
///
/// Four out-of-sample candles is the minimum that can produce a CLOSED trade:
/// one to signal, one for the resting limit to trade through, and one for the
/// position to reach its stop. A shorter window leaves the position open at
/// the end of the run and yields no `ClosedTrade` at all.
fn tiny_wf() -> WalkForwardConfig {
    WalkForwardConfig {
        in_sample_ms: 6 * H1,
        out_of_sample_ms: 4 * H1,
    }
}

#[tokio::test]
async fn the_chosen_parameters_come_from_in_sample_performance_only() {
    // THE TEST THAT MATTERS. Grid entry 10 wins in-sample and loses
    // out-of-sample; entry 20 does the reverse. A harness that peeked at
    // out-of-sample data would pick 20. Picking 10 — and then carrying 10's
    // POOR out-of-sample result into the evidence — is what proves it did not.
    let (db, _dir) = temp_db().await;
    let sym = Symbol::new("BTCUSDT");

    // Candles 0..5 are in-sample, 6..9 out-of-sample.
    // In-sample: winners. Out-of-sample: losers.
    let mut candles: Vec<Candle> = (0..6).map(|i| winning_candle(i * H1)).collect();
    candles.extend((6..10).map(|i| losing_candle(i * H1)));
    db.insert_candles(&sym, Timeframe::H1, &candles)
        .await
        .expect("insert");

    // Tag 10 signals in-sample only; tag 20 signals out-of-sample only. So
    // in-sample, 10 trades (and wins) while 20 does nothing at all.
    let windows = vec![(10usize, 0i64, 6 * H1), (20usize, 6 * H1, 10 * H1)];
    let grid = vec![params(10), params(20)];

    let cfg = base_cfg(&sym, 0, 10 * H1);
    let result = run_walk_forward(
        &db,
        &cfg,
        &tiny_wf(),
        &grid,
        &RiskParams::defaults(),
        dec!(0.3),
        &|p| Box::new(TaggedStrategy::for_params(p, &windows)),
    )
    .await
    .expect("walk forward");

    assert_eq!(result.folds.len(), 1);
    assert_eq!(
        result.folds[0].chosen_params.ema_fast, 10,
        "must choose the in-sample winner, not the out-of-sample one"
    );
    assert!(
        result.folds[0].is_metrics.expectancy > Decimal::ZERO,
        "the in-sample winner really did win in-sample"
    );
    // Tag 10 never signals out-of-sample, so the frozen choice produces no
    // out-of-sample trades. A peeking harness would have picked 20 and shown
    // trades here instead.
    assert_eq!(result.oos_trades.len(), 0);
}

#[tokio::test]
async fn out_of_sample_trades_are_concatenated_and_drive_the_reported_metrics() {
    let (db, _dir) = temp_db().await;
    let sym = Symbol::new("BTCUSDT");

    let candles: Vec<Candle> = (0..10).map(|i| losing_candle(i * H1)).collect();
    db.insert_candles(&sym, Timeframe::H1, &candles)
        .await
        .expect("insert");

    // One tag that signals across the whole range, so the out-of-sample
    // window genuinely trades.
    let windows = vec![(10usize, 0i64, 10 * H1)];
    let grid = vec![params(10)];

    let cfg = base_cfg(&sym, 0, 10 * H1);
    let result = run_walk_forward(
        &db,
        &cfg,
        &tiny_wf(),
        &grid,
        &RiskParams::defaults(),
        dec!(0.3),
        &|p| Box::new(TaggedStrategy::for_params(p, &windows)),
    )
    .await
    .expect("walk forward");

    assert_eq!(
        result.oos_metrics.trade_count,
        result.oos_trades.len(),
        "reported metrics must be computed over exactly the concatenated OOS trades"
    );
    assert!(
        !result.oos_trades.is_empty(),
        "not a vacuous pass: the OOS window really traded"
    );
}

#[tokio::test]
async fn the_same_inputs_produce_an_identical_walk_forward_result() {
    // Every comparison the gate makes rests on this.
    let (db, _dir) = temp_db().await;
    let sym = Symbol::new("BTCUSDT");
    let candles: Vec<Candle> = (0..10).map(|i| losing_candle(i * H1)).collect();
    db.insert_candles(&sym, Timeframe::H1, &candles)
        .await
        .expect("insert");

    let windows = vec![(10usize, 0i64, 10 * H1)];
    let grid = vec![params(10), params(20)];
    let cfg = base_cfg(&sym, 0, 10 * H1);

    let run = || async {
        run_walk_forward(
            &db,
            &cfg,
            &tiny_wf(),
            &grid,
            &RiskParams::defaults(),
            dec!(0.3),
            &|p| Box::new(TaggedStrategy::for_params(p, &windows)),
        )
        .await
        .expect("walk forward")
    };

    assert_eq!(run().await, run().await);
}

#[tokio::test]
async fn a_tie_in_sample_keeps_the_earlier_grid_entry() {
    // Both entries behave identically, so expectancy ties. The choice must
    // fall out of the grid's declared order rather than of iteration
    // incidentals, or two identical runs could disagree.
    let (db, _dir) = temp_db().await;
    let sym = Symbol::new("BTCUSDT");
    let candles: Vec<Candle> = (0..10).map(|i| losing_candle(i * H1)).collect();
    db.insert_candles(&sym, Timeframe::H1, &candles)
        .await
        .expect("insert");

    let windows = vec![(10usize, 0i64, 10 * H1), (20usize, 0i64, 10 * H1)];
    let cfg = base_cfg(&sym, 0, 10 * H1);

    let forward = run_walk_forward(
        &db,
        &cfg,
        &tiny_wf(),
        &[params(10), params(20)],
        &RiskParams::defaults(),
        dec!(0.3),
        &|p| Box::new(TaggedStrategy::for_params(p, &windows)),
    )
    .await
    .expect("walk forward");
    assert_eq!(forward.folds[0].chosen_params.ema_fast, 10);

    let reversed = run_walk_forward(
        &db,
        &cfg,
        &tiny_wf(),
        &[params(20), params(10)],
        &RiskParams::defaults(),
        dec!(0.3),
        &|p| Box::new(TaggedStrategy::for_params(p, &windows)),
    )
    .await
    .expect("walk forward");
    assert_eq!(
        reversed.folds[0].chosen_params.ema_fast, 20,
        "the tie follows the declared order, so reversing the grid reverses the pick"
    );
}

#[tokio::test]
async fn the_reported_drawdown_is_the_worst_fold_not_the_concatenation() {
    // Concatenating every fold's trades onto ONE starting equity produces a
    // curve that never existed: each fold really began at that equity
    // independently. On a losing strategy the synthetic curve runs negative
    // and reports drawdowns above 100% — 137% and 218% were both produced
    // this way. A number that cannot be true must not reach the gate.
    let (db, _dir) = temp_db().await;
    let sym = Symbol::new("BTCUSDT");
    // Enough history for several folds, all losing.
    let candles: Vec<Candle> = (0..40).map(|i| losing_candle(i * H1)).collect();
    db.insert_candles(&sym, Timeframe::H1, &candles)
        .await
        .expect("insert");

    let windows = vec![(10usize, 0i64, 40 * H1)];
    let grid = vec![params(10)];
    let cfg = base_cfg(&sym, 0, 40 * H1);

    let result = run_walk_forward(
        &db,
        &cfg,
        &tiny_wf(),
        &grid,
        &RiskParams::defaults(),
        dec!(0.3),
        &|p| Box::new(TaggedStrategy::for_params(p, &windows)),
    )
    .await
    .expect("walk forward");

    assert!(result.folds.len() > 1, "need several folds to concatenate");

    let worst_fold = result
        .folds
        .iter()
        .map(|f| f.oos_metrics.max_drawdown_pct)
        .max()
        .expect("folds");
    assert_eq!(
        result.oos_metrics.max_drawdown_pct, worst_fold,
        "the reported drawdown must be the worst single fold"
    );
    // And it must be a figure that can actually happen.
    assert!(
        result.oos_metrics.max_drawdown_pct <= Decimal::ONE_HUNDRED,
        "a drawdown above 100% means equity went negative, which no real \
         account trajectory does: got {}",
        result.oos_metrics.max_drawdown_pct
    );
}
