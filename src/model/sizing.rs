use crate::config::RegimeSizingParams;
use crate::model::pricing::{decimal_from_f64_clean, floor_qty, MIN_ORDER_NOTIONAL_USD};
use rust_decimal::Decimal;
use serde::{Deserialize, Serialize};

fn compute_asymptotic_sizing_factor(
    undervaluation_margin: f64,
    min_margin: f64,
    max_margin: f64,
    level: f64,
) -> Option<Decimal> {
    if !undervaluation_margin.is_finite()
        || !min_margin.is_finite()
        || !max_margin.is_finite()
        || !level.is_finite()
        || undervaluation_margin < min_margin
        || undervaluation_margin > max_margin
        || level <= 0.0
    {
        return None;
    }

    let denom = max_margin - min_margin;
    if denom <= 0.0 {
        return None;
    }

    let x = ((undervaluation_margin - min_margin) / denom).clamp(0.0, 1.0);
    let x_pow = x.powf(level);
    let inner = (1.0 - x_pow).clamp(0.0, 1.0);
    let factor_f64 = (1.0 - inner.powf(1.0 / level)).clamp(0.0, 1.0);

    decimal_from_f64_clean(factor_f64)
        .filter(|f| *f > Decimal::ZERO)
        .map(|f| f.clamp(Decimal::ZERO, Decimal::ONE))
}

fn finalize_floored_qty(
    raw_qty: Decimal,
    capital_cap: Decimal,
    buy_price: Decimal,
    qty_decimals: u32,
) -> Decimal {
    let step = Decimal::new(1, qty_decimals);
    let mut qty = floor_qty(raw_qty, qty_decimals);

    // Strict post-condition: qty * buy_price MUST NEVER exceed capital_cap
    while qty > Decimal::ZERO
        && qty
            .checked_mul(buy_price)
            .is_none_or(|notional| notional > capital_cap)
    {
        let next = qty - step;
        if next >= qty {
            return Decimal::ZERO;
        }
        qty = next.max(Decimal::ZERO);
    }

    // Enforce $10.00 minimum notional after flooring
    if qty <= Decimal::ZERO
        || qty
            .checked_mul(buy_price)
            .is_none_or(|notional| notional < MIN_ORDER_NOTIONAL_USD)
    {
        return Decimal::ZERO;
    }

    qty
}

/// Dynamic portfolio position sizing using the asymptotic curve:
///   `factor(x) = 1 - (1 - x^level)^(1/level)` where `x = (margin - min_margin) / (max_margin - min_margin)`.
///
/// Strict financial invariants:
/// - Returns `0` for non-finite inputs, `margin < min_margin`, or `margin > max_margin` (circuit breaker).
/// - Strictly floors order quantity (`4` decimals for crypto, whole shares for equity limit orders)
///   and verifies `qty * buy_price <= available_cash` STRICTLY ALWAYS.
/// - Enforces Alpaca's `$10.00` minimum order notional.
#[allow(clippy::too_many_arguments)]
pub fn calculate_dynamic_portfolio_shares(
    undervaluation_margin: f64,
    min_margin: f64,
    max_margin: f64,
    level: f64,
    max_shares_param: Decimal,
    total_equity: Decimal,
    available_cash: Decimal,
    buy_price: Decimal,
    is_crypto: bool,
) -> Decimal {
    if max_shares_param <= Decimal::ZERO
        || buy_price <= Decimal::ZERO
        || available_cash <= Decimal::ZERO
    {
        return Decimal::ZERO;
    }

    let Some(factor) =
        compute_asymptotic_sizing_factor(undervaluation_margin, min_margin, max_margin, level)
    else {
        return Decimal::ZERO;
    };

    let qty_decimals: u32 = if is_crypto { 4 } else { 0 };
    let safe_cash = available_cash.max(Decimal::ZERO);

    let (raw_qty, capital_cap) = if max_shares_param <= Decimal::ONE {
        // Proportion of total portfolio equity mode (e.g. 1.0 = up to 100% equity, capped by available cash)
        let equity_cap = total_equity.max(Decimal::ZERO) * max_shares_param;
        let target_capital = factor * equity_cap;
        let allocated_capital = target_capital.min(safe_cash).min(equity_cap);
        if allocated_capital < MIN_ORDER_NOTIONAL_USD {
            return Decimal::ZERO;
        }
        (allocated_capital / buy_price, allocated_capital)
    } else {
        // Legacy absolute unit shares mode, still strictly capped by available cash
        let raw_shares = factor * max_shares_param;
        let max_affordable = floor_qty(safe_cash / buy_price, qty_decimals);
        (
            raw_shares.min(max_shares_param).min(max_affordable),
            safe_cash,
        )
    };

    finalize_floored_qty(raw_qty, capital_cap, buy_price, qty_decimals)
}

