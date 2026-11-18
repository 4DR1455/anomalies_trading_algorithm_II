use crate::alpaca::types::{Clock, LatestQuoteResponse, Order, OrderRequest, Position, QuoteDetail};
use crate::config::AlpacaCredentials;
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, CONTENT_TYPE};
use reqwest::{Client, StatusCode};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tracing::{debug, error, warn};

#[derive(Debug, thiserror::Error)]
pub enum AlpacaError {
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("API error status {status}: {message}")]
    ApiStatus { status: StatusCode, message: String },
    #[error("JSON deserialization error: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("Quote not found for symbol: {0}")]
    QuoteNotFound(String),
    #[error("Rate limit reached. Backing off.")]
    RateLimit,
}

// Lightweight manual Error implementation in case thiserror is not in Cargo.toml
impl std::fmt::Display for AlpacaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AlpacaError::Http(e) => write!(f, "HTTP request failed: {}", e),
            AlpacaError::ApiStatus { status, message } => {
                write!(f, "API error status {}: {}", status, message)
            }
            AlpacaError::Serialization(e) => write!(f, "JSON deserialization error: {}", e),
            AlpacaError::QuoteNotFound(sym) => write!(f, "Quote not found for symbol: {}", sym),
            AlpacaError::RateLimit => write!(f, "Rate limit reached. Backing off."),
        }
    }
}

impl std::error::Error for AlpacaError {}

#[derive(Clone)]
pub struct AlpacaClient {
    client: Client,
    trading_base_url: String,
    data_base_url: String,
    remaining_rate_limit: Arc<AtomicU32>,
}

impl AlpacaClient {
    pub fn new(credentials: &AlpacaCredentials) -> Result<Self, AlpacaError> {
        let mut default_headers = HeaderMap::new();
        default_headers.insert(
            "APCA-API-KEY-ID",
            HeaderValue::from_str(&credentials.key)
                .map_err(|e| AlpacaError::ApiStatus {
                    status: StatusCode::BAD_REQUEST,
                    message: format!("Invalid key header: {}", e),
                })?,
        );
        default_headers.insert(
            "APCA-API-SECRET-KEY",
            HeaderValue::from_str(&credentials.secret)
                .map_err(|e| AlpacaError::ApiStatus {
                    status: StatusCode::BAD_REQUEST,
                    message: format!("Invalid secret header: {}", e),
                })?,
        );
        default_headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        default_headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

        let client = Client::builder()
            .default_headers(default_headers)
            .connect_timeout(Duration::from_secs(4))
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(AlpacaError::Http)?;

        // Ensure proper base URL
        let trading_base_url = if credentials.endpoint.ends_with("/v2") {
            credentials.endpoint.clone()
        } else {
            format!("{}/v2", credentials.endpoint.trim_end_matches('/'))
        };

        // Standard Market Data v2 endpoint
        let data_base_url = "https://data.alpaca.markets/v2".to_string();

        Ok(Self {
            client,
            trading_base_url,
            data_base_url,
            remaining_rate_limit: Arc::new(AtomicU32::new(200)),
        })
    }

    fn check_rate_limit_headers(&self, headers: &HeaderMap) {
        if let Some(val) = headers.get("x-ratelimit-remaining") {
            if let Ok(val_str) = val.to_str() {
                if let Ok(num) = val_str.parse::<u32>() {
                    self.remaining_rate_limit.store(num, Ordering::Relaxed);
                    if num < 20 {
                        warn!("Alpaca API rate limit remaining low: {}", num);
                    }
                }
            }
        }
    }

    pub async fn get_latest_quote(&self, symbol: &str) -> Result<QuoteDetail, AlpacaError> {
        let url = if symbol.contains('/') {
            format!(
                "https://data.alpaca.markets/v1beta3/crypto/us/latest/quotes?symbols={}",
                symbol
            )
        } else {
            format!("{}/stocks/quotes/latest?symbols={}&feed=iex", self.data_base_url, symbol)
        };
        let resp = self.client.get(&url).send().await.map_err(AlpacaError::Http)?;
        self.check_rate_limit_headers(resp.headers());

        let status = resp.status();
        if !status.is_success() {
            let msg = resp.text().await.unwrap_or_default();
            return Err(AlpacaError::ApiStatus { status, message: msg });
        }

        let body: LatestQuoteResponse = resp.json().await.map_err(AlpacaError::Http)?;
        body.quotes
            .get(symbol)
            .cloned()
            .ok_or_else(|| AlpacaError::QuoteNotFound(symbol.to_string()))
    }

