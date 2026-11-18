use crate::config::KalmanParams;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KalmanState {
    pub x_hat: f64,             // Latent estimated fair price
    pub p: f64,                 // Estimation error variance
    pub q: f64,                 // Current process noise variance
    pub innovation_var: f64,    // EWMA running variance of innovations
    pub sample_count: u64,      // Number of processed ticks
}

#[derive(Debug, Clone, PartialEq)]
pub enum SignalType {
    None,
    BuyAnomaly {
        undervaluation_margin: f64,
        fair_price: f64,
        market_mid: f64,
    },
    CircuitBreakerHalt {
        undervaluation_margin: f64,
        max_margin: f64,
        reason: String,
    },
}

pub struct AdaptiveKalmanFilter {
    params: KalmanParams,
    min_margin: f64,
    max_margin: f64,
    state: KalmanState,
}

impl AdaptiveKalmanFilter {
    pub fn new(params: KalmanParams, min_margin: f64, max_margin: f64, initial_state: Option<KalmanState>) -> Self {
        let state = initial_state.unwrap_or(KalmanState {
            x_hat: 0.0,
            p: 1.0,
            q: params.q_base,
            innovation_var: params.r_base,
            sample_count: 0,
        });

        Self {
            params,
            min_margin,
            max_margin,
            state,
        }
    }

    pub fn state(&self) -> &KalmanState {
        &self.state
    }

    pub fn is_initialized(&self) -> bool {
        self.state.sample_count > 0 && self.state.x_hat > 0.0
    }

    /// Process a new market quote (best_bid, best_ask) and return the trading signal.
    pub fn update(&mut self, bid: f64, ask: f64) -> SignalType {
        if bid <= 0.0 || ask <= bid || !bid.is_finite() || !ask.is_finite() {
            return SignalType::None;
        }

        let mid = 0.5 * (bid + ask);
        let spread = ask - bid;

        // Initialization on very first valid quote
        if self.state.sample_count == 0 || self.state.x_hat <= 0.0 {
            self.state.x_hat = mid;
            self.state.p = self.params.r_base * mid * mid;
            self.state.q = self.params.q_base * mid * mid;
            self.state.innovation_var = self.state.p;
            self.state.sample_count = 1;
            return SignalType::None;
        }

        // 1. Prior Prediction
        let x_prior = self.state.x_hat;
        let p_prior = self.state.p + self.state.q;

        // 2. Spread-Adaptive Observation Noise
        let half_spread = 0.5 * spread;
        let r_t = (self.params.r_base * mid * mid)
            + self.params.spread_weight * (half_spread * half_spread);

        // 3. Innovation and Anomaly Detection
        let nu = mid - x_prior;
        // Relative undervaluation: positive when market mid is cheaper than predicted fair price
        let undervaluation_margin = (x_prior - mid) / x_prior;

        // Circuit breaker check
        if undervaluation_margin > self.max_margin {
            // Flash crash or abnormal dump detected: halt trading immediately
            // Heavily damp state update to protect filter from poisoning
            let gated_r = r_t * 20.0;
            let k_gated = p_prior / (p_prior + gated_r);
            self.state.x_hat = x_prior + k_gated * nu;
            self.state.p = (1.0 - k_gated) * p_prior;
            self.state.sample_count += 1;

            return SignalType::CircuitBreakerHalt {
                undervaluation_margin,
                max_margin: self.max_margin,
                reason: format!(
                    "Deviation {:.4}% exceeds circuit breaker threshold max_margin {:.4}%",
                    undervaluation_margin * 100.0,
                    self.max_margin * 100.0
                ),
            };
        }

        // 4. Kalman Gain & Posterior Update
        let s_t = p_prior + r_t;
        let k_t = if s_t > 0.0 { p_prior / s_t } else { 0.5 };
        let k_t = k_t.clamp(0.01, 0.99);

        self.state.x_hat = x_prior + k_t * nu;
        self.state.p = (1.0 - k_t) * p_prior;

        // 5. Myers-Tapley Innovation-Adaptive Process Noise Update
        let lambda = self.params.adapt_lambda.clamp(0.01, 0.5);
        self.state.innovation_var = (1.0 - lambda) * self.state.innovation_var + lambda * (nu * nu);
        let excess_var = self.state.innovation_var - r_t;
        let min_q = self.params.q_base * mid * mid;
        self.state.q = (lambda * excess_var).max(min_q);

        self.state.sample_count += 1;

        // 6. Signal Generation
        if undervaluation_margin >= self.min_margin {
            SignalType::BuyAnomaly {
                undervaluation_margin,
                fair_price: x_prior,
                market_mid: mid,
            }
        } else {
            SignalType::None
        }
    }
}
