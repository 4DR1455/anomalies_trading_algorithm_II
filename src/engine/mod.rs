pub mod hunt;
pub mod reconciliation;

use crate::alpaca::{AlpacaApi, AlpacaClient, PortfolioHistory};
use crate::config::{AppConfig, StrategyParams};
use crate::engine::hunt::{execute_hunt_with_notional_cap, HuntTiming};
use crate::engine::reconciliation::{
    build_broker_telemetry, calibrate_fee_tracker_from_sell, reconcile_and_maintain_positions,
    ReconciliationTiming,
};
use crate::metrics::{
    compute_performance_metrics, compute_portfolio_history_metrics, PerformanceMetrics,
};
use crate::model::{
    decimal_from_f64_clean, decimal_to_f64, AdaptiveKalmanFilter, FeeTracker,
    RegimeNotionalController, SignalType,
};
use crate::state::{trim_hunts_preserving_active, HuntStatus, StateStore, TradeRecord};
use crate::telemetry::TelemetrySnapshot;
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use std::collections::HashMap;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::time::{sleep, Instant};
use tracing::{debug, error, info, warn};

pub const MAX_TERMINAL_HUNTS: usize = 50;
pub const PORTFOLIO_HISTORY_REFRESH_SECS: u64 = 600;
pub const DEFAULT_RISK_FREE_RATE: f64 = 0.04;
pub const DEFAULT_BASE_EQUITY: f64 = 100_000.0;

/// Combined timing parameters for reconciliation and market hunt execution.
#[derive(Debug, Clone, Copy)]
pub struct EngineTiming {
    pub reconciliation: ReconciliationTiming,
    pub hunt: HuntTiming,
}

impl EngineTiming {
    pub fn from_params(params: &StrategyParams) -> Self {
        Self {
            reconciliation: ReconciliationTiming::default(),
            hunt: HuntTiming::from_params(params),
        }
    }

    pub fn fast_for_tests() -> Self {
        Self {
            reconciliation: ReconciliationTiming::fast_for_tests(),
            hunt: HuntTiming::fast_for_tests(),
        }
    }
}

/// Autonomous mean-reversion trading engine parameterized over `AlpacaApi` for deterministic testing.
pub struct TradingEngine<C: AlpacaApi> {
    params: StrategyParams,
    client: C,
    filter: AdaptiveKalmanFilter,
    regime_ctrl: RegimeNotionalController,
    store: StateStore,
    fee_tracker: FeeTracker,
    trades_history: Vec<TradeRecord>,
    portfolio_history: Option<PortfolioHistory>,
    last_ph_fetch: Option<Instant>,
    circuit_breaker_active: bool,
    timing: EngineTiming,
}

impl TradingEngine<AlpacaClient> {
    /// Production constructor building an authenticated `AlpacaClient` from `AppConfig`.
    pub fn new(config: AppConfig) -> Result<Self, String> {
        let client = AlpacaClient::new(&config.credentials)
            .map_err(|e| format!("Alpaca client initialization failed: {}", e))?;
        let timing = EngineTiming::from_params(&config.params);
        Ok(Self::with_client_and_timing(
            config.params,
            &config.data_dir,
            client,
            timing,
        ))
    }
}

impl<C: AlpacaApi> TradingEngine<C> {
    /// Generic constructor accepting any `AlpacaApi` implementation (live or mock).
    pub fn with_client<P: AsRef<Path>>(params: StrategyParams, data_dir: P, client: C) -> Self {
        let timing = EngineTiming::from_params(&params);
        Self::with_client_and_timing(params, data_dir, client, timing)
    }

