use crate::alpaca::{AlpacaClient, Order, OrderRequest};
use crate::model::{
    calculate_exit_sell_price, calculate_hidden_mm_buy_price, format_tick_price,
};
use crate::state::TradeRecord;
use chrono::Utc;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{error, info, warn};
use uuid::Uuid;

pub struct HuntExecutor<'a> {
    client: &'a AlpacaClient,
    symbol: &'a str,
    hidden_mm_timeout: Duration,
}

impl<'a> HuntExecutor<'a> {
    pub fn new(client: &'a AlpacaClient, symbol: &'a str, timeout_seconds: u64) -> Self {
        Self {
            client,
            symbol,
            hidden_mm_timeout: Duration::from_secs(timeout_seconds),
        }
    }

    /// Execute a full Market Hunt:
    /// 1. Place HiddenMM limit BUY order at optimal legal maker price.
    /// 2. Wait up to 20 seconds for fill.
    /// 3. At 20s, cancel remaining unfilled portion.
    /// 4. If filled or partially filled, place limit SELL exit order at predicted fair price.
    pub async fn execute_hunt(
        &self,
        best_bid: f64,
        best_ask: f64,
        fair_price: f64,
        shares_to_buy: f64,
    ) -> Result<Option<TradeRecord>, String> {
        if shares_to_buy <= 0.0 {
            return Ok(None);
        }

        let buy_price = calculate_hidden_mm_buy_price(best_bid, best_ask);
        let buy_price_str = format_tick_price(buy_price);
        let client_order_id = format!("hunt-buy-{}", Uuid::new_v4());

        info!(
            "Starting Market Hunt on {}: placing HiddenMM BUY limit {} shares @ ${} (Best Bid: ${}, Best Ask: ${})",
            self.symbol, shares_to_buy, buy_price_str, best_bid, best_ask
        );

        let tif = if self.symbol.contains('/') { "gtc" } else { "day" };

        let buy_req = OrderRequest {
            symbol: self.symbol.to_string(),
            qty: shares_to_buy.to_string(),
            side: "buy".to_string(),
            order_type: "limit".to_string(),
            time_in_force: tif.to_string(),
            limit_price: Some(buy_price_str.clone()),
            client_order_id: Some(client_order_id.clone()),
            extended_hours: Some(false),
        };

        let submitted_order = self
            .client
            .submit_order(&buy_req)
            .await
            .map_err(|e| format!("Failed to submit HiddenMM buy order: {}", e))?;

        let order_id = submitted_order.id;
        let start_time = tokio::time::Instant::now();
        let mut final_order = submitted_order;

        // Poll for up to 20 seconds
        while start_time.elapsed() < self.hidden_mm_timeout {
            sleep(Duration::from_millis(1500)).await;

            match self.client.get_order(&order_id).await {
                Ok(order) => {
                    final_order = order;
                    if final_order.is_terminal() {
                        break;
                    }
                }
                Err(e) => {
                    warn!("Error polling order {} status: {}", order_id, e);
                }
            }
        }

        // 20s reached: if not terminal, cancel remaining unfilled shares
        if !final_order.is_terminal() {
            info!(
                "HiddenMM timeout (20s) reached for order {}. Canceling remainder.",
                order_id
            );
            let _ = self.client.cancel_order(&order_id).await;

            // Wait a brief moment for exchange cancel confirmation and check final filled_qty
            sleep(Duration::from_millis(1000)).await;
            if let Ok(order) = self.client.get_order(&order_id).await {
                final_order = order;
            }
        }

        let filled_qty = final_order.parsed_filled_qty();
        let avg_price = final_order.parsed_avg_price().unwrap_or(buy_price);

        if filled_qty <= 0.0 {
            info!("HiddenMM order {} finished with 0 fills. Hunt ended.", order_id);
            return Ok(None);
        }

        info!(
            "HiddenMM order {} filled {} of {} shares @ avg ${:.2}",
            order_id, filled_qty, shares_to_buy, avg_price
        );

        // Market Exit: immediately place limit SELL order for exact filled_qty at predicted value
        let exit_price = calculate_exit_sell_price(avg_price, fair_price);
        let exit_price_str = format_tick_price(exit_price);
        let exit_client_id = format!("hunt-sell-{}", Uuid::new_v4());

        info!(
            "Placing Market Exit SELL order for {} shares @ ${} (predicted: ${:.2})",
            filled_qty, exit_price_str, fair_price
        );

        let sell_req = OrderRequest {
            symbol: self.symbol.to_string(),
            qty: filled_qty.to_string(),
            side: "sell".to_string(),
            order_type: "limit".to_string(),
            time_in_force: tif.to_string(),
            limit_price: Some(exit_price_str.clone()),
            client_order_id: Some(exit_client_id),
            extended_hours: Some(false),
        };

        let sell_order_id = match self.client.submit_order(&sell_req).await {
            Ok(sell_order) => {
                info!("Market Exit SELL order submitted successfully: id {}", sell_order.id);
                Some(sell_order.id)
            }
            Err(e) => {
                error!("Critical: Failed to submit Market Exit SELL order: {}", e);
                None
            }
        };

        let trade = TradeRecord {
            id: Uuid::new_v4().to_string(),
            symbol: self.symbol.to_string(),
            buy_order_id: order_id,
            sell_order_id,
            qty: filled_qty,
            buy_price: avg_price,
            sell_price: Some(exit_price),
            pnl: None,
            pnl_pct: None,
            buy_timestamp: Utc::now().to_rfc3339(),
            sell_timestamp: None,
            status: "open_exit_pending".to_string(),
        };

        Ok(Some(trade))
    }
}
