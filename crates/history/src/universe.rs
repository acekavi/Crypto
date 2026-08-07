use botcore::{Symbol, Timeframe};
use rust_decimal::Decimal;

use crate::db::{HistoryDb, HistoryError};

const DAY_MS: i64 = 86_400_000;

/// The symbols the historical backtest would have traded at `at_ms`, ranked
/// the same way `crates/engine/src/universe.rs::select_universe` ranks the
/// live bot's universe. This is what removes survivorship bias from a
/// backtest: a fixed top-N snapshot silently trades symbols the live bot
/// would never have selected at that point in history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UniverseSnapshot {
    pub at_ms: i64,
    pub symbols: Vec<Symbol>,
}

/// Mirrors `engine::universe::UniverseFilter`'s turnover and age floors.
/// `size`/top-N is a separate argument to `reconstruct_universe` rather than
/// a field here, matching how the live engine passes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HistoricalUniverseFilter {
    pub min_turnover_24h: Decimal,
    pub min_listing_age_days: i64,
}

/// Reconstructs the tradable universe as of `at_ms` from recorded H1
/// candles: filters `symbols` on both the turnover floor and the derived
/// listing-age floor, then ranks by 24h turnover descending with ties
/// broken on symbol name — mirroring `select_universe`'s ordering exactly,
/// since a different order here would silently backtest a different bot
/// than the one that trades live.
///
/// There is no historical instrument feed, so listing age is derived: a
/// symbol's age at `at_ms` is `at_ms - <its earliest stored H1 candle>`.
/// This under-estimates age whenever history was downloaded starting later
/// than the symbol's real listing date — the safe direction, since it can
/// only cause a symbol to be excluded, never cause the backtest to trade one
/// the live bot would have skipped.
///
/// A symbol with no candles in the trailing-24h window is dropped from
/// consideration entirely (see `HistoryDb::rolling_turnover_24h`) rather
/// than ranked at the bottom as if its turnover were zero.
pub async fn reconstruct_universe(
    db: &HistoryDb,
    symbols: &[Symbol],
    at_ms: i64,
    filter: &HistoricalUniverseFilter,
    top_n: usize,
) -> Result<UniverseSnapshot, HistoryError> {
    let mut qualified: Vec<(Symbol, Decimal)> = Vec::new();

    for symbol in symbols {
        let Some(turnover) = db.rolling_turnover_24h(symbol, at_ms).await? else {
            continue;
        };
        if turnover < filter.min_turnover_24h {
            continue;
        }

        let Some((earliest_ms, _)) = db.recorded_range(symbol, Timeframe::H1).await? else {
            continue;
        };
        let age_days = (at_ms - earliest_ms) / DAY_MS;
        if age_days < filter.min_listing_age_days {
            continue;
        }

        qualified.push((symbol.clone(), turnover));
    }

    qualified.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.as_str().cmp(b.0.as_str())));

    let symbols = qualified.into_iter().take(top_n).map(|(s, _)| s).collect();
    Ok(UniverseSnapshot { at_ms, symbols })
}