/// Fixed-USD notional-capped position sizing with concurrent exposure & cash budget enforcement.
///
/// Guarantees:
/// - `qty * buy_price <= min(max_notional_usd, remaining_exposure_usd, available_cash)`
/// - `qty * buy_price >= $10.00` (or `0` if unaffordable / out-of-bounds)
/// - `qty` strictly floored (`4` decimals crypto, `0` decimals equity)
#[allow(clippy::too_many_arguments)]
pub fn calculate_notional_capped_shares(
    undervaluation_margin: f64,
    min_margin: f64,
    max_margin: f64,
    level: f64,
    max_notional_usd: Decimal,
    remaining_exposure_usd: Decimal,
    available_cash: Decimal,
    buy_price: Decimal,
    is_crypto: bool,
) -> Decimal {
    if max_notional_usd <= Decimal::ZERO
        || remaining_exposure_usd <= Decimal::ZERO
        || available_cash <= Decimal::ZERO
        || buy_price <= Decimal::ZERO
    {
        return Decimal::ZERO;
    }

    let Some(factor) =
        compute_asymptotic_sizing_factor(undervaluation_margin, min_margin, max_margin, level)
    else {
        return Decimal::ZERO;
    };

    let target_capital = factor * max_notional_usd;
    let effective_cap = max_notional_usd
        .min(remaining_exposure_usd.max(Decimal::ZERO))
        .min(available_cash.max(Decimal::ZERO));
    let allocated_capital = target_capital.min(effective_cap);
    if allocated_capital < MIN_ORDER_NOTIONAL_USD {
        return Decimal::ZERO;
    }

    let Some(raw_qty) = allocated_capital.checked_div(buy_price) else {
        return Decimal::ZERO;
    };
    let qty_decimals: u32 = if is_crypto { 4 } else { 0 };
    finalize_floored_qty(raw_qty, allocated_capital, buy_price, qty_decimals)
}

/// Persistent state for the asymmetric `$3,000 <-> $30,000` regime notional controller.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegimeNotionalState {
    pub ema_fast: f64,
    pub ema_slow: f64,
    pub last_mid: f64,
    pub return_var: f64,
    pub smoothed_cap_usd: f64,
    pub sample_count: u64,
    pub stale_cooldown_remaining: u64,
}

impl RegimeNotionalState {
    pub fn is_valid(&self) -> bool {
        self.ema_fast.is_finite()
            && self.ema_slow.is_finite()
            && self.last_mid.is_finite()
            && self.return_var.is_finite()
            && self.smoothed_cap_usd.is_finite()
            && self.ema_fast >= 0.0
            && self.ema_slow >= 0.0
            && self.last_mid >= 0.0
            && self.return_var >= 0.0
            && self.smoothed_cap_usd >= 0.0
            && (self.sample_count == 0
                || (self.ema_fast > 0.0 && self.ema_slow > 0.0 && self.last_mid > 0.0))
    }
}

/// Volatility-normalized dual-EMA regime sizing controller with asymmetric smoothing
/// (slow confirmed ramp-up, instant downward clamp on bearish drift or `LiquidatedStale`).
#[derive(Debug, Clone)]
pub struct RegimeNotionalController {
    params: RegimeSizingParams,
    state: RegimeNotionalState,
}

