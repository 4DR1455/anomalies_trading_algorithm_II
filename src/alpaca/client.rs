use crate::alpaca::types::{
    Account, AccountActivity, LatestQuoteResponse, Order, OrderRequest, PortfolioHistory, Position,
    QuoteDetail,
};
use crate::config::AlpacaCredentials;
use crate::model::pricing::decimal_from_f64_clean;
use async_trait::async_trait;
use reqwest::header::{HeaderMap, HeaderValue, ACCEPT, CONTENT_TYPE};
use reqwest::{Client, Method, StatusCode};
use rust_decimal::Decimal;
use std::collections::HashSet;
use std::str::FromStr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use tracing::{debug, warn};

const MAX_HTTP_ATTEMPTS: usize = 3;
const RETRY_BASE_DELAY_MS: u64 = 250;
pub const MAX_ACTIVITY_PAGES: usize = 5;
pub const ACTIVITY_PAGE_SIZE: usize = 100;

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
    #[error("Rate limit reached")]
    RateLimit,
    #[error("Mock/Client error: {0}")]
    Other(String),
}

impl AlpacaError {
    pub fn status_code(&self) -> Option<StatusCode> {
        match self {
            AlpacaError::ApiStatus { status, .. } => Some(*status),
            AlpacaError::RateLimit => Some(StatusCode::TOO_MANY_REQUESTS),
            _ => None,
        }
    }

    pub fn is_balance_or_unprocessable(&self) -> bool {
        matches!(
            self.status_code(),
            Some(StatusCode::FORBIDDEN) | Some(StatusCode::UNPROCESSABLE_ENTITY)
        )
    }

    /// Extracts Alpaca's structured `"available"` decimal quantity from HTTP 403/422 JSON error bodies.
    pub fn parsed_available_qty(&self) -> Option<Decimal> {
        let AlpacaError::ApiStatus { message, .. } = self else {
            return None;
        };
        let parsed: serde_json::Value = serde_json::from_str(message).ok()?;
        let avail = parsed.get("available")?;
        if let Some(s) = avail.as_str() {
            Decimal::from_str(s.trim()).ok()
        } else if let Some(f) = avail.as_f64() {
            decimal_from_f64_clean(f)
        } else {
            None
        }
    }
}

/// Async broker interface abstracting all Alpaca network I/O for deterministic mock-broker testing.
#[async_trait]
pub trait AlpacaApi: Send + Sync {
    async fn get_account(&self) -> Result<Account, AlpacaError>;
    async fn get_position(&self, symbol: &str) -> Result<Option<Position>, AlpacaError>;
    async fn get_open_orders(&self, symbol: Option<&str>) -> Result<Vec<Order>, AlpacaError>;
    async fn get_order(&self, order_id: &str) -> Result<Order, AlpacaError>;
    async fn submit_order(&self, req: &OrderRequest) -> Result<Order, AlpacaError>;
    async fn cancel_order(&self, order_id: &str) -> Result<bool, AlpacaError>;
    async fn get_latest_quote(&self, symbol: &str) -> Result<QuoteDetail, AlpacaError>;
    async fn get_portfolio_history(
        &self,
        period: &str,
        timeframe: &str,
    ) -> Result<PortfolioHistory, AlpacaError>;
    async fn get_account_activities(
        &self,
        activity_types: &str,
        after: Option<&str>,
    ) -> Result<Vec<AccountActivity>, AlpacaError>;

