use crate::config::KalmanParams;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KalmanState {
    pub x_hat: f64,          // Latent estimated fair price
    pub p: f64,              // Estimation error variance
    pub q: f64,              // Current process noise variance
    pub innovation_var: f64, // EWMA running variance of innovations
    pub sample_count: u64,   // Number of processed ticks
}

impl KalmanState {
    pub fn is_valid(&self) -> bool {
        self.x_hat.is_finite()
            && self.p.is_finite()
            && self.q.is_finite()
            && self.innovation_var.is_finite()
            && self.p >= 0.0
            && self.q > 0.0
            && self.innovation_var >= 0.0
            && (self.sample_count == 0 || self.x_hat > 0.0)
    }
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
        fair_price: f64,
        reason: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct KalmanUpdate {
    pub signal: SignalType,
    pub undervaluation_margin: f64,
    pub fair_price: f64,
    pub market_mid: f64,
}

pub struct AdaptiveKalmanFilter {
    params: KalmanParams,
    min_margin: f64,
    max_margin: f64,
    state: KalmanState,
}

impl AdaptiveKalmanFilter {
    pub fn new(
        params: KalmanParams,
        min_margin: f64,
        max_margin: f64,
        initial_state: Option<KalmanState>,
    ) -> Self {
        let state = initial_state
            .filter(|s| s.is_valid())
            .unwrap_or(KalmanState {
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
        self.state.sample_count > 0 && self.state.x_hat > 0.0 && self.state.is_valid()
    }

    /// Process a new market quote `(best_bid, best_ask)` matching `AdaptiveKalmanLive` in `live_bot.py`,
    /// hardened against NaN, Infinity, non-positive bids, and locked/crossed quotes (`ask <= bid`).
    pub fn update(&mut self, bid: f64, ask: f64) -> KalmanUpdate {
        if !bid.is_finite() || !ask.is_finite() || bid <= 0.0 || ask <= bid {
            let safe_fair = if self.state.x_hat.is_finite() && self.state.x_hat > 0.0 {
                self.state.x_hat
            } else {
                0.0
            };
            return KalmanUpdate {
                signal: SignalType::None,
                undervaluation_margin: 0.0,
                fair_price: safe_fair,
                market_mid: 0.0,
            };
        }

        let mid = 0.5 * (bid + ask);
        let spread = ask - bid;
        if !mid.is_finite() || mid <= 0.0 || !spread.is_finite() || spread <= 0.0 {
            return KalmanUpdate {
                signal: SignalType::None,
                undervaluation_margin: 0.0,
                fair_price: self.state.x_hat.max(0.0),
                market_mid: 0.0,
            };
        }

        // Initialization on very first valid quote (or recovery from corrupted state)
        if self.state.sample_count == 0 || self.state.x_hat <= 0.0 || !self.state.is_valid() {
            self.state.x_hat = mid;
            self.state.p = self.params.r_base * mid * mid;
            self.state.q = self.params.q_base * mid * mid;
            self.state.innovation_var = self.state.p;
            self.state.sample_count = 1;
            return KalmanUpdate {
                signal: SignalType::None,
                undervaluation_margin: 0.0,
                fair_price: self.state.x_hat,
                market_mid: mid,
            };
        }

        // 1. Prior Prediction
        let x_prior = self.state.x_hat;
        let p_prior = (self.state.p + self.state.q).max(0.0);

        // 2. Spread-Adaptive Observation Noise R_t
        let half_spread = 0.5 * spread;
        let r_t = ((self.params.r_base * mid * mid)
            + self.params.spread_weight * (half_spread * half_spread))
            .max(1e-18);

        // 3. Innovation and Relative Undervaluation Margin
        let nu = mid - x_prior;
        let undervaluation_margin = (x_prior - mid) / x_prior;

        // 4. Circuit Breaker Check (Gated R_cb * 20.0 update anchored to max(mid, x_prior)
        //    and clamped to k_gated <= 0.25 prevents IEEE-754 P_t -> 0 collapse and flash-crash poisoning)
        if undervaluation_margin > self.max_margin {
            let ref_px = mid.max(x_prior);
            let r_cb = ((self.params.r_base * ref_px * ref_px)
                + self.params.spread_weight * (half_spread * half_spread))
                .max(1e-18);
            let gated_r = r_cb * 20.0;
            let gated_denom = p_prior + gated_r;
            let k_gated = if gated_denom.is_finite() && gated_denom > 0.0 {
                (p_prior / gated_denom).clamp(0.0, 0.25)
            } else {
                0.0
            };
            self.state.x_hat = x_prior + k_gated * nu;
            self.state.p = ((1.0 - k_gated) * p_prior).max(0.0);
            self.state.sample_count += 1;

            return KalmanUpdate {
                signal: SignalType::CircuitBreakerHalt {
                    undervaluation_margin,
                    max_margin: self.max_margin,
                    fair_price: x_prior,
                    reason: format!(
                        "Deviation {:.4}% exceeds circuit breaker threshold max_margin {:.4}%",
                        undervaluation_margin * 100.0,
                        self.max_margin * 100.0
                    ),
                },
                undervaluation_margin,
                fair_price: x_prior,
                market_mid: mid,
            };
        }

        // 5. Kalman Gain & Posterior Update
        let s_t = p_prior + r_t;
        let k_t = if s_t > 0.0 { p_prior / s_t } else { 0.5 }.clamp(0.01, 0.99);
        self.state.x_hat = x_prior + k_t * nu;
        self.state.p = ((1.0 - k_t) * p_prior).max(0.0);

        // 6. Myers-Tapley Innovation-Adaptive Process Noise Q_t Update
        let lambda = self.params.adapt_lambda.clamp(0.01, 0.5);
        self.state.innovation_var =
            ((1.0 - lambda) * self.state.innovation_var + lambda * (nu * nu)).max(0.0);
        let excess_var = self.state.innovation_var - r_t;
        let min_q = self.params.q_base * mid * mid;
        self.state.q = (lambda * excess_var).max(min_q);
        self.state.sample_count += 1;

        // 7. Signal Generation
        let signal = if undervaluation_margin >= self.min_margin {
            SignalType::BuyAnomaly {
                undervaluation_margin,
                fair_price: x_prior,
                market_mid: mid,
            }
        } else {
            SignalType::None
        };

        KalmanUpdate {
            signal,
            undervaluation_margin,
            fair_price: x_prior,
            market_mid: mid,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn default_params() -> KalmanParams {
        KalmanParams {
            q_base: 1.5e-7,
            r_base: 4.0e-7,
            spread_weight: 2.0,
            adapt_lambda: 0.20,
        }
    }

    #[test]
    fn test_kalman_rejects_bad_quotes_and_stays_finite() {
        let mut kf = AdaptiveKalmanFilter::new(default_params(), 0.0036, 0.025, None);
        let u0 = kf.update(75.00, 75.02);
        assert_eq!(u0.signal, SignalType::None);
        assert!((u0.fair_price - 75.01).abs() < 1e-9);

        // Bad quotes must not corrupt state
        for (b, a) in [
            (f64::NAN, 75.0),
            (75.0, f64::INFINITY),
            (-1.0, 75.0),
            (0.0, 75.0),
            (75.05, 75.05),
            (75.10, 75.00),
        ] {
            let u = kf.update(b, a);
            assert_eq!(u.signal, SignalType::None);
            assert!(kf.state().is_valid());
            assert_eq!(kf.state().sample_count, 1);
        }
    }

    #[test]
    fn test_kalman_buy_anomaly_and_circuit_breaker() {
        let mut kf = AdaptiveKalmanFilter::new(default_params(), 0.0036, 0.025, None);
        for _ in 0..5 {
            kf.update(100.00, 100.02);
        }

        // 0.60% dip -> BuyAnomaly
        let u_buy = kf.update(99.40, 99.42);
        assert!(matches!(u_buy.signal, SignalType::BuyAnomaly { .. }));
        assert!(u_buy.undervaluation_margin >= 0.0036);

        // 5% flash crash -> CircuitBreakerHalt with gated state update
        let x_before_crash = kf.state().x_hat;
        let u_cb = kf.update(94.50, 94.52);
        assert!(matches!(u_cb.signal, SignalType::CircuitBreakerHalt { .. }));
        // Gated update (R_t * 20.0) absorbs <20% of the ~$5.20 shock (~0.86 vs ~4.15 ungated)
        let shock = (x_before_crash - 94.51).abs();
        let shift = (x_before_crash - kf.state().x_hat).abs();
        assert!(
            shift < 0.25 * shock,
            "gated shift {:.4} should stay well below 25% of crash shock {:.4}",
            shift,
            shock
        );
    }

    #[test]
    fn test_kalman_extreme_bad_tick_preserves_positive_variance() {
        // CE-3.1 regression: BTC=$90,000 hit by near-zero bad tick $1e-5 must NOT collapse P_t to 0
        // or absorb >25% of the bogus tick into x_hat.
        let mut kf = AdaptiveKalmanFilter::new(default_params(), 0.0036, 0.025, None);
        for _ in 0..20 {
            kf.update(89_999.0, 90_001.0);
        }
        let x_before = kf.state().x_hat;
        let p_prior = kf.state().p + kf.state().q;
        let u = kf.update(1e-5, 1.01e-5);

        assert!(matches!(u.signal, SignalType::CircuitBreakerHalt { .. }));
        assert!(kf.state().is_valid());
        assert!(
            kf.state().p >= 0.75 * p_prior && kf.state().p > 0.0,
            "P_t must remain strictly >= 0.75 * p_prior (> 0), got {}",
            kf.state().p
        );
        let shift = (x_before - kf.state().x_hat).abs();
        let shock = (x_before - 1.005e-5).abs();
        assert!(
            shift <= 0.25 * shock + 1e-9,
            "single bad tick shift {} exceeded 25% of shock {}",
            shift,
            shock
        );
        assert!(kf.state().x_hat > 0.75 * x_before);
    }
}