impl RegimeNotionalController {
    pub fn new(params: RegimeSizingParams, initial_state: Option<RegimeNotionalState>) -> Self {
        let (min_n, max_n, _, _) = Self::sanitize_bounds(&params);
        let state = match initial_state.filter(|s| s.is_valid()) {
            Some(mut st) => {
                st.return_var = st.return_var.max(1e-12);
                st.smoothed_cap_usd = if st.sample_count == 0 {
                    min_n
                } else {
                    st.smoothed_cap_usd.clamp(min_n, max_n)
                };
                st
            }
            None => RegimeNotionalState {
                ema_fast: 0.0,
                ema_slow: 0.0,
                last_mid: 0.0,
                return_var: 1e-6,
                smoothed_cap_usd: min_n,
                sample_count: 0,
                stale_cooldown_remaining: 0,
            },
        };
        Self { params, state }
    }

    pub fn state(&self) -> &RegimeNotionalState {
        &self.state
    }

    pub fn params(&self) -> &RegimeSizingParams {
        &self.params
    }

    fn sanitize_bounds(params: &RegimeSizingParams) -> (f64, f64, f64, f64) {
        let min_n = if params.min_notional_usd.is_finite() {
            params.min_notional_usd.max(0.0)
        } else {
            3000.0
        };
        let max_n = if params.max_notional_usd.is_finite() {
            params.max_notional_usd.max(min_n)
        } else {
            min_n
        };
        let min_e = if params.min_total_exposure_usd.is_finite() {
            params.min_total_exposure_usd.max(min_n)
        } else {
            min_n
        };
        let max_e = if params.max_total_exposure_usd.is_finite() {
            params.max_total_exposure_usd.max(min_e).max(max_n)
        } else {
            min_e.max(max_n)
        };
        (min_n, max_n, min_e, max_e)
    }

