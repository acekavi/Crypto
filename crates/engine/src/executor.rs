use std::sync::Arc;

use botcore::{LimitEntry, OrderState, Symbol};
use exchange::ExchangeClient;
use exchange::bybit::transport::ExchangeError;
use persistence::{Journal, OrderRecord};
use risk::OrderIntent;
use rust_decimal::Decimal;
use tracing::warn;

use crate::link_id::order_link_id;
use crate::order_tracker::RestingOrder;

/// Places orders and records them.
///
/// The only component that talks to the exchange's trading endpoints. Every
/// entry carries its stop and target in the same request, so no window exists
/// where a position is open unprotected.
pub struct Executor {
    client: Arc<dyn ExchangeClient>,
    journal: Arc<Journal>,
    /// SHA-256 of the effective config, written to every order row so each
    /// trade is attributable to an exact ruleset.
    config_hash: String,
}

impl Executor {
    pub fn new(
        client: Arc<dyn ExchangeClient>,
        journal: Arc<Journal>,
        config_hash: String,
    ) -> Self {
        Executor {
            client,
            journal,
            config_hash,
        }
    }

    /// Place a PostOnly limit entry with protection attached.
    ///
    /// The journal is written only after the exchange accepts, so a row means
    /// "the exchange took this" rather than "we asked". A journal failure is
    /// logged and swallowed — persistence must never fail an order that the
    /// exchange has already accepted.
    pub async fn place_entry(&self, intent: &OrderIntent) -> Result<RestingOrder, ExchangeError> {
        let link_id = order_link_id(&intent.symbol, intent.signal_candle_open_ms, intent.side);

        let req = LimitEntry {
            symbol: intent.symbol.clone(),
            side: intent.side,
            qty: intent.qty,
            price: intent.entry_price,
            order_link_id: link_id.clone(),
            stop_loss: intent.stop_price,
            stop_limit_price: intent.stop_limit_price,
            take_profit: intent.target_price,
            breakeven_at_r: intent.breakeven_at_r,
        };

        let ack = self.client.place_limit_entry(req).await?;

        let record = OrderRecord {
            order_link_id: ack.order_link_id.clone(),
            order_id: Some(ack.order_id.clone()),
            symbol: intent.symbol.clone(),
            side: intent.side,
            price: intent.entry_price,
            qty: intent.qty,
            stop_loss: intent.stop_price,
            take_profit: intent.target_price,
            state: OrderState::New,
            cum_exec_qty: Decimal::ZERO,
            config_hash: self.config_hash.clone(),
            created_at_ms: intent.signal_candle_open_ms,
        };
        if let Err(e) = self.journal.record_order(&record).await {
            // The exchange has already accepted this order. Failing here would
            // mean reporting an error for an order that genuinely exists.
            warn!(error = %e, link_id = %ack.order_link_id, "journalling a placed order failed");
        }

        Ok(RestingOrder {
            link_id: ack.order_link_id,
            symbol: intent.symbol.clone(),
            side: intent.side,
            qty: intent.qty,
            cum_exec_qty: Decimal::ZERO,
            placed_at_candle_ms: intent.signal_candle_open_ms,
        })
    }

    pub async fn cancel(&self, symbol: &Symbol, link_id: &str) -> Result<(), ExchangeError> {
        self.client.cancel_order(symbol, link_id).await
    }

    /// Move a triggered stop's limit further into the move. Still a limit
    /// order — this never converts to a market order.
    pub async fn widen_stop(
        &self,
        symbol: &Symbol,
        trigger: Decimal,
        limit_price: Decimal,
    ) -> Result<(), ExchangeError> {
        self.client.amend_stop(symbol, trigger, limit_price).await
    }
}
