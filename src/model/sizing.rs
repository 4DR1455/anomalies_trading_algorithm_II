/// Capital allocation and SEC Rule 612 compliant pricing module.

/// Calculate dynamic position size based on asymptotic formula from SPECIFICATION.md:
/// SharesAmount(x) = Max * (1 - (1 - x^level)^(1/level))
/// where x = (CurrentRange / MaxRange) normalized to [0, 1].
pub fn calculate_shares_amount(
    undervaluation_margin: f64,
    min_margin: f64,
    max_margin: f64,
    level: f64,
    max_shares: u32,
) -> u32 {
    if undervaluation_margin < min_margin || max_shares == 0 || level <= 0.0 {
        return 0;
    }
    if undervaluation_margin > max_margin {
        // Circuit breaker: halted
        return 0;
    }

    let denominator = max_margin - min_margin;
    if denominator <= 0.0 {
        return 0;
    }

    let x = ((undervaluation_margin - min_margin) / denominator).clamp(0.0, 1.0);

    // Compute 1 - (1 - x^level)^(1/level)
    let x_pow = x.powf(level);
    let inner = (1.0 - x_pow).clamp(0.0, 1.0);
    let factor = 1.0 - inner.powf(1.0 / level);

    let shares = (max_shares as f64 * factor).floor() as u32;
    // If signal triggered and above min_margin, allocate at least 1 share if max_shares >= 1
    if shares == 0 && max_shares > 0 && x > 0.0 {
        1
    } else {
        shares.min(max_shares)
    }
}

/// SEC Rule 612 Minimum Price Variation (MPV) tick size
pub fn get_tick_size(price: f64) -> f64 {
    if price >= 1.0 {
        0.01
    } else {
        0.0001
    }
}

/// Round price down to valid legal tick boundary
pub fn floor_to_tick(price: f64) -> f64 {
    if price <= 0.0 {
        return 0.0;
    }
    let tick = get_tick_size(price);
    let ticks = ((price / tick) + 1e-8).floor();
    (ticks * tick * 10000.0).round() / 10000.0
}

/// Round price to nearest valid legal tick boundary
pub fn round_to_tick(price: f64) -> f64 {
    if price <= 0.0 {
        return 0.0;
    }
    let tick = get_tick_size(price);
    let ticks = (price / tick).round();
    (ticks * tick * 10000.0).round() / 10000.0
}

/// Format price as strict SEC Rule 612 compliant decimal string
pub fn format_tick_price(price: f64) -> String {
    let rounded = round_to_tick(price);
    if rounded >= 1.0 {
        format!("{:.2}", rounded)
    } else {
        format!("{:.4}", rounded)
    }
}

/// Calculate optimal legal HiddenMM limit BUY price:
/// - Aims for maker queue priority without crossing the Best Ask.
/// - If spread >= 2 ticks: place at Best Bid + 1 tick (inside the NBBO).
/// - If spread == 1 tick: join Best Bid to remain passive maker.
pub fn calculate_hidden_mm_buy_price(best_bid: f64, best_ask: f64) -> f64 {
    let tick = get_tick_size(best_bid);
    let bid_tick = round_to_tick(best_bid);
    let ask_tick = round_to_tick(best_ask);

    let spread_ticks = ((ask_tick - bid_tick) / tick).round() as i64;
    if spread_ticks >= 2 {
        // Step inside the spread legally
        bid_tick + tick
    } else {
        // 1-tick spread: join the best bid
        bid_tick
    }
}

/// Calculate legal exit limit SELL price:
/// At or slightly below predicted fair price, but at least 1 tick above entry price.
pub fn calculate_exit_sell_price(entry_fill_price: f64, predicted_fair_price: f64) -> f64 {
    let tick = get_tick_size(predicted_fair_price);
    let target = floor_to_tick(predicted_fair_price);
    let min_profitable = round_to_tick(entry_fill_price + tick);
    target.max(min_profitable)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_asymptotic_sizing_bounds() {
        let min_m = 0.0010;
        let max_m = 0.0100;
        let max_s = 100;

        // Below min margin -> 0
        assert_eq!(calculate_shares_amount(0.0005, min_m, max_m, 2.0, max_s), 0);

        // At min margin -> 0 or 1
        let s_at_min = calculate_shares_amount(min_m, min_m, max_m, 2.0, max_s);
        assert!(s_at_min <= 1);

        // At max margin -> max_shares
        assert_eq!(calculate_shares_amount(max_m, min_m, max_m, 2.0, max_s), max_s);

        // Above max margin (circuit breaker) -> 0
        assert_eq!(calculate_shares_amount(0.0150, min_m, max_m, 2.0, max_s), 0);
    }

    #[test]
    fn test_sec_rule_612_compliance() {
        assert_eq!(format_tick_price(182.508), "182.51");
        assert_eq!(format_tick_price(182.502), "182.50");
        assert_eq!(format_tick_price(0.50254), "0.5025");

        let buy_p = calculate_hidden_mm_buy_price(182.50, 182.52);
        assert_eq!(format_tick_price(buy_p), "182.51");

        let buy_1tick = calculate_hidden_mm_buy_price(182.50, 182.51);
        assert_eq!(format_tick_price(buy_1tick), "182.50");
    }
}
