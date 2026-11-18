use crate::state::TradeRecord;
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetrySnapshot {
    pub timestamp: String,
    pub symbol: String,
    pub best_bid: f64,
    pub best_ask: f64,
    pub market_mid: f64,
    pub fair_price: f64,
    pub undervaluation_margin: f64,
    pub min_margin: f64,
    pub max_margin: f64,
    pub circuit_breaker_active: bool,
    pub active_hunts_count: usize,
    pub completed_trades_count: usize,
    pub total_realized_pnl: f64,
    pub trades: Vec<TradeRecord>,
}

pub struct TelemetryWriter {
    file_path: PathBuf,
    temp_path: PathBuf,
}

impl TelemetryWriter {
    pub fn new<P: AsRef<Path>>(data_dir: P) -> Self {
        let dir = data_dir.as_ref();
        if !dir.exists() {
            let _ = fs::create_dir_all(dir);
        }
        Self {
            file_path: dir.join("telemetry.json"),
            temp_path: dir.join("telemetry.json.tmp"),
        }
    }

    pub fn write_snapshot(&self, snapshot: &TelemetrySnapshot) -> Result<(), String> {
        let json_bytes = serde_json::to_vec_pretty(snapshot)
            .map_err(|e| format!("Telemetry serialization error: {}", e))?;

        let mut file = File::create(&self.temp_path)
            .map_err(|e| format!("Failed to create temp telemetry file: {}", e))?;
        file.write_all(&json_bytes)
            .map_err(|e| format!("Failed to write to temp telemetry file: {}", e))?;
        file.sync_all()
            .map_err(|e| format!("Failed to fsync telemetry file: {}", e))?;
        drop(file);

        fs::rename(&self.temp_path, &self.file_path)
            .map_err(|e| format!("Failed to rename telemetry file: {}", e))?;

        Ok(())
    }
}
