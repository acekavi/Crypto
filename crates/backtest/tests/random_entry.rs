use backtest::random_entry::{RandomEntryStrategy, percentile, run_benchmark};
use backtest::{BacktestConfig, CostModel};
use botcore::{Candle, Instrument, Side, Symbol, Timeframe};
use history::HistoryDb;
use risk::RiskParams;
use rust_decimal::Decimal;
use rust_decimal_macros::dec;
use strategy::pullback::StrategyParams;
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

/// Prices wander enough for ATR to be non-zero, which the strategy requires
/// before it will size anything.
fn candle(open_time_ms: i64) -> Candle {
    let drift = Decimal::from(open_time_ms / H1 % 5);
    Candle {
        open_time_ms,
        open: dec!(100) + drift,
        high: dec!(104) + drift,
        low: dec!(96) + drift,
        close: dec!(100) + drift,
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

/// Drive a strategy directly over candles and collect what it signalled,
/// without an engine in the way — this isolates the RNG behaviour.
fn signals_from(strat: &mut dyn Strategy, count: i64) -> Vec<Signal> {
    let sym = Symbol::new("BTCUSDT");
    let inst = instrument(&sym);
    let mut out = Vec::new();
    for i in 0..count {
        let c = candle(i * H1);
        let ctx = MarketContext {
            symbol: &sym,
            timeframe: Timeframe::H1,
            candle: &c,
            instrument: &inst,
        };
        if let Some(s) = strat.on_candle_close(&ctx) {
            out.push(s);
        }
    }
    out
}

fn strat(seed: u64, probability: Decimal) -> RandomEntryStrategy {
    RandomEntryStrategy::new(
        seed,
        probability,
        vec![Timeframe::H1],
        0,
        &StrategyParams::defaults(),
    )
}

#[test]
fn the_same_seed_produces_the_same_signals() {
    // Without this the benchmark distribution is not reproducible, and a
    // reported percentile cannot be checked by anyone later.
    let a = signals_from(&mut strat(42, dec!(0.5)), 200);
    let b = signals_from(&mut strat(42, dec!(0.5)), 200);
    assert_eq!(a.len(), b.len());
    for (x, y) in a.iter().zip(b.iter()) {
        assert_eq!(x.signal_candle_open_ms, y.signal_candle_open_ms);
        assert_eq!(x.side, y.side);
        assert_eq!(x.entry_price, y.entry_price);
    }
    assert!(!a.is_empty(), "not a vacuous pass: it really signalled");
}

#[test]
fn different_seeds_produce_different_signals() {
    // If seeds were ignored, the whole distribution would be one run repeated
    // and the 95th percentile would equal the median.
    let a = signals_from(&mut strat(1, dec!(0.5)), 200);
    let b = signals_from(&mut strat(2, dec!(0.5)), 200);
    let ta: Vec<i64> = a.iter().map(|s| s.signal_candle_open_ms).collect();
    let tb: Vec<i64> = b.iter().map(|s| s.signal_candle_open_ms).collect();
    assert_ne!(ta, tb);
}

#[test]
fn a_probability_of_zero_never_signals() {
    assert!(signals_from(&mut strat(7, dec!(0)), 200).is_empty());
}

#[test]
fn a_probability_of_one_signals_on_every_eligible_candle() {
    // Eligible means ATR has warmed. `Atr::update` seeds on its `period`-th
    // call and RETURNS a value on that same call, so exactly `period - 1`
    // leading candles yield None and cannot size a stop.
    let signals = signals_from(&mut strat(7, dec!(1)), 200);
    let blank = StrategyParams::defaults().atr_period as i64 - 1;
    assert_eq!(signals.len() as i64, 200 - blank);

    // And they are contiguous to the end — "every eligible candle" means no
    // holes once ATR is warm, not merely the right total.
    for pair in signals.windows(2) {
        assert_eq!(
            pair[1].signal_candle_open_ms - pair[0].signal_candle_open_ms,
            H1,
            "probability 1 must leave no gaps once ATR has warmed"
        );
    }
}

#[test]
fn both_sides_are_drawn() {
    // A long-only benchmark would measure a market-direction bias rather than
    // an entry edge, and would flatter any strategy tested in a bull sample.
    let signals = signals_from(&mut strat(11, dec!(1)), 400);
    assert!(
        signals.iter().any(|s| s.side == Side::Buy),
        "no long entries drawn"
    );
    assert!(
        signals.iter().any(|s| s.side == Side::Sell),
        "no short entries drawn"
    );
}

#[test]
fn the_stop_and_target_mirror_the_strategy_under_test() {
    // Identical sizing and R:R is what makes the comparison fair — only the
    // timing and direction may differ.
    let p = StrategyParams::defaults();
    let signals = signals_from(&mut strat(3, dec!(1)), 100);
    let s = signals.first().expect("signalled");

    let risk = (s.entry_price - s.stop_price).abs();
    assert_eq!(
        risk,
        s.atr * p.atr_stop_multiple,
        "stop distance must match"
    );
    let reward = (s.target_price - s.entry_price).abs();
    assert_eq!(
        reward,
        risk * p.reward_multiple,
        "reward multiple must match"
    );
}

#[test]
fn percentile_uses_nearest_rank() {
    // 100 values 1..=100. The 95th percentile is the 95th smallest, which is
    // 95 — no interpolation inventing a value no run produced.
    let sorted: Vec<Decimal> = (1..=100).map(Decimal::from).collect();
    assert_eq!(percentile(&sorted, dec!(95)), dec!(95));
    assert_eq!(percentile(&sorted, dec!(50)), dec!(50));
    assert_eq!(percentile(&sorted, dec!(100)), dec!(100));
}

#[test]
fn percentile_handles_small_and_empty_inputs() {
    assert_eq!(percentile(&[], dec!(95)), dec!(0));
    assert_eq!(percentile(&[dec!(5)], dec!(95)), dec!(5));
    // ceil(0.95 * 2) = 2 -> the larger of two.
    assert_eq!(percentile(&[dec!(1), dec!(9)], dec!(95)), dec!(9));
}

#[tokio::test]
async fn the_benchmark_returns_one_expectancy_per_seed_in_seed_order() {
    let (db, _dir) = temp_db().await;
    let sym = Symbol::new("BTCUSDT");
    let candles: Vec<Candle> = (0..80).map(|i| candle(i * H1)).collect();
    db.insert_candles(&sym, Timeframe::H1, &candles)
        .await
        .expect("insert");

    let cfg = BacktestConfig {
        start_ms: 0,
        end_ms: 80 * H1,
        starting_equity: dec!(10000),
        symbols: vec![sym.clone()],
        instruments: vec![instrument(&sym)],
        costs: CostModel {
            maker_fee_rate: dec!(0.0002),
        },
        warmup_candles: 0,
        entry_expiry_candles: 3,
        breakeven_at_r: None,
    };

    let seeds = vec![1u64, 2, 3];
    let dist = run_benchmark(
        &db,
        &cfg,
        &RiskParams::defaults(),
        dec!(0.3),
        &seeds,
        dec!(0.2),
        &StrategyParams::defaults(),
        vec![Timeframe::H1],
        0,
    )
    .await
    .expect("benchmark");

    assert_eq!(dist.seeds, seeds);
    assert_eq!(dist.expectancies.len(), seeds.len());
}