    /// Updates dual EMAs, EWMA return variance, and asymmetric notional cap on valid positive mid prices.
    pub fn update(&mut self, mid: f64) {
        if !mid.is_finite() || mid <= 0.0 {
            return;
        }
        let (min_n, max_n, _, _) = Self::sanitize_bounds(&self.params);

        if self.state.sample_count == 0 || !self.state.is_valid() {
            self.state.ema_fast = mid;
            self.state.ema_slow = mid;
            self.state.last_mid = mid;
            self.state.return_var = 1e-6;
            self.state.smoothed_cap_usd = min_n;
            self.state.sample_count = 1;
            return;
        }

        let fast_cycles = self.params.ema_fast_cycles.max(1);
        let slow_cycles = self.params.ema_slow_cycles.max(1);
        let vol_cycles = self.params.vol_cycles.max(1);

        let alpha_fast = 2.0 / (fast_cycles as f64 + 1.0);
        let alpha_slow = 2.0 / (slow_cycles as f64 + 1.0);
        let alpha_v = 2.0 / (vol_cycles as f64 + 1.0);

        self.state.ema_fast = ((1.0 - alpha_fast) * self.state.ema_fast + alpha_fast * mid)
            .clamp(self.state.ema_fast.min(mid), self.state.ema_fast.max(mid));
        self.state.ema_slow = ((1.0 - alpha_slow) * self.state.ema_slow + alpha_slow * mid)
            .clamp(self.state.ema_slow.min(mid), self.state.ema_slow.max(mid));

        let ratio = mid / self.state.last_mid;
        let log_ret = if ratio.is_finite() && ratio > 0.0 {
            ratio.ln()
        } else {
            mid.ln() - self.state.last_mid.ln()
        };
        let ret_sq = if log_ret.is_finite() {
            let sq = log_ret * log_ret;
            if sq.is_finite() {
                sq
            } else {
                1.0
            }
        } else {
            0.0
        };
        self.state.return_var =
            ((1.0 - alpha_v) * self.state.return_var + alpha_v * ret_sq).max(1e-12);

        let trend_rel = if self.state.ema_slow > 0.0 && self.state.ema_slow.is_finite() {
            (self.state.ema_fast - self.state.ema_slow) / self.state.ema_slow
        } else {
            0.0
        };
        let vol_horizon = (self.state.return_var * fast_cycles as f64).sqrt().max(1e-6);
        let raw_z = trend_rel / vol_horizon;
        let z_reg = if raw_z.is_finite() {
            raw_z.clamp(-20.0, 20.0)
        } else if trend_rel >= 0.0 {
            20.0
        } else {
            -20.0
        };

        let target_usd = if self.state.stale_cooldown_remaining > 0 {
            self.state.stale_cooldown_remaining -= 1;
            min_n
        } else if self.state.sample_count < fast_cycles {
            min_n
        } else {
            let k = if self.params.sigmoid_k.is_finite() && self.params.sigmoid_k > 0.0 {
                self.params.sigmoid_k
            } else {
                2.0
            };
            let center = if self.params.sigmoid_center.is_finite() {
                self.params.sigmoid_center
            } else {
                0.25
            };
            let sig = (1.0 / (1.0 + (-k * (z_reg - center)).exp())).clamp(0.0, 1.0);
            min_n + (max_n - min_n) * sig
        };

        let ramp_alpha = if self.params.ramp_up_alpha.is_finite() {
            self.params.ramp_up_alpha.clamp(0.0, 1.0)
        } else {
            0.01
        };

        if target_usd <= self.state.smoothed_cap_usd {
            self.state.smoothed_cap_usd = target_usd;
        } else {
            self.state.smoothed_cap_usd += ramp_alpha * (target_usd - self.state.smoothed_cap_usd);
        }
        self.state.smoothed_cap_usd = self.state.smoothed_cap_usd.clamp(min_n, max_n);
        self.state.last_mid = mid;
        self.state.sample_count = self.state.sample_count.saturating_add(1);
    }

    /// Immediately clamps per-hunt notional cap to `min_notional_usd` and locks cooldown.
    pub fn on_stale_liquidation(&mut self) {
        let (min_n, _, _, _) = Self::sanitize_bounds(&self.params);
        self.state.smoothed_cap_usd = min_n;
        self.state.stale_cooldown_remaining = self.params.stale_cooldown_cycles;
    }

    /// Exact per-hunt USD notional cap clamped strictly in `[min_notional_usd, max_notional_usd]`.
    pub fn current_hunt_cap_usd(&self) -> Decimal {
        let (min_n, max_n, _, _) = Self::sanitize_bounds(&self.params);
        let min_dec = decimal_from_f64_clean(min_n).unwrap_or(Decimal::ZERO);
        let max_dec = decimal_from_f64_clean(max_n)
            .unwrap_or(min_dec)
            .max(min_dec);
        let cap = if self.state.smoothed_cap_usd.is_finite() {
            self.state.smoothed_cap_usd.clamp(min_n, max_n)
        } else {
            min_n
        };
        decimal_from_f64_clean(cap)
            .unwrap_or(min_dec)
            .clamp(min_dec, max_dec)
    }

