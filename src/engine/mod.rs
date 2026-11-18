pub mod hunt;
pub mod order_hygiene;

use crate::alpaca::AlpacaClient;
use crate::config::AppConfig;
use crate::model::{calculate_shares_amount, AdaptiveKalmanFilter, SignalType};
use crate::state::{EngineState, StateManager};
use crate::telemetry::{TelemetrySnapshot, TelemetryWriter};
use chrono::Utc;
use hunt::HuntExecutor;
use order_hygiene::OrderHygieneManager;
use std::time::Duration;
use tokio::time::{sleep, Instant};
use tracing::{error, info, warn};

pub struct TradingEngine {
    config: AppConfig,
    client: AlpacaClient,
    filter: AdaptiveKalmanFilter,
    state_mgr: StateManager,
    telemetry: TelemetryWriter,
    state: EngineState,
}

impl TradingEngine {
    pub fn new(config: AppConfig) -> Result<Self, String> {
        let client = AlpacaClient::new(&config.credentials)
            .map_err(|e| format!("Alpaca client initialization failed: {}", e))?;

        let state_mgr = StateManager::new(&config.data_dir);
        let telemetry = TelemetryWriter::new(&config.data_dir);
        let state = state_mgr.load();

        let filter = AdaptiveKalmanFilter::new(
            config.params.kalman.clone(),
            config.params.min_margin,
            config.params.max_margin,
            state.kalman.clone(),
        );

        Ok(Self {
            config,
            client,
            filter,
            state_mgr,
            telemetry,
            state,
        })
    }

    /// Run the main continuous trading cycle (every 30 seconds)
    pub async fn start(&mut self, mut shutdown_rx: tokio::sync::broadcast::Receiver<()>) {
        let cycle_duration = Duration::from_secs(self.config.params.cycle_seconds);
        info!(
            "Trading Engine started for symbol {} with cycle interval {}s",
            self.config.params.symbol, self.config.params.cycle_seconds
        );

        loop {
            let cycle_start = Instant::now();

            tokio::select! {
                _ = shutdown_rx.recv() => {
                    info!("Graceful shutdown signal received. Saving state and terminating trading engine.");
                    self.state.kalman = Some(self.filter.state().clone());
                    let _ = self.state_mgr.save(&self.state);
                    break;
                }
                _ = self.step_cycle() => {
                    // Step cycle completed
                }
            }

            let elapsed = cycle_start.elapsed();
            if elapsed < cycle_duration {
                let remaining = cycle_duration - elapsed;
                tokio::select! {
                    _ = shutdown_rx.recv() => {
                        info!("Graceful shutdown signal received during wait interval.");
                        self.state.kalman = Some(self.filter.state().clone());
                        let _ = self.state_mgr.save(&self.state);
                        break;
                    }
                    _ = sleep(remaining) => {}
                }
            }
        }
    }

    async fn step_cycle(&mut self) {
        let symbol = &self.config.params.symbol;
        info!("--- Starting 30s cycle for {} ---", symbol);

        // 1. Fetch latest quote
        let quote = match self.client.get_latest_quote(symbol).await {
            Ok(q) => q,
            Err(e) => {
                warn!("Failed to fetch quote for {}: {}", symbol, e);
                return;
            }
        };

        let bid = quote.bid_price;
        let ask = quote.ask_price;
        let mid = 0.5 * (bid + ask);

        // 2. Update Kalman filter
        let signal = self.filter.update(bid, ask);
        let fair_price = self.filter.state().x_hat;

        match &signal {
            SignalType::CircuitBreakerHalt {
                undervaluation_margin,
                max_margin,
                reason,
            } => {
                warn!(
                    "CIRCUIT BREAKER TRIGGERED: margin {:.4}% exceeds max_margin {:.4}%. Halting entries! ({})",
                    undervaluation_margin * 100.0,
                    max_margin * 100.0,
                    reason
                );
                self.state.circuit_breaker_active = true;
            }
            SignalType::BuyAnomaly {
                undervaluation_margin,
                fair_price,
                market_mid,
            } => {
                self.state.circuit_breaker_active = false;
                let shares = calculate_shares_amount(
                    *undervaluation_margin,
                    self.config.params.min_margin,
                    self.config.params.max_margin,
                    self.config.params.level,
                    self.config.params.max_shares,
                );

                info!(
                    "Undervaluation anomaly detected! Margin: {:.4}%, Fair: ${:.2}, Mid: ${:.2} -> Sizing: {} shares",
                    undervaluation_margin * 100.0, fair_price, market_mid, shares
                );

                if shares > 0 {
                    let executor = HuntExecutor::new(
                        &self.client,
                        symbol,
                        self.config.params.hidden_mm_timeout_seconds,
                    );
                    match executor.execute_hunt(bid, ask, *fair_price, shares).await {
                        Ok(Some(trade)) => {
                            self.state.active_hunts.push(trade);
                        }
                        Ok(None) => {}
                        Err(e) => {
                            error!("Error during Market Hunt execution: {}", e);
                        }
                    }
                }
            }
            SignalType::None => {
                self.state.circuit_breaker_active = false;
                info!(
                    "Quote: Bid ${:.2} / Ask ${:.2} (Mid ${:.2}) | Kalman Fair Price: ${:.2} | No Anomaly",
                    bid, ask, mid, fair_price
                );
            }
        }

        // 3. Run Order Hygiene & Stale Order Cleanup
        let hygiene = OrderHygieneManager::new(
            &self.client,
            symbol,
            self.config.params.stale_order_max_age_seconds,
        );
        hygiene
            .run_hygiene_cycle(
                &mut self.state.active_hunts,
                &mut self.state.completed_trades,
                &mut self.state.total_realized_pnl,
                fair_price,
            )
            .await;

        // 4. Update Engine State & Save Atomically
        self.state.kalman = Some(self.filter.state().clone());
        self.state.last_cycle_timestamp = Some(Utc::now().to_rfc3339());
        if let Err(e) = self.state_mgr.save(&self.state) {
            error!("Failed to persist state: {}", e);
        }

        // 5. Emit Telemetry Snapshot for Python Dashboard
        let current_margin = if fair_price > 0.0 {
            (fair_price - mid) / fair_price
        } else {
            0.0
        };

        let mut all_trades = self.state.completed_trades.clone();
        all_trades.extend(self.state.active_hunts.clone());

        let snapshot = TelemetrySnapshot {
            timestamp: Utc::now().to_rfc3339(),
            symbol: symbol.clone(),
            best_bid: bid,
            best_ask: ask,
            market_mid: mid,
            fair_price,
            undervaluation_margin: current_margin,
            min_margin: self.config.params.min_margin,
            max_margin: self.config.params.max_margin,
            circuit_breaker_active: self.state.circuit_breaker_active,
            active_hunts_count: self.state.active_hunts.len(),
            completed_trades_count: self.state.completed_trades.len(),
            total_realized_pnl: self.state.total_realized_pnl,
            trades: all_trades,
        };

        if let Err(e) = self.telemetry.write_snapshot(&snapshot) {
            warn!("Failed to write telemetry: {}", e);
        }
    }
}
