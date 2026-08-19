//! A volume profile built from OHLCV alone.
//!
//! A real volume profile needs volume distributed ACROSS PRICE within each
//! candle. `data/history.db` stores one total volume figure per candle, no
//! intra-candle distribution — this is therefore a PROXY: each candle's
//! volume is spread evenly across the fixed-width buckets its own high-low
//! range spans. Considered and declined once already as a standalone
//! strategy for exactly this reason (see `docs/strategies/ict-h4-sweep-m15-fvg.md`,
//! "Volume profile — considered and skipped"); built now as a confluence at
//! the owner's explicit request, scoped to evaluate only at candidate entry
//! candles rather than continuously, so the weaker signal costs little and
//! is declared rather than hidden.

use botcore::Candle;
use rust_decimal::Decimal;
use rust_decimal::prelude::ToPrimitive;

pub struct VolumeProfile {
    /// Point of control — the price bucket with the most volume.
    pub poc: Decimal,
    pub value_area_low: Decimal,
    pub value_area_high: Decimal,
}

impl VolumeProfile {
    pub fn contains(&self, price: Decimal) -> bool {
        price >= self.value_area_low && price <= self.value_area_high
    }
}

/// Build a profile from `candles` (any order), splitting each candle's
/// volume evenly across however many of `buckets` FIXED-WIDTH price levels
/// — spanning the whole window's [low, high], not each candle's own range —
/// its high-low straddles. A fixed grid keeps volume comparable across
/// candles of different range; a per-candle grid would not.
///
/// Value area follows the standard construction: expand outward from the
/// POC bucket, always taking whichever neighbour holds more volume, until at
/// least 70% of total volume is included.
pub fn build(candles: &[Candle], buckets: usize) -> Option<VolumeProfile> {
    if candles.is_empty() || buckets == 0 {
        return None;
    }
    let lo = candles.iter().map(|c| c.low).min()?;
    let hi = candles.iter().map(|c| c.high).max()?;
    if hi <= lo {
        return None;
    }
    let step = (hi - lo) / Decimal::from(buckets);
    if step <= Decimal::ZERO {
        return None;
    }
    let bucket_of = |price: Decimal| -> usize {
        ((price - lo) / step)
            .floor()
            .to_usize()
            .unwrap_or(0)
            .min(buckets - 1)
    };

    let mut vols = vec![Decimal::ZERO; buckets];
    for c in candles {
        let start = bucket_of(c.low);
        let end = bucket_of(c.high).max(start);
        let span = Decimal::from(end - start + 1);
        let per_bucket = c.volume / span;
        for slot in vols.iter_mut().take(end + 1).skip(start) {
            *slot += per_bucket;
        }
    }
    let total: Decimal = vols.iter().sum();
    if total <= Decimal::ZERO {
        return None;
    }
    let poc_idx = vols
        .iter()
        .enumerate()
        .max_by_key(|(_, v)| **v)
        .map(|(i, _)| i)?;

    let target = total * Decimal::new(70, 2); // 70% of volume
    let mut included = vols[poc_idx];
    let (mut lo_i, mut hi_i) = (poc_idx, poc_idx);
    while included < target && (lo_i > 0 || hi_i < buckets - 1) {
        let lo_vol = if lo_i > 0 {
            vols[lo_i - 1]
        } else {
            Decimal::ZERO
        };
        let hi_vol = if hi_i < buckets - 1 {
            vols[hi_i + 1]
        } else {
            Decimal::ZERO
        };
        if lo_i > 0 && (hi_i >= buckets - 1 || lo_vol >= hi_vol) {
            lo_i -= 1;
            included += lo_vol;
        } else {
            hi_i += 1;
            included += hi_vol;
        }
    }

    Some(VolumeProfile {
        poc: lo + step * Decimal::from(poc_idx) + step / Decimal::TWO,
        value_area_low: lo + step * Decimal::from(lo_i),
        value_area_high: lo + step * Decimal::from(hi_i + 1),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rust_decimal_macros::dec;

    fn c(low: Decimal, high: Decimal, volume: Decimal) -> Candle {
        Candle {
            open_time_ms: 0,
            open: low,
            high,
            low,
            close: high,
            volume,
            turnover: Decimal::ZERO,
        }
    }

    #[test]
    fn poc_lands_where_volume_concentrates() {
        // Heavy volume clustered near the top of the window.
        let candles = vec![
            c(dec!(100), dec!(101), dec!(1)),
            c(dec!(109), dec!(110), dec!(100)),
            c(dec!(100), dec!(101), dec!(1)),
        ];
        let vp = build(&candles, 10).expect("profile builds");
        assert!(
            vp.poc > dec!(105),
            "POC should sit near the heavy-volume top"
        );
    }

    #[test]
    fn value_area_contains_the_poc() {
        let candles = vec![
            c(dec!(100), dec!(110), dec!(50)),
            c(dec!(102), dec!(104), dec!(20)),
        ];
        let vp = build(&candles, 20).expect("profile builds");
        assert!(vp.contains(vp.poc));
    }

    #[test]
    fn a_flat_price_range_produces_no_profile() {
        let candles = vec![c(dec!(100), dec!(100), dec!(50))];
        assert!(build(&candles, 10).is_none());
    }

    #[test]
    fn an_empty_window_produces_no_profile() {
        assert!(build(&[], 10).is_none());
    }
}
