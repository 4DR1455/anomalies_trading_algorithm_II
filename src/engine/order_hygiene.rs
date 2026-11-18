use crate::alpaca::{AlpacaClient, OrderRequest};
use crate::model::format_tick_price;
use crate::state::TradeRecord;
use chrono::{DateTime, Utc};
use tracing::{info, warn};
use uuid::Uuid;

pub struct OrderHygieneManager<'a> {
    client: &'a AlpacaClient,
    symbol: &'a str,
    max_order_age_seconds: u64,
}

impl<'a> OrderHygieneManager<'a> {
    pub fn new(client: &'a AlpacaClient, symbol: &'a str, max_age_seconds: u64) -> Self {
        Self {
            client,
            symbol,
            max_order_age_seconds: max_age_seconds,
        }
    }

    /// Perform comprehensive order hygiene checks:
    /// 1. Reconcile active hunts against order statuses (check if exit filled).
    /// 2. Check for orders open > 12 hours -> cancel and liquidate at market price.
    /// 3. Detect dropped exit orders and re-raise them if position remains unhedged.
    pub async fn run_hygiene_cycle(
        &self,
        active_hunts: &mut Vec<TradeRecord>,
        completed_trades: &mut Vec<TradeRecord>,
        total_pnl: &mut f64,
        fair_price: f64,
    ) {
        // Step 1: Reconcile active hunts
        let mut still_active = Vec::new();

        for mut hunt in active_hunts.drain(..) {
            if let Some(ref sell_id) = hunt.sell_order_id {
                match self.client.get_order(sell_id).await {
                    Ok(order) => {
                        if order.is_filled() {
                            let filled_price = order
                                .parsed_avg_price()
                                .or(hunt.sell_price)
                                .unwrap_or(hunt.buy_price);
                            let profit_per_share = filled_price - hunt.buy_price;
                            let pnl = profit_per_share * hunt.qty as f64;
                            let pnl_pct = if hunt.buy_price > 0.0 {
                                profit_per_share / hunt.buy_price
                            } else {
                                0.0
                            };

                            hunt.sell_price = Some(filled_price);
                            hunt.pnl = Some(pnl);
                            hunt.pnl_pct = Some(pnl_pct);
                            hunt.sell_timestamp = Some(Utc::now().to_rfc3339());
                            hunt.status = "completed".to_string();

                            *total_pnl += pnl;
                            info!(
                                "Market Hunt completed: {} shares of {} sold @ ${:.2} (Buy: ${:.2}, PnL: ${:.2})",
                                hunt.qty, self.symbol, filled_price, hunt.buy_price, pnl
                            );
                            completed_trades.push(hunt);
                            continue;
                        } else if order.is_terminal() && !order.is_filled() {
                            // Exit order was canceled or expired unexpectedly!
                            warn!(
                                "Exit SELL order {} terminated with status '{}' without full fill. Re-evaluating inventory.",
                                sell_id, order.status
                            );
                            hunt.sell_order_id = None;
                            still_active.push(hunt);
                            continue;
                        }
                    }
                    Err(e) => {
                        warn!("Could not fetch status for sell order {}: {}", sell_id, e);
                    }
                }
            }
            still_active.push(hunt);
        }

        *active_hunts = still_active;

        // Step 2: Query all open orders on Alpaca and enforce 12-hour circuit/liquidation
        match self.client.get_open_orders(Some(self.symbol)).await {
            Ok(open_orders) => {
                let now = Utc::now();
                for order in open_orders {
                    let is_stale = if let Some(ref created_at_str) = order.created_at {
                        if let Ok(created_at) = DateTime::parse_from_rfc3339(created_at_str) {
                            let age_secs = (now - created_at.with_timezone(&Utc)).num_seconds();
                            age_secs > self.max_order_age_seconds as i64
                        } else {
                            false
                        }
                    } else {
                        false
                    };

                    if is_stale {
                        info!(
                            "Order {} exceeded 12-hour limit (age > {}s). Canceling and liquidating.",
                            order.id, self.max_order_age_seconds
                        );

                        let _ = self.client.cancel_order(&order.id).await;

                        // Check position to liquidate
                        if let Ok(Some(position)) = self.client.get_position(self.symbol).await {
                            let qty = position.parsed_qty();
                            if qty > 0 {
                                info!(
                                    "Stale hygiene: Liquidating {} held shares of {} at market",
                                    qty, self.symbol
                                );
                                let tif = if self.symbol.contains('/') { "gtc" } else { "day" };
                                let liq_req = OrderRequest {
                                    symbol: self.symbol.to_string(),
                                    qty: qty.to_string(),
                                    side: "sell".to_string(),
                                    order_type: "market".to_string(),
                                    time_in_force: tif.to_string(),
                                    limit_price: None,
                                    client_order_id: Some(format!("stale-liq-{}", Uuid::new_v4())),
                                    extended_hours: Some(false),
                                };
                                let _ = self.client.submit_order(&liq_req).await;
                            }
                        }
                    }
                }
            }
            Err(e) => {
                warn!("Hygiene check: failed to query open orders: {}", e);
            }
        }

        // Step 3: Dropped Order Recovery
        // Check if we have active hunts without an open sell order, but inventory exists
        for hunt in active_hunts.iter_mut() {
            if hunt.sell_order_id.is_none() {
                info!(
                    "Recovering dropped exit order for hunt {} ({} shares)",
                    hunt.id, hunt.qty
                );
                let exit_price_str = format_tick_price(fair_price);
                let re_raise_req = OrderRequest {
                    symbol: self.symbol.to_string(),
                    qty: hunt.qty.to_string(),
                    side: "sell".to_string(),
                    order_type: "limit".to_string(),
                    time_in_force: "day".to_string(),
                    limit_price: Some(exit_price_str),
                    client_order_id: Some(format!("reraise-sell-{}", Uuid::new_v4())),
                    extended_hours: Some(false),
                };
                if let Ok(new_order) = self.client.submit_order(&re_raise_req).await {
                    info!("Successfully re-raised exit sell order {}", new_order.id);
                    hunt.sell_order_id = Some(new_order.id);
                }
            }
        }
    }
}
