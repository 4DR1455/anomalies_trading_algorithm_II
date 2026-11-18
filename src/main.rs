mod alpaca;
mod config;
mod engine;
mod model;
mod state;
mod telemetry;

use config::AppConfig;
use engine::TradingEngine;
use std::env;
use std::path::Path;
use tokio::signal;
use tokio::sync::broadcast;
use tracing::{error, info, Level};
use tracing_subscriber::FmtSubscriber;

#[tokio::main]
async fn main() {
    let subscriber = FmtSubscriber::builder()
        .with_max_level(Level::INFO)
        .with_target(false)
        .with_thread_ids(false)
        .finish();

    let _ = tracing::subscriber::set_global_default(subscriber);

    info!("Initializing Anomalies Trading System v2.0 (Resilient Autonomous Engine)...");

    let args: Vec<String> = env::args().collect();
    let mut params_file = "params.json".to_string();
    let mut env_file = ".env".to_string();
    let mut data_dir = "data".to_string();

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--params" if i + 1 < args.len() => {
                params_file = args[i + 1].clone();
                i += 2;
            }
            "--env" if i + 1 < args.len() => {
                env_file = args[i + 1].clone();
                i += 2;
            }
            "--data" if i + 1 < args.len() => {
                data_dir = args[i + 1].clone();
                i += 2;
            }
            _ => {
                i += 1;
            }
        }
    }

    if !Path::new(&params_file).exists() {
        error!("Parameters file not found at: {}", params_file);
        std::process::exit(1);
    }

    let config = match AppConfig::load(&params_file, &env_file, &data_dir) {
        Ok(cfg) => cfg,
        Err(e) => {
            error!("Configuration loading error: {}", e);
            std::process::exit(1);
        }
    };

    info!(
        "Configuration verified: Symbol='{}', Level={}, MaxShares={}, MinMargin={:.4}%, MaxMargin={:.4}%",
        config.params.symbol,
        config.params.level,
        config.params.max_shares,
        config.params.min_margin * 100.0,
        config.params.max_margin * 100.0
    );

    let (shutdown_tx, shutdown_rx) = broadcast::channel(1);

    // Spawn signal listener
    let shutdown_sender = shutdown_tx.clone();
    tokio::spawn(async move {
        match signal::ctrl_c().await {
            Ok(()) => {
                info!("SIGINT / Ctrl-C received, requesting engine shutdown...");
                let _ = shutdown_sender.send(());
            }
            Err(err) => {
                error!("Unable to listen for shutdown signal: {}", err);
            }
        }
    });

    let mut engine = match TradingEngine::new(config) {
        Ok(eng) => eng,
        Err(e) => {
            error!("Failed to create Trading Engine: {}", e);
            std::process::exit(1);
        }
    };

    engine.start(shutdown_rx).await;

    info!("Anomalies Trading System terminated cleanly. Good bye.");
}
