use anomalies_trading_engine::config::AppConfig;
use anomalies_trading_engine::engine::TradingEngine;
use std::env;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::signal;
use tokio::sync::broadcast;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(false)
        .with_thread_ids(false)
        .init();

    info!("Initializing Anomalies Trading Engine v2.1 (Rust 2021 + Exact Decimal + Reconciled)...");

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
        "Configuration verified: Symbol='{}', Level={}, MaxShares={}, MinMargin={:.4}%, MaxMargin={:.4}%, Cycle={}s, StaleMax={}s",
        config.params.symbol,
        config.params.level,
        config.params.max_shares,
        config.params.min_margin * 100.0,
        config.params.max_margin * 100.0,
        config.params.cycle_seconds,
        config.params.stale_order_max_age_seconds
    );

    let (shutdown_tx, shutdown_rx) = broadcast::channel::<()>(2);
    let shutdown_flag = Arc::new(AtomicBool::new(false));

    // Spawn graceful SIGINT + SIGTERM handler
    let tx_signal = shutdown_tx.clone();
    let flag_signal = Arc::clone(&shutdown_flag);
    tokio::spawn(async move {
        wait_for_shutdown_signal().await;
        flag_signal.store(true, Ordering::SeqCst);
        let _ = tx_signal.send(());
    });

    let mut engine = match TradingEngine::new(config) {
        Ok(eng) => eng,
        Err(e) => {
            error!("Failed to initialize Trading Engine: {}", e);
            std::process::exit(1);
        }
    };

    engine.start(shutdown_rx, shutdown_flag).await;

    info!("Anomalies Trading Engine terminated cleanly.");
}

async fn wait_for_shutdown_signal() {
    let ctrl_c = async {
        match signal::ctrl_c().await {
            Ok(()) => info!("SIGINT / Ctrl-C received, initiating graceful shutdown..."),
            Err(err) => error!("Unable to listen for SIGINT: {}", err),
        }
    };

    #[cfg(unix)]
    let terminate = async {
        match signal::unix::signal(signal::unix::SignalKind::terminate()) {
            Ok(mut sigterm) => {
                sigterm.recv().await;
                info!("SIGTERM received, initiating graceful shutdown...");
            }
            Err(err) => {
                error!("Unable to install SIGTERM handler: {}", err);
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
}