    /// Fetches a single page (up to `ACTIVITY_PAGE_SIZE = 100` entries) of account activities
    /// using Alpaca's `page_token` cursor. Default implementation delegates page 1 (`None`) to
    /// `get_account_activities` and returns an empty page for subsequent tokens so simple
    /// mock brokers remain 100% compatible.
    async fn get_account_activities_page(
        &self,
        activity_types: &str,
        after: Option<&str>,
        page_token: Option<&str>,
    ) -> Result<Vec<AccountActivity>, AlpacaError> {
        if page_token.is_some() {
            Ok(Vec::new())
        } else {
            self.get_account_activities(activity_types, after).await
        }
    }
}

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
            HeaderValue::from_str(&credentials.key).map_err(|e| AlpacaError::ApiStatus {
                status: StatusCode::BAD_REQUEST,
                message: format!("Invalid key header: {}", e),
            })?,
        );
        default_headers.insert(
            "APCA-API-SECRET-KEY",
            HeaderValue::from_str(&credentials.secret).map_err(|e| AlpacaError::ApiStatus {
                status: StatusCode::BAD_REQUEST,
                message: format!("Invalid secret header: {}", e),
            })?,
        );
        default_headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        default_headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));

        let client = Client::builder()
            .default_headers(default_headers)
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(12))
            .build()
            .map_err(AlpacaError::Http)?;

        let trimmed = credentials.endpoint.trim_end_matches('/');
        let trading_base_url = if trimmed.ends_with("/v2") {
            trimmed.to_string()
        } else {
            format!("{}/v2", trimmed)
        };

        Ok(Self {
            client,
            trading_base_url,
            data_base_url: "https://data.alpaca.markets".to_string(),
            remaining_rate_limit: Arc::new(AtomicU32::new(200)),
        })
    }

    pub fn remaining_rate_limit(&self) -> u32 {
        self.remaining_rate_limit.load(Ordering::Relaxed)
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

    fn should_retry_status(status: StatusCode) -> bool {
        status == StatusCode::TOO_MANY_REQUESTS || status.is_server_error()
    }

    async fn send_with_retry(
        &self,
        method: Method,
        url: &str,
        body_json: Option<&serde_json::Value>,
    ) -> Result<(StatusCode, String), AlpacaError> {
        let mut last_err: Option<AlpacaError> = None;

        for attempt in 0..MAX_HTTP_ATTEMPTS {
            let mut req = self.client.request(method.clone(), url);
            if let Some(body) = body_json {
                req = req.json(body);
            }

            match req.send().await {
                Ok(resp) => {
                    self.check_rate_limit_headers(resp.headers());
                    let status = resp.status();
                    let text = resp.text().await.unwrap_or_default();

                    if Self::should_retry_status(status) && attempt + 1 < MAX_HTTP_ATTEMPTS {
                        let backoff_ms = RETRY_BASE_DELAY_MS * (1u64 << attempt);
                        warn!(
                            "Transient Alpaca HTTP {} on {} {} (attempt {}/{}). Retrying in {}ms...",
                            status,
                            method,
                            url,
                            attempt + 1,
                            MAX_HTTP_ATTEMPTS,
                            backoff_ms
                        );
                        sleep(Duration::from_millis(backoff_ms)).await;
                        last_err = Some(AlpacaError::ApiStatus {
                            status,
                            message: text,
                        });
                        continue;
                    }

                    return Ok((status, text));
                }
                Err(e) => {
                    if attempt + 1 < MAX_HTTP_ATTEMPTS {
                        let backoff_ms = RETRY_BASE_DELAY_MS * (1u64 << attempt);
                        warn!(
                            "Transient transport error on {} {} (attempt {}/{}): {}. Retrying in {}ms...",
                            method,
                            url,
                            attempt + 1,
                            MAX_HTTP_ATTEMPTS,
                            e,
                            backoff_ms
                        );
                        sleep(Duration::from_millis(backoff_ms)).await;
                        last_err = Some(AlpacaError::Http(e));
                        continue;
                    }
                    return Err(AlpacaError::Http(e));
                }
            }
        }

        Err(last_err.unwrap_or(AlpacaError::RateLimit))
    }
}

#[async_trait]
impl AlpacaApi for AlpacaClient {
    async fn get_account(&self) -> Result<Account, AlpacaError> {
        let url = format!("{}/account", self.trading_base_url);
        let (status, text) = self.send_with_retry(Method::GET, &url, None).await?;
        if !status.is_success() {
            return Err(AlpacaError::ApiStatus {
                status,
                message: text,
            });
        }
        Ok(serde_json::from_str(&text)?)
    }

