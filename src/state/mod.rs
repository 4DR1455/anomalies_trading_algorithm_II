use crate::model::KalmanState;
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use tracing::{debug, error, info};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TradeRecord {
    pub id: String,
    pub symbol: String,
    pub buy_order_id: String,
    pub sell_order_id: Option<String>,
    pub qty: u32,
    pub buy_price: f64,
    pub sell_price: Option<f64>,
    pub pnl: Option<f64>,
    pub pnl_pct: Option<f64>,
    pub buy_timestamp: String,
    pub sell_timestamp: Option<String>,
    pub status: String, // "open_exit_pending", "completed", "liquidated_stale"
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineState {
    pub kalman: Option<KalmanState>,
    pub active_hunts: Vec<TradeRecord>,
    pub completed_trades: Vec<TradeRecord>,
    pub last_cycle_timestamp: Option<String>,
    pub circuit_breaker_active: bool,
    pub total_realized_pnl: f64,
}

impl Default for EngineState {
    fn default() -> Self {
        Self {
            kalman: None,
            active_hunts: Vec::new(),
            completed_trades: Vec::new(),
            last_cycle_timestamp: None,
            circuit_breaker_active: false,
            total_realized_pnl: 0.0,
        }
    }
}

pub struct StateManager {
    file_path: PathBuf,
    temp_path: PathBuf,
}

impl StateManager {
    pub fn new<P: AsRef<Path>>(data_dir: P) -> Self {
        let dir = data_dir.as_ref();
        if !dir.exists() {
            let _ = fs::create_dir_all(dir);
        }
        Self {
            file_path: dir.join("state.json"),
            temp_path: dir.join("state.json.tmp"),
        }
    }

    pub fn load(&self) -> EngineState {
        if self.file_path.exists() {
            match fs::read_to_string(&self.file_path) {
                Ok(content) => match serde_json::from_str::<EngineState>(&content) {
                    Ok(state) => {
                        info!("Successfully restored persistent state from {:?}", self.file_path);
                        return state;
                    }
                    Err(e) => {
                        error!("Failed to parse state file: {}. Re-initializing default state.", e);
                    }
                },
                Err(e) => {
                    error!("Failed to read state file: {}. Re-initializing default state.", e);
                }
            }
        }
        EngineState::default()
    }

    /// Save state using atomic write and fsync to prevent corruptions
    pub fn save(&self, state: &EngineState) -> Result<(), String> {
        let json_bytes = serde_json::to_vec_pretty(state)
            .map_err(|e| format!("Serialization error: {}", e))?;

        let mut file = File::create(&self.temp_path)
            .map_err(|e| format!("Failed to create temp state file: {}", e))?;
        file.write_all(&json_bytes)
            .map_err(|e| format!("Failed to write to temp state file: {}", e))?;
        file.sync_all()
            .map_err(|e| format!("Failed to fsync temp state file: {}", e))?;
        drop(file);

        fs::rename(&self.temp_path, &self.file_path)
            .map_err(|e| format!("Failed to atomic rename state file: {}", e))?;

        debug!("Persisted engine state atomically to {:?}", self.file_path);
        Ok(())
    }
}
