use crate::model::pricing::serde_decimal::{
    deserialize_decimal_or_zero, deserialize_opt_decimal, serialize_decimal_as_f64,
    serialize_opt_decimal_as_f64,
};
use crate::model::KalmanState;
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use tracing::{debug, warn};

/// Exhaustive hunt lifecycle state machine.
/// Illegal state names are unrepresentable at compile time and rejected during deserialization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HuntStatus {
    OpenExitPending,
    ExitSubmitPending,
    ExitPartiallyFilled,
    Completed,
    LiquidatedStale,
    LiquidatedManual,
    LiquidatedOrphan,
    AdoptedClosed,
}

impl HuntStatus {
    pub fn is_active(self) -> bool {
        matches!(
            self,
            HuntStatus::OpenExitPending
                | HuntStatus::ExitSubmitPending
                | HuntStatus::ExitPartiallyFilled
        )
    }

    pub fn is_terminal(self) -> bool {
        !self.is_active()
    }

    /// Matches `CLOSED_TRADE_STATUSES` in `dashboard/metrics.py`.
    /// `AdoptedClosed` is terminal (not active) but excluded from strategy trade PnL/win-rate stats.
    pub fn is_closed_strategy_trade(self) -> bool {
        matches!(
            self,
            HuntStatus::Completed
                | HuntStatus::LiquidatedStale
                | HuntStatus::LiquidatedManual
                | HuntStatus::LiquidatedOrphan
        )
    }

    pub fn as_str(self) -> &'static str {
        match self {
            HuntStatus::OpenExitPending => "open_exit_pending",
            HuntStatus::ExitSubmitPending => "exit_submit_pending",
            HuntStatus::ExitPartiallyFilled => "exit_partially_filled",
            HuntStatus::Completed => "completed",
            HuntStatus::LiquidatedStale => "liquidated_stale",
            HuntStatus::LiquidatedManual => "liquidated_manual",
            HuntStatus::LiquidatedOrphan => "liquidated_orphan",
            HuntStatus::AdoptedClosed => "adopted_closed",
        }
    }
}

impl fmt::Display for HuntStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Persistent hunt/trade record 100% JSON-schema compatible with `data/hunts_history.json`
/// and the read-only Python Waitress dashboard (`dashboard/app.py`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeRecord {
    pub id: String,
    pub symbol: String,
    #[serde(default)]
    pub buy_order_id: Option<String>,
    #[serde(default)]
    pub sell_order_id: Option<String>,
    #[serde(
        default,
        deserialize_with = "deserialize_decimal_or_zero",
        serialize_with = "serialize_decimal_as_f64"
    )]
    pub buy_qty: Decimal,
    #[serde(
        default,
        deserialize_with = "deserialize_decimal_or_zero",
        serialize_with = "serialize_decimal_as_f64"
    )]
    pub qty: Decimal,
    #[serde(
        default,
        deserialize_with = "deserialize_decimal_or_zero",
        serialize_with = "serialize_decimal_as_f64"
    )]
    pub filled_exit_qty: Decimal,
    #[serde(
        default,
        deserialize_with = "deserialize_decimal_or_zero",
        serialize_with = "serialize_decimal_as_f64"
    )]
    pub buy_price: Decimal,
    #[serde(
        default,
        deserialize_with = "deserialize_opt_decimal",
        serialize_with = "serialize_opt_decimal_as_f64"
    )]
    pub sell_price: Option<Decimal>,
    #[serde(
        default,
        deserialize_with = "deserialize_opt_decimal",
        serialize_with = "serialize_opt_decimal_as_f64"
    )]
    pub pnl: Option<Decimal>,
    #[serde(
        default,
        deserialize_with = "deserialize_opt_decimal",
        serialize_with = "serialize_opt_decimal_as_f64"
    )]
    pub pnl_pct: Option<Decimal>,
    #[serde(
        default,
        deserialize_with = "deserialize_opt_decimal",
        serialize_with = "serialize_opt_decimal_as_f64"
    )]
    pub gross_pnl: Option<Decimal>,
    #[serde(
        default,
        deserialize_with = "deserialize_opt_decimal",
        serialize_with = "serialize_opt_decimal_as_f64"
    )]
    pub fees_usd: Option<Decimal>,
    #[serde(default)]
    pub buy_timestamp: String,
    #[serde(default)]
    pub sell_timestamp: Option<String>,
    pub status: HuntStatus,
    #[serde(default)]
    pub adopted_from_broker: bool,
    #[serde(default)]
    pub recovered_orphan: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buy_fee_already_deducted: Option<bool>,
}

