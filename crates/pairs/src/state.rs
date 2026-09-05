use exchange::ExchangeClient;
use persistence::PairPositionRecord;
use rust_decimal::Decimal;

use crate::executor::ExecutionError;
use crate::signal::PairParams;

/// What the journal and the exchange, taken together, say this bot is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Reconciliation {
    Flat,
    Holding(Box<PairPositionRecord>),
    /// The two sources disagree. Trading must stop for this pair until a human
    /// looks. Guessing which one is right is how a bot compounds a mistake it
    /// did not make.
    Halt { reason: String },
}

/// Compare the journal against the exchange before any strategy evaluation.
///
/// The exchange is the source of truth about what exists; the journal is the
/// only source of truth about why. When they disagree neither can be
/// reconstructed from the other, so this halts and reports rather than
/// choosing.
pub async fn reconcile_pair(
    client: &dyn ExchangeClient,
    journal_position: Option<&PairPositionRecord>,
    params: &PairParams,
) -> Result<Reconciliation, ExecutionError> {
    let ours: Vec<_> = client
        .positions()
        .await?
        .into_iter()
        .filter(|p| p.symbol == params.leg_a || p.symbol == params.leg_b)
        .filter(|p| p.size > Decimal::ZERO)
        .collect();

    match (journal_position, ours.len()) {
        (None, 0) => Ok(Reconciliation::Flat),
        (Some(rec), 2) => Ok(Reconciliation::Holding(Box::new(rec.clone()))),
        (None, _) => Ok(Reconciliation::Halt {
            reason: format!(
                "exchange holds {} on {} with no journalled pair position; refusing to trade until reviewed",
                ours.len(),
                ours.iter()
                    .map(|p| p.symbol.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }),
        (Some(_), n) => Ok(Reconciliation::Halt {
            reason: format!(
                "journal holds a pair position but the exchange shows {n} of 2 legs open on {}; refusing to trade until reviewed",
                params.display_pair()
            ),
        }),
    }
}