    /// Generic constructor with explicit polling/settlement timing (ideal for fast integration tests).
    pub fn with_client_and_timing<P: AsRef<Path>>(
        params: StrategyParams,
        data_dir: P,
        client: C,
        timing: EngineTiming,
    ) -> Self {
        let store = StateStore::new(data_dir);
        let trades_history = store.load_hunts(MAX_TERMINAL_HUNTS);
        let initial_kalman = store.load_kalman_state();
        let initial_regime = store.load_regime_state();

        let filter = AdaptiveKalmanFilter::new(
            params.kalman.clone(),
            params.min_margin,
            params.max_margin,
            initial_kalman,
        );
        let regime_ctrl =
            RegimeNotionalController::new(params.regime_sizing.clone(), initial_regime);

        Self {
            params,
            client,
            filter,
            regime_ctrl,
            store,
            fee_tracker: FeeTracker::new(),
            trades_history,
            portfolio_history: None,
            last_ph_fetch: None,
            circuit_breaker_active: false,
            timing,
        }
    }

    pub fn params(&self) -> &StrategyParams {
        &self.params
    }

    pub fn client(&self) -> &C {
        &self.client
    }

    pub fn filter(&self) -> &AdaptiveKalmanFilter {
        &self.filter
    }

    pub fn filter_mut(&mut self) -> &mut AdaptiveKalmanFilter {
        &mut self.filter
    }

    pub fn regime_ctrl(&self) -> &RegimeNotionalController {
        &self.regime_ctrl
    }

    pub fn regime_ctrl_mut(&mut self) -> &mut RegimeNotionalController {
        &mut self.regime_ctrl
    }

    pub fn store(&self) -> &StateStore {
        &self.store
    }

    pub fn fee_tracker(&self) -> &FeeTracker {
        &self.fee_tracker
    }

    pub fn fee_tracker_mut(&mut self) -> &mut FeeTracker {
        &mut self.fee_tracker
    }

    pub fn trades_history(&self) -> &[TradeRecord] {
        &self.trades_history
    }

    pub fn trades_history_mut(&mut self) -> &mut Vec<TradeRecord> {
        &mut self.trades_history
    }

    /// Performs startup initialization:
    /// 1. Calibrates `FeeTracker` from recent Alpaca `CFEE` activities (crypto only).
    /// 2. Loads official Alpaca `portfolio/history` (1A / 1D) for equity/drawdown charts.
    /// 3. Persists initial `metrics.json`.
    pub async fn initialize(&mut self) {
        if self.params.is_crypto() {
            self.calibrate_fee_tier_from_startup_cfee().await;
        }

        match self.client.get_portfolio_history("1A", "1D").await {
            Ok(ph) => {
                let count = ph.equity.len();
                self.portfolio_history = Some(ph);
                self.last_ph_fetch = Some(Instant::now());
                info!(
                    "[PORTFOLIO HISTORY] Loaded {} official Alpaca daily equity points.",
                    count
                );
            }
            Err(e) => {
                warn!("Could not load initial Alpaca portfolio history: {}", e);
            }
        }

        let now = Utc::now();
        let metrics = self.compute_current_metrics(None, now);
        if let Err(e) = self.store.save_metrics(&metrics) {
            warn!("Failed to persist initial metrics.json: {}", e);
        }

        let tier = self.fee_tracker.current_tier();
        info!(
            "[STARTUP] Loaded {} hunts | Kalman initialized={} (samples={}) | Crypto Fee Tier {} (maker={}, taker={}, exit_floor={}) | Portfolio PnL: ${:+.2}",
            self.trades_history.len(),
            self.filter.is_initialized(),
            self.filter.state().sample_count,
            tier.tier,
            tier.maker_rate,
            tier.taker_rate,
            self.fee_tracker.worst_sell_fee_rate(),
            metrics.total_pnl
        );
    }