/// Trims hunt history to the latest `max_terminal` terminal hunts while ALWAYS preserving
/// every active hunt in chronological order so pending exits can never be silently evicted.
pub fn trim_hunts_preserving_active(
    hunts: &[TradeRecord],
    max_terminal: usize,
) -> Vec<TradeRecord> {
    if hunts.is_empty() {
        return Vec::new();
    }

    let mut active_indices = HashSet::new();
    let mut terminal_indices = Vec::new();

    for (idx, h) in hunts.iter().enumerate() {
        if h.status.is_active() {
            active_indices.insert(idx);
        } else {
            terminal_indices.push(idx);
        }
    }

    let keep_start = terminal_indices.len().saturating_sub(max_terminal);
    let kept_terminal: HashSet<usize> = if max_terminal > 0 {
        terminal_indices[keep_start..].iter().copied().collect()
    } else {
        HashSet::new()
    };

    hunts
        .iter()
        .enumerate()
        .filter(|(idx, _)| active_indices.contains(idx) || kept_terminal.contains(idx))
        .map(|(_, h)| h.clone())
        .collect()
}

/// Crash-safe atomic JSON write: write-temp -> flush -> `sync_all()` -> `0o644` (unix) -> `fs::rename()`.
pub fn atomic_write_json<T: Serialize>(path: &Path, data: &T) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create directory {:?}: {}", parent, e))?;
        }
    }

    let json_bytes = serde_json::to_vec_pretty(data)
        .map_err(|e| format!("JSON serialization error for {:?}: {}", path, e))?;

    let temp_path = PathBuf::from(format!("{}.tmp", path.display()));

    let mut file = File::create(&temp_path)
        .map_err(|e| format!("Failed to create temp file {:?}: {}", temp_path, e))?;
    file.write_all(&json_bytes)
        .map_err(|e| format!("Failed to write temp file {:?}: {}", temp_path, e))?;
    file.flush()
        .map_err(|e| format!("Failed to flush temp file {:?}: {}", temp_path, e))?;
    file.sync_all()
        .map_err(|e| format!("Failed to fsync temp file {:?}: {}", temp_path, e))?;
    drop(file);

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let perms = fs::Permissions::from_mode(0o644);
        fs::set_permissions(&temp_path, perms)
            .map_err(|e| format!("Failed to set 0644 permissions on {:?}: {}", temp_path, e))?;
    }

    fs::rename(&temp_path, path)
        .map_err(|e| format!("Failed to atomically rename {:?} -> {:?}: {}", temp_path, path, e))?;

    debug!("Atomically persisted {:?}", path);
    Ok(())
}

/// Crash-safe persistent store for `hunts_history.json`, `metrics.json`, `telemetry.json`, and `kalman_state.json`.
#[derive(Debug, Clone)]
pub struct StateStore {
    data_dir: PathBuf,
    hunts_path: PathBuf,
    metrics_path: PathBuf,
    telemetry_path: PathBuf,
    kalman_path: PathBuf,
}

impl StateStore {
    pub fn new<P: AsRef<Path>>(data_dir: P) -> Self {
        let dir = data_dir.as_ref().to_path_buf();
        if !dir.exists() {
            let _ = fs::create_dir_all(&dir);
        }
        Self {
            hunts_path: dir.join("hunts_history.json"),
            metrics_path: dir.join("metrics.json"),
            telemetry_path: dir.join("telemetry.json"),
            kalman_path: dir.join("kalman_state.json"),
            data_dir: dir,
        }
    }

    pub fn data_dir(&self) -> &Path {
        &self.data_dir
    }

    pub fn hunts_path(&self) -> &Path {
        &self.hunts_path
    }

    pub fn metrics_path(&self) -> &Path {
        &self.metrics_path
    }

    pub fn telemetry_path(&self) -> &Path {
        &self.telemetry_path
    }

    pub fn kalman_path(&self) -> &Path {
        &self.kalman_path
    }

    /// Loads hunts from `hunts_history.json` (or falls back to `telemetry.json`'s `trades` array).
    /// Always preserves all active hunts plus the last `max_terminal` terminal hunts.
    pub fn load_hunts(&self, max_terminal: usize) -> Vec<TradeRecord> {
        if self.hunts_path.exists() {
            match fs::read_to_string(&self.hunts_path) {
                Ok(content) => match serde_json::from_str::<Vec<TradeRecord>>(&content) {
                    Ok(hunts) if !hunts.is_empty() => {
                        return trim_hunts_preserving_active(&hunts, max_terminal);
                    }
                    Ok(_) => {}
                    Err(e) => {
                        warn!("Failed to parse {:?}: {}", self.hunts_path, e);
                    }
                },
                Err(e) => {
                    warn!("Failed to read {:?}: {}", self.hunts_path, e);
                }
            }
        }

        // Fallback: recover trades array from telemetry.json if present
        if self.telemetry_path.exists() {
            if let Ok(content) = fs::read_to_string(&self.telemetry_path) {
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(&content) {
                    if let Some(trades_val) = val.get("trades") {
                        if let Ok(trades) =
                            serde_json::from_value::<Vec<TradeRecord>>(trades_val.clone())
                        {
                            if !trades.is_empty() {
                                let trimmed = trim_hunts_preserving_active(&trades, max_terminal);
                                let _ = self.save_hunts(&trimmed, max_terminal);
                                return trimmed;
                            }
                        }
                    }
                }
            }
        }

        Vec::new()
    }

