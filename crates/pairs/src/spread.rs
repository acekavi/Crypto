use std::collections::VecDeque;

/// Floor applied to the variance before the square root.
///
/// Mirrors `max(var, 1e-12)` in the Python. Without it a window of identical
/// prices gives `sd == 0` and every z becomes infinite — which on a pair whose
/// legs are briefly quoted flat would fire an entry signal on noise.
const VAR_FLOOR: f64 = 1e-12;

/// Rolling mean, standard deviation and z-score over a fixed window that
/// **excludes** the value being scored.
///
/// The exclusion is the whole design: scoring a value against a window that
/// contains it leaks the present into the statistic and makes every backtest
/// optimistic. `push` therefore computes against what came before, then folds
/// the value in.
///
/// O(1) per push. The Python it replaces re-summed the entire window every bar
/// (O(n·window)), which is 12.5 s of CPU per dashboard refresh at this
/// project's data sizes.
#[derive(Debug, Clone)]
pub struct RollingZ {
    window: usize,
    buf: VecDeque<f64>,
    /// Values are accumulated as `x - offset` rather than as `x`.
    ///
    /// The variance is computed as `E[x²] - E[x]²`, and log-spreads sit far
    /// from zero (AAVE/ETH runs near -2.3) while their standard deviation is
    /// around 0.02. Subtracting two numbers near 5.29 to recover 4e-4 throws
    /// away most of the available precision. Centring first keeps both terms
    /// small, so the incremental form stays as accurate as a full recompute.
    offset: f64,
    sum: f64,
    sumsq: f64,
    /// Pushes since the accumulators were last rebuilt exactly from `buf`.
    /// Bounds accumulated rounding drift to one window's worth of updates at
    /// a cost that amortises to O(1).
    since_rebuild: usize,
}

/// The statistics of one window, plus the score of the value that was pushed
/// against them.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Stats {
    pub mean: f64,
    pub sd: f64,
    pub z: f64,
}

impl RollingZ {
    /// # Panics
    /// If `window` is zero. A zero window is a programming error, not a
    /// runtime condition — the Python raised `ValueError` from the same place.
    pub fn new(window: usize) -> Self {
        assert!(window > 0, "rolling window must be positive");
        Self {
            window,
            buf: VecDeque::with_capacity(window + 1),
            offset: 0.0,
            sum: 0.0,
            sumsq: 0.0,
            since_rebuild: 0,
        }
    }

    pub fn window(&self) -> usize {
        self.window
    }

    /// Whether a full window has accumulated, so the next push will score.
    pub fn is_warm(&self) -> bool {
        self.buf.len() >= self.window
    }

    /// Score `value` against the preceding window, then admit it to the window.
    ///
    /// Returns `None` until `window` values have been seen — the caller must
    /// treat that as "no opinion", never as a zero score.
    pub fn push(&mut self, value: f64) -> Option<Stats> {
        let stats = self.stats_for(value);

        self.buf.push_back(value);
        let d = value - self.offset;
        self.sum += d;
        self.sumsq += d * d;

        if self.buf.len() > self.window {
            let old = self.buf.pop_front().expect("buffer is non-empty");
            let d = old - self.offset;
            self.sum -= d;
            self.sumsq -= d * d;
        }

        self.since_rebuild += 1;
        if self.since_rebuild >= self.window {
            self.rebuild();
        }

        stats
    }

    fn stats_for(&self, value: f64) -> Option<Stats> {
        if self.buf.len() < self.window {
            return None;
        }
        let n = self.window as f64;
        let centred_mean = self.sum / n;
        let mean = self.offset + centred_mean;
        // Population variance (/ n), matching the Python. Using the sample
        // form (/ (n-1)) would shift every z by a factor of sqrt(n/(n-1)) and
        // silently retune every entry threshold.
        let var = (self.sumsq / n - centred_mean * centred_mean).max(VAR_FLOOR);
        let sd = var.sqrt();
        Some(Stats {
            mean,
            sd,
            z: (value - mean) / sd,
        })
    }

    /// Recompute the accumulators exactly from the buffer and re-centre.
    fn rebuild(&mut self) {
        self.offset = self.buf.front().copied().unwrap_or(0.0);
        self.sum = 0.0;
        self.sumsq = 0.0;
        for &x in &self.buf {
            let d = x - self.offset;
            self.sum += d;
            self.sumsq += d * d;
        }
        self.since_rebuild = 0;
    }
}

