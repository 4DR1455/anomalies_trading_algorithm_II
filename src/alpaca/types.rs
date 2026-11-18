use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QuoteDetail {
    #[serde(rename = "t")]
    pub timestamp: Option<String>,
    #[serde(rename = "bp")]
    pub bid_price: f64,
    #[serde(rename = "ap")]
    pub ask_price: f64,
    #[serde(rename = "bs")]
    pub bid_size: Option<f64>,
    #[serde(rename = "as")]
    pub ask_size: Option<f64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LatestQuoteResponse {
    pub quotes: std::collections::HashMap<String, QuoteDetail>,
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
    pub client_order_id: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub submitted_at: Option<String>,
    pub filled_at: Option<String>,
    pub expired_at: Option<String>,
    pub canceled_at: Option<String>,
    pub failed_at: Option<String>,
    pub symbol: String,
    pub qty: String,
    pub filled_qty: String,
    pub filled_avg_price: Option<String>,
    pub order_class: Option<String>,
    pub side: String,
    #[serde(rename = "type")]
    pub order_type: String,
    pub time_in_force: String,
    pub limit_price: Option<String>,
    pub status: String, // "new", "partially_filled", "filled", "canceled", "rejected", etc.
}

impl Order {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self.status.as_str(),
            "filled" | "canceled" | "expired" | "done_for_day" | "rejected" | "replaced"
        )
    }

    pub fn is_filled(&self) -> bool {
        self.status == "filled"
    }

    pub fn parsed_filled_qty(&self) -> u32 {
        self.filled_qty.parse::<f64>().map(|v| v as u32).unwrap_or(0)
    }

    pub fn parsed_avg_price(&self) -> Option<f64> {
        self.filled_avg_price.as_ref().and_then(|s| s.parse::<f64>().ok())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Position {
    pub asset_id: Option<String>,
    pub symbol: String,
    pub exchange: Option<String>,
    pub asset_class: Option<String>,
    pub qty: String,
    pub avg_entry_price: String,
    pub side: String,
    pub market_value: Option<String>,
    pub cost_basis: Option<String>,
    pub unrealized_pl: Option<String>,
    pub current_price: Option<String>,
}

impl Position {
    pub fn parsed_qty(&self) -> i64 {
        self.qty.parse::<f64>().map(|v| v as i64).unwrap_or(0)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Clock {
    pub timestamp: String,
    pub is_open: bool,
    pub next_open: String,
    pub next_close: String,
}