    pub fn save_hunts(&self, hunts: &[TradeRecord], max_terminal: usize) -> Result<(), String> {
        let trimmed = trim_hunts_preserving_active(hunts, max_terminal);
        atomic_write_json(&self.hunts_path, &trimmed)
    }

    pub fn load_kalman_state(&self) -> Option<KalmanState> {
        if !self.kalman_path.exists() {
            return None;
        }
        match fs::read_to_string(&self.kalman_path) {
            Ok(content) => match serde_json::from_str::<KalmanState>(&content) {
                Ok(state) if state.is_valid() => Some(state),
                Ok(_) => {
                    warn!("Ignoring invalid/corrupted Kalman state in {:?}", self.kalman_path);
                    None
                }
                Err(e) => {
                    warn!("Failed to parse {:?}: {}", self.kalman_path, e);
                    None
                }
            },
            Err(e) => {
                warn!("Failed to read {:?}: {}", self.kalman_path, e);
                None
            }
        }
    }

    pub fn save_kalman_state(&self, state: &KalmanState) -> Result<(), String> {
        atomic_write_json(&self.kalman_path, state)
    }

    pub fn save_metrics<T: Serialize>(&self, metrics: &T) -> Result<(), String> {
        atomic_write_json(&self.metrics_path, metrics)
    }

    pub fn save_telemetry<T: Serialize>(&self, snapshot: &T) -> Result<(), String> {
        atomic_write_json(&self.telemetry_path, snapshot)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;
    use tempfile::tempdir;

    #[test]
    fn test_hunt_status_classification_and_serde() {
        assert!(HuntStatus::OpenExitPending.is_active());
        assert!(HuntStatus::ExitSubmitPending.is_active());
        assert!(HuntStatus::ExitPartiallyFilled.is_active());

        assert!(HuntStatus::Completed.is_terminal());
        assert!(HuntStatus::LiquidatedStale.is_terminal());
        assert!(HuntStatus::LiquidatedManual.is_terminal());
        assert!(HuntStatus::LiquidatedOrphan.is_terminal());
        assert!(HuntStatus::AdoptedClosed.is_terminal());

        assert!(HuntStatus::Completed.is_closed_strategy_trade());
        assert!(HuntStatus::LiquidatedStale.is_closed_strategy_trade());
        assert!(!HuntStatus::AdoptedClosed.is_closed_strategy_trade());

        assert_eq!(
            serde_json::to_string(&HuntStatus::AdoptedClosed).unwrap(),
            "\"adopted_closed\""
        );
    }

    #[test]
    fn test_legacy_hunts_history_json_roundtrip_and_trimming() {
        let legacy_json = r#"[
            {
                "id": "64dc0008",
                "symbol": "SOL/USD",
                "buy_order_id": "ba1e3bc2",
                "sell_order_id": "da4b6a70",
                "buy_qty": 111.4638,
                "qty": 111.2966,
                "filled_exit_qty": 0.0,
                "buy_price": 74.48,
                "sell_price": 74.73,
                "pnl": null,
                "pnl_pct": null,
                "gross_pnl": null,
                "fees_usd": null,
                "buy_timestamp": "2026-11-20T09:00:00Z",
                "sell_timestamp": null,
                "status": "open_exit_pending"
            }
        ]"#;

        let parsed: Vec<TradeRecord> = serde_json::from_str(legacy_json).unwrap();
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].status, HuntStatus::OpenExitPending);
        assert_eq!(parsed[0].qty, Decimal::from_str("111.2966").unwrap());
        assert!(!parsed[0].adopted_from_broker);
        assert!(parsed[0].buy_fee_already_deducted.is_none());

        let tmp = tempdir().unwrap();
        let store = StateStore::new(tmp.path());
        store.save_hunts(&parsed, 50).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(store.hunts_path()).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o644);
        }

        let reloaded = store.load_hunts(50);
        assert_eq!(reloaded.len(), 1);
        assert_eq!(reloaded[0].id, "64dc0008");
        assert_eq!(reloaded[0].qty, Decimal::from_str("111.2966").unwrap());
    }
}