    async fn get_position(&self, symbol: &str) -> Result<Option<Position>, AlpacaError> {
        // Alpaca /v2/positions/{symbol} expects crypto pairs without slash (e.g. SOLUSD)
        let pos_sym = symbol.replace('/', "");
        let url = format!("{}/positions/{}", self.trading_base_url, pos_sym);
        let (status, text) = self.send_with_retry(Method::GET, &url, None).await?;
        if status == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            return Err(AlpacaError::ApiStatus {
                status,
                message: text,
            });
        }
        Ok(Some(serde_json::from_str(&text)?))
    }

    async fn get_open_orders(&self, symbol: Option<&str>) -> Result<Vec<Order>, AlpacaError> {
        // Alpaca /v2/orders?status=open expects crypto symbols WITH slash (e.g. SOL/USD)
        let mut url = format!("{}/orders?status=open&limit=100", self.trading_base_url);
        if let Some(sym) = symbol {
            url.push_str(&format!("&symbols={}", sym));
        }
        let (status, text) = self.send_with_retry(Method::GET, &url, None).await?;
        if !status.is_success() {
            return Err(AlpacaError::ApiStatus {
                status,
                message: text,
            });
        }
        Ok(serde_json::from_str(&text)?)
    }

    async fn get_order(&self, order_id: &str) -> Result<Order, AlpacaError> {
        let url = format!("{}/orders/{}", self.trading_base_url, order_id);
        let (status, text) = self.send_with_retry(Method::GET, &url, None).await?;
        if !status.is_success() {
            return Err(AlpacaError::ApiStatus {
                status,
                message: text,
            });
        }
        Ok(serde_json::from_str(&text)?)
    }

    async fn submit_order(&self, req: &OrderRequest) -> Result<Order, AlpacaError> {
        let url = format!("{}/orders", self.trading_base_url);
        let body = serde_json::to_value(req)?;
        let (status, text) = self
            .send_with_retry(Method::POST, &url, Some(&body))
            .await?;
        if !status.is_success() {
            return Err(AlpacaError::ApiStatus {
                status,
                message: text,
            });
        }
        Ok(serde_json::from_str(&text)?)
    }

    async fn cancel_order(&self, order_id: &str) -> Result<bool, AlpacaError> {
        let url = format!("{}/orders/{}", self.trading_base_url, order_id);
        let (status, text) = self.send_with_retry(Method::DELETE, &url, None).await?;
        if status == StatusCode::NO_CONTENT || status == StatusCode::OK {
            Ok(true)
        } else if status == StatusCode::NOT_FOUND || status == StatusCode::UNPROCESSABLE_ENTITY {
            debug!(
                "Order {} cancel returned {} (already terminal)",
                order_id, status
            );
            Ok(false)
        } else {
            Err(AlpacaError::ApiStatus {
                status,
                message: text,
            })
        }
    }

    async fn get_latest_quote(&self, symbol: &str) -> Result<QuoteDetail, AlpacaError> {
        let url = if symbol.contains('/') {
            format!(
                "{}/v1beta3/crypto/us/latest/quotes?symbols={}",
                self.data_base_url, symbol
            )
        } else {
            format!(
                "{}/v2/stocks/quotes/latest?symbols={}&feed=iex",
                self.data_base_url, symbol
            )
        };
        let (status, text) = self.send_with_retry(Method::GET, &url, None).await?;
        if !status.is_success() {
            return Err(AlpacaError::ApiStatus {
                status,
                message: text,
            });
        }
        let body: LatestQuoteResponse = serde_json::from_str(&text)?;
        body.quotes
            .get(symbol)
            .cloned()
            .ok_or_else(|| AlpacaError::QuoteNotFound(symbol.to_string()))
    }

    async fn get_portfolio_history(
        &self,
        period: &str,
        timeframe: &str,
    ) -> Result<PortfolioHistory, AlpacaError> {
        let url = format!(
            "{}/account/portfolio/history?period={}&timeframe={}",
            self.trading_base_url, period, timeframe
        );
        let (status, text) = self.send_with_retry(Method::GET, &url, None).await?;
        if !status.is_success() {
            return Err(AlpacaError::ApiStatus {
                status,
                message: text,
            });
        }
        Ok(serde_json::from_str(&text)?)
    }

    async fn get_account_activities_page(
        &self,
        activity_types: &str,
        after: Option<&str>,
        page_token: Option<&str>,
    ) -> Result<Vec<AccountActivity>, AlpacaError> {
        let mut url = format!(
            "{}/account/activities?activity_types={}&direction=desc&page_size={}",
            self.trading_base_url, activity_types, ACTIVITY_PAGE_SIZE
        );
        if let Some(after_ts) = after {
            url.push_str(&format!("&after={}", after_ts));
        }
        if let Some(token) = page_token.filter(|t| !t.is_empty()) {
            url.push_str(&format!("&page_token={}", token));
        }
        let (status, text) = self.send_with_retry(Method::GET, &url, None).await?;
        if !status.is_success() {
            return Err(AlpacaError::ApiStatus {
                status,
                message: text,
            });
        }
        Ok(serde_json::from_str(&text)?)
    }

    async fn get_account_activities(
        &self,
        activity_types: &str,
        after: Option<&str>,
    ) -> Result<Vec<AccountActivity>, AlpacaError> {
        let mut all_activities: Vec<AccountActivity> = Vec::new();
        let mut seen_ids: HashSet<String> = HashSet::new();
        let mut seen_tokens: HashSet<String> = HashSet::new();
        let mut page_token: Option<String> = None;

        for _ in 0..MAX_ACTIVITY_PAGES {
            let page = self
                .get_account_activities_page(activity_types, after, page_token.as_deref())
                .await?;
            let page_len = page.len();
            let next_token = page
                .last()
                .map(|a| a.id.clone())
                .filter(|id| !id.is_empty());

            for act in page {
                if act.id.is_empty() || seen_ids.insert(act.id.clone()) {
                    all_activities.push(act);
                }
            }

            if page_len < ACTIVITY_PAGE_SIZE {
                break;
            }
            match next_token {
                Some(tok) if seen_tokens.insert(tok.clone()) => {
                    page_token = Some(tok);
                }
                _ => break,
            }
        }

        Ok(all_activities)
    }
}