    /// Exact concurrent open exposure cap linearly interpolated across `[min_total_exposure_usd, max_total_exposure_usd]`,
    /// strictly clamped `>= current_hunt_cap_usd()`.
    pub fn current_total_exposure_cap_usd(&self) -> Decimal {
        let (min_n, max_n, min_e, max_e) = Self::sanitize_bounds(&self.params);
        let hunt_cap = self.current_hunt_cap_usd();
        let denom = max_n - min_n;
        let r = if denom > 0.0 && self.state.smoothed_cap_usd.is_finite() {
            ((self.state.smoothed_cap_usd - min_n) / denom).clamp(0.0, 1.0)
        } else {
            0.0
        };
        let target_exp = (min_e + r * (max_e - min_e)).clamp(min_e, max_e);
        let min_exp_dec = decimal_from_f64_clean(min_e).unwrap_or(hunt_cap);
        let max_exp_dec = decimal_from_f64_clean(max_e)
            .unwrap_or(min_exp_dec)
            .max(min_exp_dec);
        decimal_from_f64_clean(target_exp)
            .unwrap_or(min_exp_dec)
            .clamp(min_exp_dec, max_exp_dec)
            .max(hunt_cap)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    fn d(s: &str) -> Decimal {
        Decimal::from_str(s).unwrap()
    }

    #[test]
    fn test_dynamic_sizing_bounds_and_cash_cap() {
        let min_m = 0.0036;
        let max_m = 0.0250;
        let level = 0.05;

        // Outside [min_margin, max_margin] -> 0
        assert_eq!(
            calculate_dynamic_portfolio_shares(
                0.0035, min_m, max_m, level, d("1.0"), d("100000"), d("50000"), d("73.00"), true
            ),
            Decimal::ZERO
        );
        assert_eq!(
            calculate_dynamic_portfolio_shares(
                0.0251, min_m, max_m, level, d("1.0"), d("100000"), d("50000"), d("73.00"), true
            ),
            Decimal::ZERO
        );
        assert_eq!(
            calculate_dynamic_portfolio_shares(
                f64::NAN, min_m, max_m, level, d("1.0"), d("100000"), d("50000"), d("73.00"), true
            ),
            Decimal::ZERO
        );

        // Cash cap strictly respected (never exceeds available_cash even with ugly price)
        let cash = d("1000.00");
        let px = d("73.33");
        let qty = calculate_dynamic_portfolio_shares(
            0.0249, min_m, max_m, level, d("1.0"), d("100000"), cash, px, true,
        );
        assert!(qty > Decimal::ZERO);
        assert!(qty * px <= cash, "notional {} exceeded cash {}", qty * px, cash);

        // Below $10 notional -> 0
        assert_eq!(
            calculate_dynamic_portfolio_shares(
                0.0249, min_m, max_m, level, d("1.0"), d("100000"), d("9.99"), d("73.00"), true
            ),
            Decimal::ZERO
        );

        // Equity mode uses whole integer shares and stays <= available cash
        let eq_qty = calculate_dynamic_portfolio_shares(
            0.0249, min_m, max_m, level, d("1.0"), d("100000"), d("250.00"), d("73.33"), false,
        );
        assert_eq!(eq_qty, d("3"));
        assert!(eq_qty * d("73.33") <= d("250.00"));
    }

    #[test]
    fn test_dynamic_sizing_29_digit_equity_cap_invariant() {
        // CE-4.1 regression: 29-digit equity / buy_price must still strictly obey qty * buy_price <= min(cash, equity * max_shares)
        let equity = d("29.999999999999999999999999999");
        let cash = d("35.00");
        let buy_px = d("3.00");
        let qty = calculate_dynamic_portfolio_shares(
            0.0250,
            0.0036,
            0.0250,
            0.05,
            Decimal::ONE,
            equity,
            cash,
            buy_px,
            true,
        );
        assert_eq!(qty, d("9.9999"));
        assert!(qty * buy_px <= cash);
        assert!(
            qty * buy_px <= equity * Decimal::ONE,
            "notional {} exceeded equity cap {}",
            qty * buy_px,
            equity
        );
    }

    #[test]
    fn test_regime_controller_warmup_bull_bear_and_stale_cooldown() {
        let params = RegimeSizingParams {
            ema_fast_cycles: 10,
            ema_slow_cycles: 40,
            vol_cycles: 10,
            ramp_up_alpha: 0.25,
            stale_cooldown_cycles: 5,
            ..Default::default()
        };
        let mut ctrl = RegimeNotionalController::new(params, None);

        // Non-finite / non-positive ticks are pure no-ops
        ctrl.update(f64::NAN);
        ctrl.update(-5.0);
        ctrl.update(0.0);
        assert_eq!(ctrl.state().sample_count, 0);
        assert_eq!(ctrl.current_hunt_cap_usd(), d("3000"));
        assert_eq!(ctrl.current_total_exposure_cap_usd(), d("15000"));

        // Warmup ticks (< ema_fast_cycles) keep cap locked at $3,000 floor even in rally
        for i in 0..10 {
            ctrl.update(100.0 + i as f64);
            assert_eq!(ctrl.current_hunt_cap_usd(), d("3000"));
        }

        // Sustained bull trend ramps cap smoothly upward toward $30,000
        for i in 10..60 {
            ctrl.update(100.0 + i as f64 * 1.5);
        }
        let bull_cap = ctrl.current_hunt_cap_usd();
        let bull_exp = ctrl.current_total_exposure_cap_usd();
        assert!(bull_cap > d("15000") && bull_cap <= d("30000"));
        assert!(bull_exp >= bull_cap && bull_exp <= d("90000"));

        // Stale liquidation immediately clamps to $3,000 and holds for 5 ticks
        ctrl.on_stale_liquidation();
        assert_eq!(ctrl.current_hunt_cap_usd(), d("3000"));
        assert_eq!(ctrl.current_total_exposure_cap_usd(), d("15000"));
        for i in 60..65 {
            ctrl.update(100.0 + i as f64 * 1.5);
            assert_eq!(ctrl.current_hunt_cap_usd(), d("3000"));
        }

        // Instant downward drop in bear reversal
        let mut bear_ctrl = RegimeNotionalController::new(
            RegimeSizingParams {
                ema_fast_cycles: 5,
                ema_slow_cycles: 20,
                vol_cycles: 5,
                ramp_up_alpha: 0.5,
                stale_cooldown_cycles: 3,
                ..Default::default()
            },
            None,
        );
        for i in 0..30 {
            bear_ctrl.update(100.0 + i as f64 * 2.0);
        }
        let peak = bear_ctrl.current_hunt_cap_usd();
        assert!(peak > d("15000"));
        for i in 0..25 {
            bear_ctrl.update(160.0 - i as f64 * 3.0);
        }
        assert!(bear_ctrl.current_hunt_cap_usd() < peak);
        assert!(bear_ctrl.current_hunt_cap_usd() >= d("3000"));
    }

    #[test]
    fn test_notional_capped_shares_respects_all_caps() {
        let min_m = 0.0036;
        let max_m = 0.0250;
        let level = 0.05;

        // Capped by max_notional_usd ($3,000) despite $1M cash
        let qty = calculate_notional_capped_shares(
            0.0250,
            min_m,
            max_m,
            level,
            d("3000"),
            d("15000"),
            d("1000000"),
            d("73.33"),
            true,
        );
        assert!(qty > Decimal::ZERO);
        assert!(qty * d("73.33") <= d("3000"));
        assert!(qty * d("73.33") >= d("2999"));

        // Capped by remaining_exposure_usd ($500)
        let qty_rem = calculate_notional_capped_shares(
            0.0250,
            min_m,
            max_m,
            level,
            d("30000"),
            d("500"),
            d("100000"),
            d("73.33"),
            true,
        );
        assert!(qty_rem * d("73.33") <= d("500"));
        assert!(qty_rem * d("73.33") >= d("499"));

        // Remaining exposure below $10 minimum notional -> 0
        assert_eq!(
            calculate_notional_capped_shares(
                0.0250,
                min_m,
                max_m,
                level,
                d("30000"),
                d("9.99"),
                d("100000"),
                d("73.33"),
                true,
            ),
            Decimal::ZERO
        );

        // Equity whole-share floor
        let eq = calculate_notional_capped_shares(
            0.0250,
            min_m,
            max_m,
            level,
            d("3000"),
            d("15000"),
            d("100000"),
            d("73.33"),
            false,
        );
        assert_eq!(eq, d("40"));
        assert!(eq * d("73.33") <= d("3000"));
    }
}
