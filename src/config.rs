use rust_decimal::Decimal;
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
    #[serde(default)]
    pub backfill_closed_orders: bool,
    pub kalman: KalmanParams,
}

impl StrategyParams {
    pub fn is_crypto(&self) -> bool {
        self.symbol.contains('/')
    }

    pub fn limit_tif(&self) -> &'static str {
        if self.is_crypto() {
            "gtc"
        } else {
            "day"
        }
    }

    pub fn market_tif(&self) -> &'static str {
        if self.is_crypto() {
            "gtc"
        } else {
            "day"
        }
    }

    pub fn min_margin_decimal(&self) -> Decimal {
        crate::model::pricing::decimal_from_f64_clean(self.min_margin).unwrap_or(Decimal::ZERO)
    }

    pub fn max_margin_decimal(&self) -> Decimal {
        crate::model::pricing::decimal_from_f64_clean(self.max_margin).unwrap_or(Decimal::ZERO)
    }

    pub fn max_shares_decimal(&self) -> Decimal {
        crate::model::pricing::decimal_from_f64_clean(self.max_shares).unwrap_or(Decimal::ZERO)
    }
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
    pub fn load<P: AsRef<Path>>(
        params_path: P,
        env_path: P,
        data_dir: &str,
    ) -> Result<Self, String> {
        let params_content = fs::read_to_string(params_path.as_ref()).map_err(|e| {
            format!(
                "Failed to read params file {:?}: {}",
                params_path.as_ref(),
                e
            )
        })?;
        let params: StrategyParams = serde_json::from_str(&params_content)
            .map_err(|e| format!("Failed to parse params.json: {}", e))?;

        Self::validate_params(&params)?;

        let credentials = Self::load_credentials(env_path.as_ref())?;

        Ok(Self {
            params,
            credentials,
            data_dir: data_dir.to_string(),
        })
    }

    pub fn validate_params(params: &StrategyParams) -> Result<(), String> {
        if params.symbol.trim().is_empty() {
            return Err("symbol must not be empty".to_string());
        }
        if !params.min_margin.is_finite() || params.min_margin <= 0.0 {
            return Err("min_margin must be finite and strictly positive".to_string());
        }
        if !params.max_margin.is_finite() || params.max_margin <= params.min_margin {
            return Err("max_margin must be finite and strictly greater than min_margin".to_string());
        }
        if !params.level.is_finite() || params.level <= 0.0 {
            return Err("level parameter must be finite and strictly positive".to_string());
        }
        if !params.max_shares.is_finite() || params.max_shares <= 0.0 {
            return Err("max_shares must be finite and strictly greater than zero".to_string());
        }
        if !params.kalman.q_base.is_finite() || params.kalman.q_base <= 0.0 {
            return Err("kalman.q_base must be finite and strictly positive".to_string());
        }
        if !params.kalman.r_base.is_finite() || params.kalman.r_base <= 0.0 {
            return Err("kalman.r_base must be finite and strictly positive".to_string());
        }
        if !params.kalman.spread_weight.is_finite() || params.kalman.spread_weight < 0.0 {
            return Err("kalman.spread_weight must be finite and non-negative".to_string());
        }
        if !params.kalman.adapt_lambda.is_finite() || params.kalman.adapt_lambda <= 0.0 {
            return Err("kalman.adapt_lambda must be finite and strictly positive".to_string());
        }
        Ok(())
    }

    pub fn load_credentials(path: &Path) -> Result<AlpacaCredentials, String> {
        let endpoint = std::env::var("APCA_API_BASE_URL")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let key = std::env::var("APCA_API_KEY_ID")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let secret = std::env::var("APCA_API_SECRET_KEY")
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());

        let content = if (endpoint.is_none() || key.is_none() || secret.is_none()) && path.exists() {
            fs::read_to_string(path)
                .map_err(|e| format!("Failed to read .env file {:?}: {}", path, e))?
        } else {
            String::new()
        };

        Self::parse_credentials_content(&content, endpoint, key, secret)
    }

    pub fn parse_credentials_content(
        content: &str,
        mut endpoint: Option<String>,
        mut key: Option<String>,
        mut secret: Option<String>,
    ) -> Result<AlpacaCredentials, String> {
        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            let trimmed = trimmed
                .strip_prefix("export ")
                .map(str::trim_start)
                .unwrap_or(trimmed);

            // Split on '=' before ':' when '=' precedes ':' (e.g. URL values containing ':' like
            // "APCA_API_BASE_URL=https://..." or "End point=https://..."), while still allowing
            // "Secret: abc==" where ':' precedes base64 '=' padding.
            let eq_pos = trimmed.find('=');
            let colon_pos = trimmed.find(':');
            let split_pos = match (eq_pos, colon_pos) {
                (Some(e), Some(c)) => Some(e.min(c)),
                (Some(e), None) => Some(e),
                (None, Some(c)) => Some(c),
                (None, None) => None,
            };

            let Some(pos) = split_pos else {
                continue;
            };

            let k = trimmed[..pos].trim();
            let v = trimmed[pos + 1..]
                .trim()
                .trim_matches('"')
                .trim_matches('\'')
                .trim();

            if v.is_empty() {
                continue;
            }

            if k.eq_ignore_ascii_case("End point")
                || k.eq_ignore_ascii_case("Endpoint")
                || k.eq_ignore_ascii_case("APCA_API_BASE_URL")
            {
                if endpoint.is_none() {
                    endpoint = Some(v.to_string());
                }
            } else if k.eq_ignore_ascii_case("Key") || k.eq_ignore_ascii_case("APCA_API_KEY_ID") {
                if key.is_none() {
                    key = Some(v.to_string());
                }
            } else if (k.eq_ignore_ascii_case("Secret")
                || k.eq_ignore_ascii_case("APCA_API_SECRET_KEY"))
                && secret.is_none()
            {
                secret = Some(v.to_string());
            }
        }

        let endpoint = endpoint
            .unwrap_or_else(|| "https://paper-api.alpaca.markets/v2".to_string())
            .trim_end_matches('/')
            .to_string();

        let key =
            key.ok_or_else(|| "Missing Alpaca API Key (Key: / APCA_API_KEY_ID)".to_string())?;
        let secret = secret.ok_or_else(|| {
            "Missing Alpaca API Secret (Secret: / APCA_API_SECRET_KEY)".to_string()
        })?;

        Ok(AlpacaCredentials {
            endpoint,
            key,
            secret,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_equals_with_colon_in_url() {
        let env_content = "\
# Alpaca paper credentials
APCA_API_BASE_URL=https://paper-api.alpaca.markets/v2/
APCA_API_KEY_ID=\"PKTEST123\"
APCA_API_SECRET_KEY='secret-val=='
";
        let creds = AppConfig::parse_credentials_content(env_content, None, None, None).unwrap();
        assert_eq!(creds.endpoint, "https://paper-api.alpaca.markets/v2");
        assert_eq!(creds.key, "PKTEST123");
        assert_eq!(creds.secret, "secret-val==");
    }

    #[test]
    fn test_parse_local_env_equals_format() {
        let env_content = "\
End point=https://paper-api.alpaca.markets/v2

Key=PKLOCAL999

Secret=local_secret_xyz
";
        let creds = AppConfig::parse_credentials_content(env_content, None, None, None).unwrap();
        assert_eq!(creds.endpoint, "https://paper-api.alpaca.markets/v2");
        assert_eq!(creds.key, "PKLOCAL999");
        assert_eq!(creds.secret, "local_secret_xyz");
    }

    #[test]
    fn test_parse_colon_format_with_base64_secret() {
        let env_content = "\
End point: https://paper-api.alpaca.markets/v2
Key: PKCOLON123
Secret: abcdef123==
";
        let creds = AppConfig::parse_credentials_content(env_content, None, None, None).unwrap();
        assert_eq!(creds.endpoint, "https://paper-api.alpaca.markets/v2");
        assert_eq!(creds.key, "PKCOLON123");
        assert_eq!(creds.secret, "abcdef123==");
    }
}
