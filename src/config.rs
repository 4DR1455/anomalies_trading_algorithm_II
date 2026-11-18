use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KalmanParams {
    pub q_base: f64,
    pub r_base: f64,
    pub spread_weight: f64,
    pub adapt_lambda: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrategyParams {
    pub symbol: String,
    pub level: f64,
    pub max_shares: f64,
    pub min_margin: f64,
    pub max_margin: f64,
    #[serde(default = "default_cycle_seconds")]
    pub cycle_seconds: u64,
    #[serde(default = "default_hidden_mm_timeout")]
    pub hidden_mm_timeout_seconds: u64,
    #[serde(default = "default_stale_order_max_age")]
    pub stale_order_max_age_seconds: u64,
    pub kalman: KalmanParams,
}

fn default_cycle_seconds() -> u64 {
    30
}

fn default_hidden_mm_timeout() -> u64 {
    20
}

fn default_stale_order_max_age() -> u64 {
    43200
}

#[derive(Debug, Clone)]
pub struct AlpacaCredentials {
    pub endpoint: String,
    pub key: String,
    pub secret: String,
}

#[derive(Debug, Clone)]
pub struct AppConfig {
    pub params: StrategyParams,
    pub credentials: AlpacaCredentials,
    pub data_dir: String,
}

impl AppConfig {
    pub fn load<P: AsRef<Path>>(params_path: P, env_path: P, data_dir: &str) -> Result<Self, String> {
        let params_content = fs::read_to_string(params_path.as_ref())
            .map_err(|e| format!("Failed to read params file {:?}: {}", params_path.as_ref(), e))?;
        let params: StrategyParams = serde_json::from_str(&params_content)
            .map_err(|e| format!("Failed to parse params.json: {}", e))?;

        if params.min_margin <= 0.0 {
            return Err("min_margin must be strictly positive".to_string());
        }
        if params.max_margin <= params.min_margin {
            return Err("max_margin must be strictly greater than min_margin".to_string());
        }
        if params.level <= 0.0 {
            return Err("level parameter must be strictly positive".to_string());
        }
        if params.max_shares <= 0.0 {
            return Err("max_shares must be strictly greater than zero".to_string());
        }

        let credentials = Self::load_credentials(env_path.as_ref())?;

        Ok(Self {
            params,
            credentials,
            data_dir: data_dir.to_string(),
        })
    }

    fn load_credentials(path: &Path) -> Result<AlpacaCredentials, String> {
        let mut endpoint = std::env::var("APCA_API_BASE_URL").ok();
        let mut key = std::env::var("APCA_API_KEY_ID").ok();
        let mut secret = std::env::var("APCA_API_SECRET_KEY").ok();

        if (endpoint.is_none() || key.is_none() || secret.is_none()) && path.exists() {
            let content = fs::read_to_string(path)
                .map_err(|e| format!("Failed to read .env file {:?}: {}", path, e))?;

            for line in content.lines() {
                let trimmed = line.trim();
                if trimmed.is_empty() || trimmed.starts_with('#') {
                    continue;
                }

                // Format 1: "Key: VALUE" or "Secret: VALUE" or "End point: VALUE"
                if let Some(pos) = trimmed.find(':') {
                    let k = trimmed[..pos].trim();
                    let v = trimmed[pos + 1..].trim();
                    if k.eq_ignore_ascii_case("End point") || k.eq_ignore_ascii_case("Endpoint") {
                        if endpoint.is_none() && !v.is_empty() {
                            endpoint = Some(v.to_string());
                        }
                    } else if k.eq_ignore_ascii_case("Key") || k.eq_ignore_ascii_case("APCA_API_KEY_ID") {
                        if key.is_none() && !v.is_empty() {
                            key = Some(v.to_string());
                        }
                    } else if k.eq_ignore_ascii_case("Secret") || k.eq_ignore_ascii_case("APCA_API_SECRET_KEY") {
                        if secret.is_none() && !v.is_empty() {
                            secret = Some(v.to_string());
                        }
                    }
                }

                // Format 2: "KEY=VALUE"
                if let Some(pos) = trimmed.find('=') {
                    let k = trimmed[..pos].trim();
                    let v = trimmed[pos + 1..].trim().trim_matches('"').trim_matches('\'');
                    if k == "APCA_API_BASE_URL" && endpoint.is_none() {
                        endpoint = Some(v.to_string());
                    } else if k == "APCA_API_KEY_ID" && key.is_none() {
                        key = Some(v.to_string());
                    } else if k == "APCA_API_SECRET_KEY" && secret.is_none() {
                        secret = Some(v.to_string());
                    }
                }
            }
        }

        let endpoint = endpoint.unwrap_or_else(|| "https://paper-api.alpaca.markets/v2".to_string());
        // Clean trailing slashes and /v2 if needed
        let endpoint = endpoint.trim_end_matches('/').to_string();

        let key = key.ok_or_else(|| "Missing Alpaca API Key (Key: / APCA_API_KEY_ID)".to_string())?;
        let secret = secret.ok_or_else(|| "Missing Alpaca API Secret (Secret: / APCA_API_SECRET_KEY)".to_string())?;

        Ok(AlpacaCredentials {
            endpoint,
            key,
            secret,
        })
    }
}