/// The pair spread: `ln(a) - ln(b)`.
///
/// A log spread rather than a ratio or a difference because it makes the two
/// legs' percentage moves additive, which is what lets an equal-notional pair
/// be scored with a single number.
pub fn log_spread(a: f64, b: f64) -> f64 {
    a.ln() - b.ln()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact series the Python reference was evaluated on, so the golden
    /// numbers below are comparable rather than merely plausible. Generated by
    /// `math.sin(i/7)*0.03 + math.cos(i/13)*0.02 + 0.5`.
    fn golden_series() -> Vec<f64> {
        (0..40)
            .map(|i| {
                let i = i as f64;
                (i / 7.0).sin() * 0.03 + (i / 13.0).cos() * 0.02 + 0.5
            })
            .collect()
    }

    #[test]
    fn no_score_is_produced_until_a_full_window_precedes_the_value() {
        let mut r = RollingZ::new(8);
        for v in golden_series().iter().take(8) {
            assert_eq!(r.push(*v), None);
        }
        assert!(r.push(golden_series()[8]).is_some());
    }

    #[test]
    fn scores_match_the_python_reference_to_within_one_part_in_a_billion() {
        // Captured from scripts/pairs_bot.py rolling_zscores(values, 8).
        let expected = [
            1.473716198484,
            1.322946909167,
            1.106905206937,
            0.759560999308,
            0.102115965891,
            -1.394626742276,
        ];
        let mut r = RollingZ::new(8);
        let mut got = Vec::new();
        for v in golden_series() {
            if let Some(s) = r.push(v) {
                got.push(s.z);
            }
        }
        for (i, want) in expected.iter().enumerate() {
            assert!(
                (got[i] - want).abs() < 1e-9,
                "z[{}]: got {}, want {}",
                i + 8,
                got[i],
                want
            );
        }
    }

    #[test]
    fn mean_and_sd_match_the_python_reference() {
        // rolling_mean_stddev(values, 8) at i == 8.
        let mut r = RollingZ::new(8);
        let series = golden_series();
        let mut first = None;
        for v in series {
            if let Some(s) = r.push(v) {
                first = Some(s);
                break;
            }
        }
        let s = first.expect("a window's worth of values was pushed");
        assert!((s.mean - 0.532605712195).abs() < 1e-9, "mean was {}", s.mean);
        assert!((s.sd - 0.007477698075).abs() < 1e-9, "sd was {}", s.sd);
    }

    #[test]
    fn the_incremental_accumulator_never_drifts_from_an_exact_recomputation() {
        // The whole point of the O(1) form is that it stays exact. A long run
        // over a large-mean, small-variance series is where sum-of-squares
        // cancellation would show up if the offset trick were dropped.
        let window = 240;
        let series: Vec<f64> = (0..5000)
            .map(|i| {
                let i = i as f64;
                -std::f64::consts::LN_10 + (i / 31.0).sin() * 0.004
            })
            .collect();
        let mut r = RollingZ::new(window);
        for (i, v) in series.iter().enumerate() {
            let got = r.push(*v);
            if i < window {
                continue;
            }
            let hist = &series[i - window..i];
            let mean = hist.iter().sum::<f64>() / window as f64;
            let var = hist.iter().map(|x| (x - mean).powi(2)).sum::<f64>() / window as f64;
            let sd = var.max(1e-12).sqrt();
            let want_z = (v - mean) / sd;
            let got = got.expect("window is full");
            assert!(
                (got.z - want_z).abs() < 1e-9,
                "drift at i={i}: got {}, want {want_z}",
                got.z
            );
        }
    }

    #[test]
    fn a_flat_window_uses_the_variance_floor_instead_of_dividing_by_zero() {
        // Python clamps with max(var, 1e-12) before the sqrt; a constant
        // series would otherwise produce sd == 0 and an infinite z.
        let mut r = RollingZ::new(4);
        for _ in 0..4 {
            r.push(1.0);
        }
        let s = r.push(1.000_001).expect("window is full");
        assert!(s.z.is_finite(), "z was {}", s.z);
        assert!((s.sd - 1e-6).abs() < 1e-9, "sd was {}", s.sd);
    }

    #[test]
    fn log_spread_is_the_difference_of_natural_logs() {
        // Captured from scripts/pairs_bot.py spread_series.
        assert!((log_spread(10.0, 2.0) - 1.609437912434).abs() < 1e-9);
        assert!((log_spread(11.0, 2.5) - 1.481604540924).abs() < 1e-9);
        assert!((log_spread(12.0, 2.4) - 1.609437912434).abs() < 1e-9);
    }
}
