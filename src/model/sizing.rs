use crate::model::pricing::{decimal_from_f64_clean, floor_qty, MIN_ORDER_NOTIONAL_USD};
use rust_decimal::Decimal;

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
    if !undervaluation_margin.is_finite()
        || !min_margin.is_finite()
        || !max_margin.is_finite()
        || !level.is_finite()
    {
        return Decimal::ZERO;
    }

    if undervaluation_margin < min_margin
        || undervaluation_margin > max_margin
        || level <= 0.0
        || max_shares_param <= Decimal::ZERO
        || buy_price <= Decimal::ZERO
        || available_cash <= Decimal::ZERO
    {
        return Decimal::ZERO;
    }

    let denom = max_margin - min_margin;
    if denom <= 0.0 {
        return Decimal::ZERO;
    }

    let x = ((undervaluation_margin - min_margin) / denom).clamp(0.0, 1.0);
    let x_pow = x.powf(level);
    let inner = (1.0 - x_pow).clamp(0.0, 1.0);
    let factor_f64 = (1.0 - inner.powf(1.0 / level)).clamp(0.0, 1.0);

    let factor = match decimal_from_f64_clean(factor_f64) {
        Some(f) if f > Decimal::ZERO => f.clamp(Decimal::ZERO, Decimal::ONE),
        _ => return Decimal::ZERO,
    };

    let qty_decimals: u32 = if is_crypto { 4 } else { 0 };
    let step = Decimal::new(1, qty_decimals);
    let safe_cash = available_cash.max(Decimal::ZERO);

    let (mut qty, capital_cap) = if max_shares_param <= Decimal::ONE {
        // Proportion of total portfolio equity mode (e.g. 1.0 = up to 100% equity, capped by available cash)
        let equity_cap = total_equity.max(Decimal::ZERO) * max_shares_param;
        let target_capital = factor * equity_cap;
        let allocated_capital = target_capital.min(safe_cash).min(equity_cap);
        if allocated_capital < MIN_ORDER_NOTIONAL_USD {
            return Decimal::ZERO;
        }
        (
            floor_qty(allocated_capital / buy_price, qty_decimals),
            allocated_capital,
        )
    } else {
        // Legacy absolute unit shares mode, still strictly capped by available cash
        let raw_shares = factor * max_shares_param;
        let max_affordable = floor_qty(safe_cash / buy_price, qty_decimals);
        (
            floor_qty(
                raw_shares.min(max_shares_param).min(max_affordable),
                qty_decimals,
            ),
            safe_cash,
        )
    };

    // Strict post-condition: qty * buy_price MUST NEVER exceed capital_cap (min(available_cash, equity * max_shares))
    while qty > Decimal::ZERO
        && qty
            .checked_mul(buy_price)
            .is_none_or(|notional| notional > capital_cap)
    {
        qty = (qty - step).max(Decimal::ZERO);
    }

    // Enforce $10.00 minimum notional after flooring
    if qty <= Decimal::ZERO || qty * buy_price < MIN_ORDER_NOTIONAL_USD {
        return Decimal::ZERO;
    }

    qty
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
}
