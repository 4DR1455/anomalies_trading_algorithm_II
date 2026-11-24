use crate::model::pricing::serde_decimal::{
    deserialize_decimal, deserialize_decimal_or_zero, deserialize_opt_decimal,
};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    #[serde(default, deserialize_with = "deserialize_decimal_or_zero")]
    pub equity: Decimal,
    #[serde(default, deserialize_with = "deserialize_decimal_or_zero")]
    pub cash: Decimal,
    #[serde(default, deserialize_with = "deserialize_decimal_or_zero")]
    pub buying_power: Decimal,
    #[serde(default, deserialize_with = "deserialize_opt_decimal")]
    pub non_marginable_buying_power: Option<Decimal>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuoteDetail {
    #[serde(rename = "t", default)]
    pub timestamp: Option<String>,
    #[serde(rename = "bp", deserialize_with = "deserialize_decimal")]
    pub bid_price: Decimal,
    #[serde(rename = "ap", deserialize_with = "deserialize_decimal")]
    pub ask_price: Decimal,
    #[serde(rename = "bs", default, deserialize_with = "deserialize_opt_decimal")]
    pub bid_size: Option<Decimal>,
    #[serde(rename = "as", default, deserialize_with = "deserialize_opt_decimal")]
    pub ask_size: Option<Decimal>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LatestQuoteResponse {
    pub quotes: HashMap<String, QuoteDetail>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrderRequest {
    pub symbol: String,
    pub qty: String,
    pub side: String, // "buy" or "sell"
    #[serde(rename = "type")]
    pub order_type: String, // "limit" or "market"
    pub time_in_force: String, // "day" or "gtc"
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_price: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_order_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extended_hours: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Order {
    pub id: String,
    #[serde(default)]
    pub client_order_id: Option<String>,
    #[serde(default)]
    pub created_at: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default)]
    pub submitted_at: Option<String>,
    #[serde(default)]
    pub filled_at: Option<String>,
    #[serde(default)]
    pub expired_at: Option<String>,
    #[serde(default)]
    pub canceled_at: Option<String>,
    #[serde(default)]
    pub failed_at: Option<String>,
    pub symbol: String,
    #[serde(default, deserialize_with = "deserialize_decimal_or_zero")]
    pub qty: Decimal,
    #[serde(default, deserialize_with = "deserialize_decimal_or_zero")]
    pub filled_qty: Decimal,
    #[serde(default, deserialize_with = "deserialize_opt_decimal")]
    pub filled_avg_price: Option<Decimal>,
    #[serde(default)]
    pub order_class: Option<String>,
    pub side: String,
    #[serde(rename = "type")]
    pub order_type: String,
    #[serde(default)]
    pub time_in_force: String,
    #[serde(default, deserialize_with = "deserialize_opt_decimal")]
    pub limit_price: Option<Decimal>,
    pub status: String,
}

impl Order {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.status.as_str(),
            "filled" | "canceled" | "expired" | "done_for_day" | "rejected" | "replaced"
        )
    }

    pub fn is_open(&self) -> bool {
        matches!(
            self.status.as_str(),
            "new"
                | "accepted"
                | "pending_new"
                | "partially_filled"
                | "accepted_for_bidding"
                | "pending_replace"
                | "pending_cancel"
        )
    }

    pub fn is_filled(&self) -> bool {
        self.status == "filled"
    }

    pub fn is_canceled_or_failed(&self) -> bool {
        matches!(self.status.as_str(), "canceled" | "rejected" | "expired")
    }

    pub fn remaining_qty(&self) -> Decimal {
        (self.qty - self.filled_qty).max(Decimal::ZERO)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    #[serde(default)]
    pub asset_id: Option<String>,
    pub symbol: String,
    #[serde(default)]
    pub exchange: Option<String>,
    #[serde(default)]
    pub asset_class: Option<String>,
    #[serde(default, deserialize_with = "deserialize_decimal_or_zero")]
    pub qty: Decimal,
    #[serde(default, deserialize_with = "deserialize_decimal_or_zero")]
    pub qty_available: Decimal,
    #[serde(default, deserialize_with = "deserialize_decimal_or_zero")]
    pub avg_entry_price: Decimal,
    #[serde(default)]
    pub side: String,
    #[serde(default, deserialize_with = "deserialize_opt_decimal")]
    pub market_value: Option<Decimal>,
    #[serde(default, deserialize_with = "deserialize_opt_decimal")]
    pub cost_basis: Option<Decimal>,
    #[serde(default, deserialize_with = "deserialize_opt_decimal")]
    pub unrealized_pl: Option<Decimal>,
    #[serde(default, deserialize_with = "deserialize_opt_decimal")]
    pub current_price: Option<Decimal>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PortfolioHistory {
    #[serde(default)]
    pub timestamp: Vec<i64>,
    #[serde(default)]
    pub equity: Vec<Option<f64>>,
    #[serde(default)]
    pub profit_loss: Vec<Option<f64>>,
    #[serde(default)]
    pub profit_loss_pct: Vec<Option<f64>>,
    #[serde(default)]
    pub base_value: Option<f64>,
    #[serde(default)]
    pub timeframe: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AccountActivity {
    pub id: String,
    #[serde(default)]
    pub activity_type: String,
    #[serde(default)]
    pub order_id: Option<String>,
    #[serde(default)]
    pub symbol: Option<String>,
    #[serde(default)]
    pub transaction_time: Option<String>,
    #[serde(default)]
    pub date: Option<String>,
    #[serde(default, deserialize_with = "deserialize_opt_decimal")]
    pub net_amount: Option<Decimal>,
    #[serde(default, deserialize_with = "deserialize_opt_decimal")]
    pub qty: Option<Decimal>,
    #[serde(default, deserialize_with = "deserialize_opt_decimal")]
    pub price: Option<Decimal>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
}

impl AccountActivity {
    /// Returns positive USD fee amount extracted from `net_amount` (Alpaca reports `CFEE` as negative USD).
    pub fn fee_usd_abs(&self) -> Option<Decimal> {
        self.net_amount
            .map(|d| d.abs())
            .filter(|d| *d > Decimal::ZERO)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn test_order_request_omits_extended_hours_when_none() {
        let req = OrderRequest {
            symbol: "SOL/USD".to_string(),
            qty: "10.5".to_string(),
            side: "buy".to_string(),
            order_type: "limit".to_string(),
            time_in_force: "gtc".to_string(),
            limit_price: Some("72.50".to_string()),
            client_order_id: Some("hunt-buy-1".to_string()),
            extended_hours: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(!json.contains("extended_hours"));
    }

    #[test]
    fn test_position_and_quote_decimal_parsing_no_truncation() {
        let pos_json = r#"{
            "symbol": "SOLUSD",
            "qty": "882.486814541",
            "qty_available": "332.305980573",
            "avg_entry_price": "73.43",
            "side": "long",
            "market_value": "63971.47",
            "unrealized_pl": "-829.54",
            "current_price": "72.49"
        }"#;
        let pos: Position = serde_json::from_str(pos_json).unwrap();
        assert_eq!(pos.qty, Decimal::from_str("882.486814541").unwrap());
        assert_eq!(
            pos.qty_available,
            Decimal::from_str("332.305980573").unwrap()
        );

        let quote_json = r#"{"t":"2026-11-24T00:00:00Z","bp":72.45,"ap":72.47,"bs":10.5,"as":20}"#;
        let q: QuoteDetail = serde_json::from_str(quote_json).unwrap();
        assert_eq!(q.bid_price, Decimal::from_str("72.45").unwrap());
        assert_eq!(q.ask_price, Decimal::from_str("72.47").unwrap());
    }
}