    pub async fn submit_order(&self, req: &OrderRequest) -> Result<Order, AlpacaError> {
        let url = format!("{}/orders", self.trading_base_url);
        let resp = self
            .client
            .post(&url)
            .json(req)
            .send()
            .await
            .map_err(AlpacaError::Http)?;
        self.check_rate_limit_headers(resp.headers());

        let status = resp.status();
        if !status.is_success() {
            let msg = resp.text().await.unwrap_or_default();
            return Err(AlpacaError::ApiStatus { status, message: msg });
        }

        let order: Order = resp.json().await.map_err(AlpacaError::Http)?;
        Ok(order)
    }

    pub async fn get_order(&self, order_id: &str) -> Result<Order, AlpacaError> {
        let url = format!("{}/orders/{}", self.trading_base_url, order_id);
        let resp = self.client.get(&url).send().await.map_err(AlpacaError::Http)?;
        self.check_rate_limit_headers(resp.headers());

        let status = resp.status();
        if !status.is_success() {
            let msg = resp.text().await.unwrap_or_default();
            return Err(AlpacaError::ApiStatus { status, message: msg });
        }

        let order: Order = resp.json().await.map_err(AlpacaError::Http)?;
        Ok(order)
    }

    /// Cancel order by ID.
    /// Returns Ok(true) if 204 or canceled, Ok(false) if 422 (order already terminal or not cancelable).
    pub async fn cancel_order(&self, order_id: &str) -> Result<bool, AlpacaError> {
        let url = format!("{}/orders/{}", self.trading_base_url, order_id);
        let resp = self.client.delete(&url).send().await.map_err(AlpacaError::Http)?;
        self.check_rate_limit_headers(resp.headers());

        let status = resp.status();
        if status == StatusCode::NO_CONTENT || status == StatusCode::OK {
            Ok(true)
        } else if status == StatusCode::UNPROCESSABLE_ENTITY {
            // Already filled or not cancelable
            debug!("Order {} cancellation returned 422 (already terminal)", order_id);
            Ok(false)
        } else {
            let msg = resp.text().await.unwrap_or_default();
            Err(AlpacaError::ApiStatus { status, message: msg })
        }
    }

    pub async fn get_open_orders(&self, symbol: Option<&str>) -> Result<Vec<Order>, AlpacaError> {
        let mut url = format!("{}/orders?status=open&limit=100", self.trading_base_url);
        if let Some(sym) = symbol {
            url.push_str(&format!("&symbols={}", sym));
        }

        let resp = self.client.get(&url).send().await.map_err(AlpacaError::Http)?;
        self.check_rate_limit_headers(resp.headers());

        let status = resp.status();
        if !status.is_success() {
            let msg = resp.text().await.unwrap_or_default();
            return Err(AlpacaError::ApiStatus { status, message: msg });
        }

        let orders: Vec<Order> = resp.json().await.map_err(AlpacaError::Http)?;
        Ok(orders)
    }

    pub async fn get_position(&self, symbol: &str) -> Result<Option<Position>, AlpacaError> {
        let url = format!("{}/positions/{}", self.trading_base_url, symbol);
        let resp = self.client.get(&url).send().await.map_err(AlpacaError::Http)?;
        self.check_rate_limit_headers(resp.headers());

        let status = resp.status();
        if status == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            let msg = resp.text().await.unwrap_or_default();
            return Err(AlpacaError::ApiStatus { status, message: msg });
        }

        let pos: Position = resp.json().await.map_err(AlpacaError::Http)?;
        Ok(Some(pos))
    }

    pub async fn get_clock(&self) -> Result<Clock, AlpacaError> {
        let url = format!("{}/clock", self.trading_base_url);
        let resp = self.client.get(&url).send().await.map_err(AlpacaError::Http)?;
        self.check_rate_limit_headers(resp.headers());

        let status = resp.status();
        if !status.is_success() {
            let msg = resp.text().await.unwrap_or_default();
            return Err(AlpacaError::ApiStatus { status, message: msg });
        }

        let clock: Clock = resp.json().await.map_err(AlpacaError::Http)?;
        Ok(clock)
    }
}