    /// Inspects recent `CFEE` activities at startup to calibrate the active Alpaca crypto volume tier.
    /// Always preserves `worst_sell_fee_rate() >= 0.0025` inside `FeeTracker`.
    async fn calibrate_fee_tier_from_startup_cfee(&mut self) {
        let activities = match self.client.get_account_activities("CFEE", None).await {
            Ok(acts) => acts,
            Err(e) => {
                debug!("Startup CFEE fetch skipped: {}", e);
                return;
            }
        };

        // Group CFEE amounts by order_id while preserving most-recent-first encounter order
        let mut order_ids_ordered: Vec<String> = Vec::new();
        let mut fee_by_order: HashMap<String, Decimal> = HashMap::new();
        let mut direct_candidates: Vec<(Decimal, Decimal)> = Vec::new();

        for act in activities {
            if !act.activity_type.is_empty() && !act.activity_type.eq_ignore_ascii_case("CFEE") {
                continue;
            }
            let Some(fee_abs) = act.fee_usd_abs() else {
                continue;
            };
            if let Some(oid) = act.order_id.filter(|s| !s.is_empty()) {
                let entry = fee_by_order.entry(oid.clone()).or_insert_with(|| {
                    order_ids_ordered.push(oid);
                    Decimal::ZERO
                });
                *entry += fee_abs;
            } else if let (Some(qty), Some(price)) = (act.qty, act.price) {
                if qty > Decimal::ZERO && price > Decimal::ZERO {
                    direct_candidates.push((fee_abs, qty * price));
                }
            }
        }

        let tier_before = self.fee_tracker.current_tier();

        // Check up to the 5 most recent CFEE orders for a clean calibration sample (>= $100 notional)
        for oid in order_ids_ordered.into_iter().take(5) {
            let Some(&fee_usd) = fee_by_order.get(&oid) else {
                continue;
            };

            let notional_and_type = match self.client.get_order(&oid).await {
                Ok(ord) => {
                    let px = ord.filled_avg_price.unwrap_or(Decimal::ZERO);
                    if ord.filled_qty > Decimal::ZERO && px > Decimal::ZERO {
                        Some((ord.filled_qty * px, ord.order_type == "market"))
                    } else {
                        None
                    }
                }
                Err(_) => self
                    .trades_history
                    .iter()
                    .find(|t| t.sell_order_id.as_deref() == Some(oid.as_str()))
                    .and_then(|t| {
                        let px = t.sell_price?;
                        if t.qty > Decimal::ZERO && px > Decimal::ZERO {
                            Some((
                                t.qty * px,
                                t.status == crate::state::HuntStatus::LiquidatedStale,
                            ))
                        } else {
                            None
                        }
                    }),
            };

            if let Some((notional, is_market)) = notional_and_type {
                calibrate_fee_tracker_from_sell(&mut self.fee_tracker, fee_usd, notional, is_market);
                if self.fee_tracker.current_tier() != tier_before {
                    return;
                }
            }
        }

        for (fee_usd, notional) in direct_candidates.into_iter().take(5) {
            calibrate_fee_tracker_from_sell(&mut self.fee_tracker, fee_usd, notional, false);
            if self.fee_tracker.current_tier() != tier_before {
                return;
            }
        }
    }

    fn compute_current_metrics(
        &self,
        live_equity: Option<f64>,
        now_dt: DateTime<Utc>,
    ) -> PerformanceMetrics {
        if let Some(ref ph) = self.portfolio_history {
            if !ph.equity.is_empty() {
                return compute_portfolio_history_metrics(
                    ph,
                    live_equity,
                    &self.trades_history,
                    DEFAULT_RISK_FREE_RATE,
                    now_dt,
                );
            }
        }
        compute_performance_metrics(
            &self.trades_history,
            DEFAULT_RISK_FREE_RATE,
            DEFAULT_BASE_EQUITY,
            now_dt,
        )
    }

    /// Runs the continuous 30-second autonomous trading loop until graceful shutdown is signaled.
    pub async fn start(
        &mut self,
        mut shutdown_rx: broadcast::Receiver<()>,
        shutdown_flag: Arc<AtomicBool>,
    ) {
        self.initialize().await;

        let cycle_duration = Duration::from_secs(self.params.cycle_seconds);
        info!(
            "Trading Engine active for {} (cycle={}s, level={}, max_shares={}, min_margin={:.4}%, max_margin={:.4}%, stale_limit={}s)",
            self.params.symbol,
            self.params.cycle_seconds,
            self.params.level,
            self.params.max_shares,
            self.params.min_margin * 100.0,
            self.params.max_margin * 100.0,
            self.params.stale_order_max_age_seconds
        );

        loop {
            if shutdown_flag.load(Ordering::Relaxed) || shutdown_rx.try_recv().is_ok() {
                self.persist_shutdown_state();
                break;
            }

            let cycle_start = Instant::now();
            if let Err(e) = self.step_cycle_at(Utc::now(), Some(&shutdown_flag)).await {
                warn!("Cycle error for {}: {}", self.params.symbol, e);
            }

            if shutdown_flag.load(Ordering::Relaxed) {
                self.persist_shutdown_state();
                break;
            }

            let elapsed = cycle_start.elapsed();
            if elapsed < cycle_duration {
                let remaining = cycle_duration - elapsed;
                tokio::select! {
                    _ = shutdown_rx.recv() => {
                        info!("Graceful shutdown signal received during cycle sleep.");
                        self.persist_shutdown_state();
                        break;
                    }
                    _ = sleep(remaining) => {}
                }
            }
        }
    }

    fn persist_shutdown_state(&self) {
        info!("Persisting Kalman state, regime state, and hunt history before shutdown...");
        if let Err(e) = self.store.save_kalman_state(self.filter.state()) {
            error!("Failed to save kalman_state.json on shutdown: {}", e);
        }
        if let Err(e) = self.store.save_regime_state(self.regime_ctrl.state()) {
            error!("Failed to save regime_state.json on shutdown: {}", e);
        }
        if let Err(e) = self
            .store
            .save_hunts(&self.trades_history, MAX_TERMINAL_HUNTS)
        {
            error!("Failed to save hunts_history.json on shutdown: {}", e);
        }
    }

    /// Executes a single trading & reconciliation cycle using the current wall-clock UTC time.
    pub async fn step_cycle(&mut self) -> Result<TelemetrySnapshot, String> {
        self.step_cycle_at(Utc::now(), None).await
    }

    /// Executes a single deterministic trading & reconciliation cycle at timestamp `now_dt`.
    pub async fn step_cycle_at(
        &mut self,
        now_dt: DateTime<Utc>,
        shutdown_flag: Option<&AtomicBool>,
    ) -> Result<TelemetrySnapshot, String> {
        let symbol = self.params.symbol.clone();

        // 1. Periodically refresh official Alpaca portfolio history (every 10 minutes)
        let should_refresh_ph = self
            .last_ph_fetch
            .is_none_or(|t| t.elapsed() >= Duration::from_secs(PORTFOLIO_HISTORY_REFRESH_SECS));
        if should_refresh_ph {
            match self.client.get_portfolio_history("1A", "1D").await {
                Ok(ph) => {
                    self.portfolio_history = Some(ph);
                    self.last_ph_fetch = Some(Instant::now());
                }
                Err(e) => {
                    warn!("Refreshing Alpaca portfolio history failed: {}", e);
                }
            }
        }

        // 2. Fetch live quote & update Adaptive 1D Kalman Filter + Regime Notional Controller
        let quote = self
            .client
            .get_latest_quote(&symbol)
            .await
            .map_err(|e| format!("Failed to fetch quote for {}: {}", symbol, e))?;

        let bid_d = quote.bid_price;
        let ask_d = quote.ask_price;
        let bid_f = decimal_to_f64(bid_d);
        let ask_f = decimal_to_f64(ask_d);

        let ku = self.filter.update(bid_f, ask_f);
        let fair_price_f = ku.fair_price;
        let fair_price_d = decimal_from_f64_clean(fair_price_f).unwrap_or(Decimal::ZERO);
        let mid_f = if ku.market_mid > 0.0 {
            ku.market_mid
        } else {
            0.5 * (bid_f + ask_f)
        };
        self.regime_ctrl.update(ku.market_mid);

        if let Err(e) = self.store.save_kalman_state(self.filter.state()) {
            warn!("Failed to persist kalman_state.json: {}", e);
        }

        // 3. Full Broker Reconciliation, Exit Monitoring, 12h Stale Liquidation, Adoption & Orphan Protection
        let stale_before = self
            .trades_history
            .iter()
            .filter(|h| h.status == HuntStatus::LiquidatedStale)
            .count();

        let (pending_updated, mut broker_telemetry) = reconcile_and_maintain_positions(
            &self.client,
            &self.params,
            &mut self.trades_history,
            &mut self.fee_tracker,
            fair_price_d,
            bid_d,
            now_dt,
            Some(&self.store),
            self.timing.reconciliation,
        )
        .await;

        let stale_after = self
            .trades_history
            .iter()
            .filter(|h| h.status == HuntStatus::LiquidatedStale)
            .count();
        if stale_after > stale_before {
            self.regime_ctrl.on_stale_liquidation();
        }
        if let Err(e) = self.store.save_regime_state(self.regime_ctrl.state()) {
            warn!("Failed to persist regime_state.json: {}", e);
        }

        if pending_updated {
            self.trades_history =
                trim_hunts_preserving_active(&self.trades_history, MAX_TERMINAL_HUNTS);
            if let Err(e) = self
                .store
                .save_hunts(&self.trades_history, MAX_TERMINAL_HUNTS)
            {
                error!("Failed to persist hunts_history.json after reconciliation: {}", e);
            }
        }

        let hunt_cap_usd = self.regime_ctrl.current_hunt_cap_usd();
        let total_exposure_cap_usd = self.regime_ctrl.current_total_exposure_cap_usd();
        let mid_dec = if bid_d > Decimal::ZERO && ask_d > Decimal::ZERO {
            (bid_d + ask_d) / Decimal::TWO
        } else {
            decimal_from_f64_clean(mid_f)
                .filter(|d| *d > Decimal::ZERO)
                .unwrap_or(Decimal::ZERO)
        };
        let tracked_exposure_usd: Decimal = self
            .trades_history
            .iter()
            .filter(|h| h.status.is_active())
            .fold(Decimal::ZERO, |acc, h| {
                let rem = (h.qty - h.filled_exit_qty).max(Decimal::ZERO);
                let mark = h.buy_price.max(mid_dec);
                let notional = rem.checked_mul(mark).unwrap_or(Decimal::MAX);
                acc.saturating_add(notional)
            });
        let broker_qty_dec = decimal_from_f64_clean(broker_telemetry.broker_position.qty)
            .filter(|d| *d > Decimal::ZERO)
            .unwrap_or(Decimal::ZERO);
        let broker_avg_entry_dec =
            decimal_from_f64_clean(broker_telemetry.broker_position.avg_entry_price)
                .filter(|d| *d > Decimal::ZERO)
                .unwrap_or(Decimal::ZERO);
        let broker_cur_px_dec =
            decimal_from_f64_clean(broker_telemetry.broker_position.current_price)
                .filter(|d| *d > Decimal::ZERO)
                .unwrap_or(Decimal::ZERO);
        let broker_mark_px = broker_avg_entry_dec
            .max(bid_d.max(Decimal::ZERO))
            .max(broker_cur_px_dec);
        let broker_exposure_usd = broker_qty_dec
            .checked_mul(broker_mark_px)
            .unwrap_or(Decimal::MAX);
        let active_exposure_usd = tracked_exposure_usd.max(broker_exposure_usd);
        let remaining_exposure_usd = total_exposure_cap_usd
            .saturating_sub(active_exposure_usd)
            .max(Decimal::ZERO);

        info!(
            "[{}] Bid=${:.2} Ask=${:.2} | Kalman=${:.2} | Margin={:+.4}% | Pos={:.4} ({}) | Cap=${}/${}",
            now_dt.format("%Y-%m-%d %H:%M:%S"),
            bid_f,
            ask_f,
            fair_price_f,
            ku.undervaluation_margin * 100.0,
            broker_telemetry.reconciliation.broker_qty,
            if broker_telemetry.reconciliation.is_synced {
                "SYNCED".to_string()
            } else {
                format!("DRIFT={:+.4}", broker_telemetry.reconciliation.untracked_qty)
            },
            hunt_cap_usd.round_dp(0),
            total_exposure_cap_usd.round_dp(0)
        );

        // 4. Process Kalman Signal
        match &ku.signal {
            SignalType::CircuitBreakerHalt {
                undervaluation_margin,
                max_margin,
                reason,
                ..
            } => {
                self.circuit_breaker_active = true;
                warn!(
                    "[CIRCUIT BREAKER] Deviation {:.4}% > {:.4}%. Halting entries ({})",
                    undervaluation_margin * 100.0,
                    max_margin * 100.0,
                    reason
                );
            }
            SignalType::BuyAnomaly {
                undervaluation_margin,
                fair_price,
                market_mid,
            } => {
                self.circuit_breaker_active = false;
                info!(
                    "[ANOMALY DETECTED] Margin {:.4}% >= {:.4}% | Fair=${:.2} Mid=${:.2}",
                    undervaluation_margin * 100.0,
                    self.params.min_margin * 100.0,
                    fair_price,
                    market_mid
                );

                match execute_hunt_with_notional_cap(
                    &self.client,
                    &self.params,
                    *fair_price,
                    &mut self.trades_history,
                    &mut self.fee_tracker,
                    Some(&self.store),
                    now_dt,
                    self.timing.hunt,
                    shutdown_flag,
                    hunt_cap_usd,
                    remaining_exposure_usd,
                )
                .await
                {
                    Ok(Some(_created)) => {
                        self.trades_history =
                            trim_hunts_preserving_active(&self.trades_history, MAX_TERMINAL_HUNTS);
                        if let Err(e) = self
                            .store
                            .save_hunts(&self.trades_history, MAX_TERMINAL_HUNTS)
                        {
                            error!("Failed to persist hunts_history.json after hunt: {}", e);
                        }

                        // Refresh broker telemetry so snapshot reflects newly placed exit immediately
                        let acc_post = self.client.get_account().await.ok();
                        let pos_post = self.client.get_position(&symbol).await.ok().flatten();
                        let open_post = self
                            .client
                            .get_open_orders(Some(&symbol))
                            .await
                            .unwrap_or_default();
                        if acc_post.is_some() || pos_post.is_some() {
                            broker_telemetry = build_broker_telemetry(
                                acc_post.as_ref(),
                                pos_post.as_ref(),
                                &open_post,
                                &self.trades_history,
                            );
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        error!("Error during Market Hunt execution: {}", e);
                    }
                }
            }
            SignalType::None => {
                self.circuit_breaker_active = false;
            }
        }

        // 5. Compute & atomically persist metrics.json
        let live_eq_now = broker_telemetry.broker_account.equity;
        let metrics = self.compute_current_metrics(live_eq_now, now_dt);
        if let Err(e) = self.store.save_metrics(&metrics) {
            warn!("Failed to persist metrics.json: {}", e);
        }

        // 6. Build & atomically persist telemetry.json
        let active_hunts_count = self
            .trades_history
            .iter()
            .filter(|h| h.status.is_active())
            .count();
        let completed_trades_count = self
            .trades_history
            .iter()
            .filter(|h| h.status.is_closed_strategy_trade())
            .count();

        let snapshot = TelemetrySnapshot {
            timestamp: now_dt.to_rfc3339(),
            symbol,
            best_bid: bid_f,
            best_ask: ask_f,
            market_mid: mid_f,
            fair_price: fair_price_f,
            undervaluation_margin: ku.undervaluation_margin,
            min_margin: self.params.min_margin,
            max_margin: self.params.max_margin,
            circuit_breaker_active: self.circuit_breaker_active,
            active_hunts_count,
            completed_trades_count,
            total_realized_pnl: metrics.total_pnl,
            broker: broker_telemetry,
            trades: self.trades_history.clone(),
            metrics,
        };

        if let Err(e) = self.store.save_telemetry(&snapshot) {
            warn!("Failed to persist telemetry.json: {}", e);
        }

        Ok(snapshot)
    }
}
