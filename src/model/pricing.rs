use rust_decimal::prelude::ToPrimitive;
use rust_decimal::{Decimal, RoundingStrategy};
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use thiserror::Error;

/// Alpaca Crypto Tier 1 maker fee rate (0.15%, deducted in base asset on buys, USD on sells).
pub const CRYPTO_TIER1_MAKER_FEE_RATE: Decimal = Decimal::from_parts(15, 0, 0, false, 4);
/// Alpaca Crypto Tier 1 taker fee rate (0.25%, worst-case ceiling used for exit safety floor).
pub const CRYPTO_TIER1_TAKER_FEE_RATE: Decimal = Decimal::from_parts(25, 0, 0, false, 4);
/// Minimum required net profit margin on normal crypto exit limit sells (3 bps = 0.0003).
pub const MIN_NET_PROFIT_MARGIN: Decimal = Decimal::from_parts(3, 0, 0, false, 4);
/// Explicit USD safety buffer covering Alpaca's cent-ceiling (`CFEE`) rounding across multi-fill
/// splits on sell orders ($0.08 covers up to k=7-8 adversarial fills even on $10 minimum orders).
pub const CFEE_CEIL_BUFFER_USD: Decimal = Decimal::from_parts(8, 0, 0, false, 2);
/// Minimum non-dust crypto quantity fallback (0.001 units) used only when no reference price exists.
pub const CRYPTO_DUST_QTY: Decimal = Decimal::from_parts(1, 0, 0, false, 3);
/// Minimum notional value required to place protective exit orders ($1.00).
pub const MIN_PROTECT_NOTIONAL_USD: Decimal = Decimal::from_parts(100, 0, 0, false, 2);
/// Minimum Alpaca crypto/portfolio order notional ($10.00).
pub const MIN_ORDER_NOTIONAL_USD: Decimal = Decimal::from_parts(1000, 0, 0, false, 2);
/// Strict half-cent threshold ($0.005): any unrounded net P&L strictly greater than $0.005
/// guarantees a 2dp `MidpointNearestEven`-rounded `net_pnl >= $0.01`.
pub const HALF_CENT_USD: Decimal = Decimal::from_parts(5, 0, 0, false, 3);

/// SEC Rule 612 tick sizes: $0.01 at or above $1.00, $0.0001 below $1.00.
pub const TICK_ABOVE_ONE: Decimal = Decimal::from_parts(1, 0, 0, false, 2);
pub const TICK_BELOW_ONE: Decimal = Decimal::from_parts(1, 0, 0, false, 4);

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum PricingError {
    #[error("Invalid pricing input: {0}")]
    InvalidInput(String),
    #[error("Non-positive break-even denominator: {0}")]
    NonPositiveDenominator(String),
    #[error("Decimal arithmetic overflow: {0}")]
    Overflow(String),
}

/// Converts a finite `f64` into a normalized base-10 `Decimal` without binary IEEE-754 mantissa noise.
pub fn decimal_from_f64_clean(val: f64) -> Option<Decimal> {
    if !val.is_finite() {
        return None;
    }
    if val == 0.0 {
        return Some(Decimal::ZERO);
    }
    let parsed = Decimal::from_str(&val.to_string())
        .ok()
        .or_else(|| Decimal::try_from(val).ok())?;
    if parsed.is_zero() {
        Some(Decimal::ZERO)
    } else {
        Some(parsed.normalize())
    }
}

/// Converts `Decimal` to `f64` safely for display/telemetry/JSON serialization.
pub fn decimal_to_f64(val: Decimal) -> f64 {
    val.to_f64().unwrap_or(0.0)
}

/// SEC Rule 612 Minimum Price Variation (MPV): $0.01 for prices >= $1.00, else $0.0001.
pub fn get_tick_size(price: Decimal) -> Decimal {
    if price >= Decimal::ONE {
        TICK_ABOVE_ONE
    } else {
        TICK_BELOW_ONE
    }
}

/// Floors a positive price down to its legal SEC Rule 612 tick boundary (`RoundingStrategy::ToZero`).
pub fn floor_to_tick(price: Decimal) -> Decimal {
    if price <= Decimal::ZERO {
        return Decimal::ZERO;
    }
    let scale = if price >= Decimal::ONE { 2 } else { 4 };
    price.round_dp_with_strategy(scale, RoundingStrategy::ToZero)
}

/// Rounds a positive price UP to its legal SEC Rule 612 tick boundary (`RoundingStrategy::AwayFromZero`).
pub fn ceil_to_tick(price: Decimal) -> Decimal {
    if price <= Decimal::ZERO {
        return Decimal::ZERO;
    }
    let scale = if price >= Decimal::ONE { 2 } else { 4 };
    let ceiled = price.round_dp_with_strategy(scale, RoundingStrategy::AwayFromZero);
    if ceiled >= Decimal::ONE {
        ceiled.round_dp_with_strategy(2, RoundingStrategy::AwayFromZero)
    } else {
        ceiled
    }
}

/// Rounds a positive price to its nearest legal SEC Rule 612 tick boundary.
pub fn round_to_tick(price: Decimal) -> Decimal {
    if price <= Decimal::ZERO {
        return Decimal::ZERO;
    }
    let scale = if price >= Decimal::ONE { 2 } else { 4 };
    let rounded = price.round_dp_with_strategy(scale, RoundingStrategy::MidpointAwayFromZero);
    if rounded >= Decimal::ONE {
        rounded.round_dp_with_strategy(2, RoundingStrategy::MidpointAwayFromZero)
    } else {
        rounded
    }
}

/// Formats a tick-compliant price string (`2` decimals >= $1.00, `4` decimals < $1.00).
pub fn format_tick_price(price: Decimal) -> String {
    let rounded = round_to_tick(price);
    if rounded >= Decimal::ONE {
        format!("{:.2}", rounded)
    } else {
        format!("{:.4}", rounded)
    }
}

/// Strictly floors a positive quantity down to `decimals` fractional places so we NEVER exceed available balance.
pub fn floor_qty(qty: Decimal, decimals: u32) -> Decimal {
    if qty <= Decimal::ZERO {
        return Decimal::ZERO;
    }
    qty.round_dp_with_strategy(decimals, RoundingStrategy::ToZero)
}

/// Formats an order quantity strictly floored without scientific notation (`9` decimals crypto, `4` decimals stocks).
pub fn format_order_qty(qty: Decimal, is_crypto: bool) -> String {
    if qty <= Decimal::ZERO {
        return "0".to_string();
    }
    let decimals = if is_crypto { 9 } else { 4 };
    let floored = floor_qty(qty, decimals).normalize();
    if floored.is_zero() {
        "0".to_string()
    } else {
        floored.to_string()
    }
}

/// Rounds a USD fee amount UP to the nearest cent, matching Alpaca's sell-side `CFEE` cent-ceiling schedule.
pub fn ceil_usd_cents(amount: Decimal) -> Decimal {
    if amount <= Decimal::ZERO {
        return Decimal::ZERO;
    }
    amount.round_dp_with_strategy(2, RoundingStrategy::AwayFromZero)
}

/// Canonical notional-aware dust predicate preventing high-priced crypto remainders
/// (e.g. `0.001 BTC = $90`) from ever being misclassified as un-sellable dust, while
/// recognizing genuinely un-sellable sub-`$1.00` (`MIN_PROTECT_NOTIONAL_USD`) remainders.
pub fn is_dust_qty(qty: Decimal, ref_price: Decimal, _is_crypto: bool) -> bool {
    if qty <= Decimal::ZERO {
        return true;
    }
    if ref_price > Decimal::ZERO {
        qty.checked_mul(ref_price)
            .is_some_and(|notional| notional < MIN_PROTECT_NOTIONAL_USD)
    } else {
        qty <= CRYPTO_DUST_QTY
    }
}

/// Calculates a strictly passive, non-crossing HiddenMM limit BUY price on a legal tick grid.
///
/// Invariants:
/// - Rejects invalid/crossed/locked quotes (`bid <= 0 || ask <= bid`).
/// - Always returns `Some(p)` with `0 < p < ask` strictly (guaranteed passive maker, never crosses Best Ask).
/// - Steps 1 tick inside the spread (`bid_floor + tick`) iff that price remains strictly below `ask`;
///   otherwise joins `bid_floor`.
pub fn calculate_hidden_mm_buy_price(bid: Decimal, ask: Decimal) -> Option<Decimal> {
    if bid <= Decimal::ZERO || ask <= bid {
        return None;
    }

    let tick = get_tick_size(bid);
    let bid_floor = floor_to_tick(bid);
    let ask_ticks_ceil = (ask / tick).ceil();
    let strictly_below_ask = (ask_ticks_ceil - Decimal::ONE) * tick;

    let candidate = if bid_floor + tick <= strictly_below_ask {
        bid_floor + tick
    } else {
        bid_floor
    };

    // Normalize across the $1.00 tick boundary if candidate stepped onto >= $1.00
    let buy_price = if candidate >= Decimal::ONE {
        floor_to_tick(candidate)
    } else {
        candidate
    };

    if buy_price <= Decimal::ZERO || buy_price >= ask {
        None
    } else {
        Some(buy_price)
    }
}

/// Computes a mathematically guaranteed net-profitable exit limit SELL price in exact `Decimal`.
///
/// For crypto:
/// - Enforces worst-case Tier 1 taker sell fee floor: `effective_worst_sell_fee = worst_sell_fee_rate.max(0.0025)`.
/// - Solves `(sell_qty * exit_price) * (1 - effective_worst_sell_fee - min_net_profit_margin) >= cost_basis + ceil_buffer_usd + $0.005`,
///   rounding UP to a legal tick.
/// - Floors at `ceil_to_tick(avg_buy_price + tick)` and `ceil_to_tick(fair_price)`.
/// - Post-verifies strict 2dp-positive profitability (`> cost_basis + $0.005`, guaranteeing
///   2dp-rounded `net_pnl >= $0.01`) against BOTH the linear buffer AND exact cent-ceiled `CFEE`,
///   bumping by +1 tick if boundary rounding ever falls at or below the half-cent threshold.
///
/// For stocks (`!is_crypto`):
/// - Floors at `ceil_to_tick(avg_buy_price + tick).max(ceil_to_tick(fair_price))` and guarantees
///   `(sell_qty * exit_price - cost_basis) > $0.005` AND `(exit_price - avg_buy_price) * sell_qty > $0.005`
///   so even sub-$1.00 equity positions realize 2dp-rounded `net_pnl >= $0.01`.
#[allow(clippy::too_many_arguments)]
pub fn compute_guaranteed_profitable_exit_price(
    buy_qty: Decimal,
    avg_buy_price: Decimal,
    sell_qty: Decimal,
    fair_price: Decimal,
    worst_sell_fee_rate: Decimal,
    min_net_profit_margin: Decimal,
    ceil_buffer_usd: Decimal,
    is_crypto: bool,
) -> Result<Decimal, PricingError> {
    if buy_qty <= Decimal::ZERO {
        return Err(PricingError::InvalidInput(
            "buy_qty must be strictly positive".to_string(),
        ));
    }
    if avg_buy_price <= Decimal::ZERO {
        return Err(PricingError::InvalidInput(
            "avg_buy_price must be strictly positive".to_string(),
        ));
    }
    if sell_qty <= Decimal::ZERO {
        return Err(PricingError::InvalidInput(
            "sell_qty must be strictly positive".to_string(),
        ));
    }
    if worst_sell_fee_rate < Decimal::ZERO
        || min_net_profit_margin < Decimal::ZERO
        || ceil_buffer_usd < Decimal::ZERO
    {
        return Err(PricingError::InvalidInput(
            "fee rates, margin, and buffer must be non-negative".to_string(),
        ));
    }

    let cost_basis = buy_qty
        .checked_mul(avg_buy_price)
        .ok_or_else(|| PricingError::Overflow("buy_qty * avg_buy_price".to_string()))?;
    let target_floor = cost_basis
        .checked_add(HALF_CENT_USD)
        .ok_or_else(|| PricingError::Overflow("cost_basis + HALF_CENT_USD".to_string()))?;

    let min_tick_above_buy = ceil_to_tick(avg_buy_price + get_tick_size(avg_buy_price));
    let fair_exit = if fair_price > Decimal::ZERO {
        ceil_to_tick(fair_price)
    } else {
        Decimal::ZERO
    };

    if !is_crypto {
        let raw_half_cent_exit = target_floor
            .checked_div(sell_qty)
            .ok_or_else(|| PricingError::Overflow("target_floor / sell_qty".to_string()))?;
        let raw_spread_half_cent_exit = avg_buy_price
            .checked_add(
                HALF_CENT_USD
                    .checked_div(sell_qty)
                    .ok_or_else(|| PricingError::Overflow("HALF_CENT_USD / sell_qty".to_string()))?,
            )
            .ok_or_else(|| PricingError::Overflow("avg_buy_price + half_cent/sell_qty".to_string()))?;
        let mut exit_price = min_tick_above_buy
            .max(fair_exit)
            .max(ceil_to_tick(raw_half_cent_exit))
            .max(ceil_to_tick(raw_spread_half_cent_exit));

        let mut safety_bumps = 0u32;
        while safety_bumps < 100 {
            let gross_proceeds = sell_qty
                .checked_mul(exit_price)
                .ok_or_else(|| PricingError::Overflow("sell_qty * exit_price".to_string()))?;
            let spread_pnl = (exit_price - avg_buy_price)
                .checked_mul(sell_qty)
                .ok_or_else(|| {
                    PricingError::Overflow("(exit_price - avg_buy_price) * sell_qty".to_string())
                })?;

            if gross_proceeds - cost_basis > HALF_CENT_USD && spread_pnl > HALF_CENT_USD {
                break;
            }
            let tick = get_tick_size(exit_price);
            exit_price = ceil_to_tick(exit_price + tick);
            safety_bumps += 1;
        }
        return Ok(exit_price);
    }

    let effective_worst_sell_fee = worst_sell_fee_rate.max(CRYPTO_TIER1_TAKER_FEE_RATE);
    let retention = Decimal::ONE - effective_worst_sell_fee - min_net_profit_margin;
    if retention <= Decimal::ZERO {
        return Err(PricingError::NonPositiveDenominator(format!(
            "1 - fee ({}) - margin ({}) <= 0",
            effective_worst_sell_fee, min_net_profit_margin
        )));
    }

    let denom = sell_qty
        .checked_mul(retention)
        .ok_or_else(|| PricingError::Overflow("sell_qty * retention".to_string()))?;
    if denom <= Decimal::ZERO {
        return Err(PricingError::NonPositiveDenominator(
            "sell_qty * retention <= 0".to_string(),
        ));
    }

    let numerator = target_floor
        .checked_add(ceil_buffer_usd)
        .ok_or_else(|| {
            PricingError::Overflow("cost_basis + HALF_CENT_USD + ceil_buffer_usd".to_string())
        })?;
    let raw_min_exit = numerator
        .checked_div(denom)
        .ok_or_else(|| PricingError::Overflow("numerator / denom".to_string()))?;

    let min_profitable_exit = ceil_to_tick(raw_min_exit);
    let mut exit_price = min_profitable_exit.max(min_tick_above_buy).max(fair_exit);

    // Post-condition verification: guarantee strict 2dp-positive net profit (> cost_basis + $0.005)
    // after worst-case taker fee + CFEE buffer AND after single-fill cent-ceiled CFEE.
    let mut safety_bumps = 0u32;
    while safety_bumps < 1000 {
        let gross_proceeds = sell_qty
            .checked_mul(exit_price)
            .ok_or_else(|| PricingError::Overflow("sell_qty * exit_price".to_string()))?;
        let linear_net =
            gross_proceeds * (Decimal::ONE - effective_worst_sell_fee) - ceil_buffer_usd;
        let cfee_net = gross_proceeds - ceil_usd_cents(gross_proceeds * effective_worst_sell_fee);

        if linear_net > target_floor && cfee_net > target_floor {
            break;
        }
        let tick = get_tick_size(exit_price);
        exit_price = ceil_to_tick(exit_price + tick);
        safety_bumps += 1;
    }

    Ok(exit_price)
}

/// Alpaca Crypto Volume Fee Tier schedule (Tiers 1–5).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CryptoFeeTier {
    pub tier: u8,
    pub maker_rate: Decimal,
    pub taker_rate: Decimal,
}

pub const ALPACA_CRYPTO_FEE_TIERS: [CryptoFeeTier; 5] = [
    CryptoFeeTier {
        tier: 1,
        maker_rate: Decimal::from_parts(15, 0, 0, false, 4), // 0.15%
        taker_rate: Decimal::from_parts(25, 0, 0, false, 4), // 0.25%
    },
    CryptoFeeTier {
        tier: 2,
        maker_rate: Decimal::from_parts(12, 0, 0, false, 4), // 0.12%
        taker_rate: Decimal::from_parts(22, 0, 0, false, 4), // 0.22%
    },
    CryptoFeeTier {
        tier: 3,
        maker_rate: Decimal::from_parts(10, 0, 0, false, 4), // 0.10%
        taker_rate: Decimal::from_parts(20, 0, 0, false, 4), // 0.20%
    },
    CryptoFeeTier {
        tier: 4,
        maker_rate: Decimal::from_parts(8, 0, 0, false, 4), // 0.08%
        taker_rate: Decimal::from_parts(18, 0, 0, false, 4), // 0.18%
    },
    CryptoFeeTier {
        tier: 5,
        maker_rate: Decimal::from_parts(5, 0, 0, false, 4), // 0.05%
        taker_rate: Decimal::from_parts(15, 0, 0, false, 4), // 0.15%
    },
];

/// Dynamic crypto fee-tier tracker that starts conservatively at Tier 1 (0.15%/0.25%),
/// calibrates from realized buy-fill base-asset deltas or Alpaca `CFEE` USD activities,
/// and ALWAYS preserves a conservative `>= 0.25%` worst-case sell fee floor for exit pricing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeeTracker {
    tier: u8,
    maker_rate: Decimal,
    taker_rate: Decimal,
}

impl Default for FeeTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl FeeTracker {
    pub fn new() -> Self {
        Self {
            tier: 1,
            maker_rate: CRYPTO_TIER1_MAKER_FEE_RATE,
            taker_rate: CRYPTO_TIER1_TAKER_FEE_RATE,
        }
    }

    pub fn current_tier(&self) -> CryptoFeeTier {
        CryptoFeeTier {
            tier: self.tier,
            maker_rate: self.maker_rate,
            taker_rate: self.taker_rate,
        }
    }

    pub fn maker_fee_rate(&self) -> Decimal {
        self.maker_rate
    }

    pub fn taker_fee_rate(&self) -> Decimal {
        self.taker_rate
    }

    /// Always returns at least Tier 1 taker rate (0.0025) so exit break-even pricing remains safe
    /// even if volume drops across a 30-day tier boundary or a passive exit turns taker.
    pub fn worst_sell_fee_rate(&self) -> Decimal {
        self.taker_rate.max(CRYPTO_TIER1_TAKER_FEE_RATE)
    }

    /// Conservative worst-case buy fee rate used when reconstructing gross cost basis of orphans.
    pub fn worst_buy_fee_rate(&self) -> Decimal {
        self.taker_rate.max(CRYPTO_TIER1_TAKER_FEE_RATE)
    }

    /// Calibrates fee tier from an observed crypto buy fill: `observed_rate = 1 - net_qty / buy_qty`.
    pub fn calibrate_from_buy_fill(
        &mut self,
        buy_qty: Decimal,
        net_qty: Decimal,
    ) -> Option<CryptoFeeTier> {
        if buy_qty <= Decimal::ZERO || net_qty <= Decimal::ZERO || net_qty >= buy_qty {
            return None;
        }
        let observed = Decimal::ONE - (net_qty / buy_qty);
        // Passive HiddenMM buys execute as maker first; only consider taker tiers if rate > 0.165%
        // so Tier 1 maker (0.15%) is never misclassified as Tier 5 taker (0.15%).
        let taker_threshold = Decimal::from_parts(165, 0, 0, false, 5); // 0.00165
        let is_maker = observed <= taker_threshold;
        self.apply_observed_rate(observed, is_maker)
    }

    /// Calibrates fee tier from an observed Alpaca `CFEE` USD sell fee and gross USD notional.
    pub fn calibrate_from_cfee(
        &mut self,
        fee_usd: Decimal,
        gross_notional_usd: Decimal,
        is_maker: bool,
    ) -> Option<CryptoFeeTier> {
        // Require >= $100 notional so $0.01 ceiling distortion is < 1 bp
        let min_notional = Decimal::from(100);
        if fee_usd <= Decimal::ZERO || gross_notional_usd < min_notional {
            return None;
        }
        let observed = fee_usd / gross_notional_usd;
        self.apply_observed_rate(observed, is_maker)
    }

    fn apply_observed_rate(&mut self, observed: Decimal, is_maker: bool) -> Option<CryptoFeeTier> {
        let tolerance = Decimal::from_parts(15, 0, 0, false, 5); // 0.00015 (1.5 bps)
        let mut best: Option<(Decimal, CryptoFeeTier)> = None;

        for tier in ALPACA_CRYPTO_FEE_TIERS {
            let reference = if is_maker {
                tier.maker_rate
            } else {
                tier.taker_rate
            };
            let diff = (observed - reference).abs();
            if diff <= tolerance {
                match best {
                    None => best = Some((diff, tier)),
                    Some((best_diff, _)) if diff < best_diff => best = Some((diff, tier)),
                    _ => {}
                }
            }
        }

        if let Some((_, matched)) = best {
            self.tier = matched.tier;
            self.maker_rate = matched.maker_rate;
            self.taker_rate = matched.taker_rate;
            Some(matched)
        } else {
            None
        }
    }
}

/// Computes exact fee-inclusive realized P&L in base-10 `Decimal`.
///
/// Supports:
/// - `actual_sell_fee_usd`: exact USD fee from Alpaca `CFEE` activities when available (otherwise
///   uses `ceil_usd_cents(gross_proceeds * sell_fee_rate)`).
/// - `buy_fee_already_deducted`: prevents double-charging buy fees on adopted/orphan records
///   where `buy_qty == sell_qty` already represents the post-buy-fee net quantity held at Alpaca.
///
/// Returns `(net_pnl, pnl_pct, gross_pnl, total_fees_usd)`.
#[allow(clippy::too_many_arguments)]
pub fn compute_net_trade_pnl(
    buy_price: Decimal,
    sell_price: Decimal,
    buy_qty: Decimal,
    sell_qty: Decimal,
    is_crypto: bool,
    buy_fee_rate: Decimal,
    sell_fee_rate: Decimal,
    actual_sell_fee_usd: Option<Decimal>,
    buy_fee_already_deducted: bool,
) -> (Decimal, Decimal, Decimal, Decimal) {
    let s_qty = sell_qty.max(Decimal::ZERO);
    let b_qty = if buy_qty > Decimal::ZERO {
        buy_qty
    } else {
        s_qty
    };

    let cost_basis = buy_price * b_qty;
    let gross_proceeds = sell_price * s_qty;

    if !is_crypto {
        let gross_pnl = (sell_price - buy_price) * s_qty;
        let pnl_pct = if cost_basis > Decimal::ZERO {
            (gross_pnl / cost_basis).round_dp_with_strategy(4, RoundingStrategy::MidpointNearestEven)
        } else {
            Decimal::ZERO
        };
        let rounded = gross_pnl.round_dp_with_strategy(2, RoundingStrategy::MidpointNearestEven);
        return (rounded, pnl_pct, rounded, Decimal::ZERO);
    }

    let sell_fee_usd = match actual_sell_fee_usd {
        Some(actual) if actual >= Decimal::ZERO => actual,
        _ => ceil_usd_cents(gross_proceeds * sell_fee_rate),
    };

    let (buy_fee_usd, gross_pnl, net_pnl) = if b_qty > s_qty {
        // Buy fee was already deducted in base asset (exact Decimal b_qty > s_qty; avoids 1e-7
        // epsilon double-counting on tiny-qty high-price assets like BTC)
        let b_fee = (b_qty - s_qty) * buy_price;
        let g_pnl = (sell_price - buy_price) * b_qty;
        let n_pnl = (gross_proceeds - sell_fee_usd) - cost_basis;
        (b_fee, g_pnl, n_pnl)
    } else if buy_fee_already_deducted {
        // Adopted/orphan net-balance record: do not double-charge buy fee
        let g_pnl = (sell_price - buy_price) * s_qty;
        let n_pnl = (gross_proceeds - sell_fee_usd) - cost_basis;
        (Decimal::ZERO, g_pnl, n_pnl)
    } else {
        // Legacy / cushion trade where full gross buy_qty was sold
        let b_fee = cost_basis * buy_fee_rate;
        let g_pnl = (sell_price - buy_price) * s_qty;
        let n_pnl = (gross_proceeds - sell_fee_usd) - cost_basis - b_fee;
        (b_fee, g_pnl, n_pnl)
    };

    let total_fees_usd = buy_fee_usd + sell_fee_usd;
    let pnl_pct = if cost_basis > Decimal::ZERO {
        (net_pnl / cost_basis).round_dp_with_strategy(4, RoundingStrategy::MidpointNearestEven)
    } else {
        Decimal::ZERO
    };

    (
        net_pnl.round_dp_with_strategy(2, RoundingStrategy::MidpointNearestEven),
        pnl_pct,
        gross_pnl.round_dp_with_strategy(2, RoundingStrategy::MidpointNearestEven),
        total_fees_usd.round_dp_with_strategy(2, RoundingStrategy::MidpointNearestEven),
    )
}

/// Shared serde helpers that parse `Decimal` losslessly from JSON strings, integers, floats, or null,
/// and serialize `Decimal` as JSON numbers for 100% schema parity with the Python dashboard & telemetry.
pub mod serde_decimal {
    use super::{decimal_from_f64_clean, decimal_to_f64};
    use rust_decimal::Decimal;
    use serde::de::{self, Visitor};
    use serde::{Deserializer, Serializer};
    use std::fmt;
    use std::str::FromStr;

    struct OptDecimalVisitor;

    impl<'de> Visitor<'de> for OptDecimalVisitor {
        type Value = Option<Decimal>;

        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("a decimal string, number, or null")
        }

        fn visit_none<E>(self) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(None)
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(None)
        }

        fn visit_some<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
        where
            D: Deserializer<'de>,
        {
            deserializer.deserialize_any(self)
        }

        fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            let trimmed = v.trim();
            if trimmed.is_empty() {
                return Ok(None);
            }
            Decimal::from_str(trimmed)
                .map(|d| Some(d.normalize()))
                .map_err(de::Error::custom)
        }

        fn visit_string<E>(self, v: String) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            self.visit_str(&v)
        }

        fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(Some(Decimal::from(v)))
        }

        fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            Ok(Some(Decimal::from(v)))
        }

        fn visit_f64<E>(self, v: f64) -> Result<Self::Value, E>
        where
            E: de::Error,
        {
            decimal_from_f64_clean(v)
                .map(Some)
                .ok_or_else(|| de::Error::custom(format!("non-finite float: {}", v)))
        }
    }

    pub fn deserialize_opt_decimal<'de, D>(deserializer: D) -> Result<Option<Decimal>, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_any(OptDecimalVisitor)
    }

    pub fn deserialize_decimal<'de, D>(deserializer: D) -> Result<Decimal, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserialize_opt_decimal(deserializer)?
            .ok_or_else(|| de::Error::custom("expected decimal value, found null or empty string"))
    }

    pub fn deserialize_decimal_or_zero<'de, D>(deserializer: D) -> Result<Decimal, D::Error>
    where
        D: Deserializer<'de>,
    {
        Ok(deserialize_opt_decimal(deserializer)?.unwrap_or(Decimal::ZERO))
    }

    pub fn serialize_decimal_as_f64<S>(val: &Decimal, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_f64(decimal_to_f64(*val))
    }

    pub fn serialize_opt_decimal_as_f64<S>(
        val: &Option<Decimal>,
        serializer: S,
    ) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match val {
            Some(d) => serializer.serialize_f64(decimal_to_f64(*d)),
            None => serializer.serialize_none(),
        }
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
    fn test_hidden_mm_buy_price_strictly_passive() {
        // Wide spread (>= 2 ticks): steps inside by 1 tick
        assert_eq!(calculate_hidden_mm_buy_price(d("72.50"), d("72.52")), Some(d("72.51")));
        // 1-tick spread: joins best bid
        assert_eq!(calculate_hidden_mm_buy_price(d("72.50"), d("72.51")), Some(d("72.50")));
        // Sub-penny crypto quote: never crosses ask
        assert_eq!(calculate_hidden_mm_buy_price(d("72.504"), d("72.509")), Some(d("72.50")));
        assert_eq!(calculate_hidden_mm_buy_price(d("72.509"), d("72.511")), Some(d("72.51")));
        // $1.00 boundary handling
        assert_eq!(calculate_hidden_mm_buy_price(d("0.9995"), d("1.00")), Some(d("0.9996")));
        assert_eq!(calculate_hidden_mm_buy_price(d("0.9999"), d("1.00")), Some(d("0.9999")));
        assert_eq!(calculate_hidden_mm_buy_price(d("0.9999"), d("1.01")), Some(d("1.00")));
        // Invalid / locked / crossed quotes rejected
        assert_eq!(calculate_hidden_mm_buy_price(d("72.50"), d("72.50")), None);
        assert_eq!(calculate_hidden_mm_buy_price(d("72.51"), d("72.50")), None);
        assert_eq!(calculate_hidden_mm_buy_price(d("0"), d("72.50")), None);
    }

    #[test]
    fn test_guaranteed_profitable_exit_price_clears_worst_case_fees() {
        // Replay deb40eea-style small margin trade where fair_price alone would lose after fees
        let buy_qty = d("100");
        let avg_buy = d("72.00");
        let sell_qty = d("99.85"); // after 0.15% maker buy fee
        let low_fair = d("72.18"); // only +0.25% above buy -> would be negative net PnL!

        let exit_px = compute_guaranteed_profitable_exit_price(
            buy_qty,
            avg_buy,
            sell_qty,
            low_fair,
            CRYPTO_TIER1_TAKER_FEE_RATE,
            MIN_NET_PROFIT_MARGIN,
            CFEE_CEIL_BUFFER_USD,
            true,
        )
        .unwrap();

        assert!(exit_px > low_fair);
        let (net_pnl, _, _, _) = compute_net_trade_pnl(
            avg_buy,
            exit_px,
            buy_qty,
            sell_qty,
            true,
            CRYPTO_TIER1_MAKER_FEE_RATE,
            CRYPTO_TIER1_TAKER_FEE_RATE, // even under worst-case 0.25% taker sell fee!
            None,
            false,
        );
        assert!(net_pnl > Decimal::ZERO, "exit_px={} produced net_pnl={}", exit_px, net_pnl);
    }

    #[test]
    fn test_fee_tracker_calibration_preserves_worst_case_floor() {
        let mut tracker = FeeTracker::new();
        assert_eq!(tracker.maker_fee_rate(), d("0.0015"));
        assert_eq!(tracker.worst_sell_fee_rate(), d("0.0025"));

        // Observe Tier 4 maker buy fill (0.08% deducted in SOL)
        let matched = tracker.calibrate_from_buy_fill(d("1000"), d("999.20")).unwrap();
        assert_eq!(matched.tier, 4);
        assert_eq!(tracker.maker_fee_rate(), d("0.0008"));
        assert_eq!(tracker.taker_fee_rate(), d("0.0018"));
        // Conservative worst-case sell fee floor MUST remain >= 0.0025!
        assert_eq!(tracker.worst_sell_fee_rate(), d("0.0025"));

        // Observe Tier 2 CFEE on $10,000 maker sell ($12.00)
        let matched2 = tracker.calibrate_from_cfee(d("12.00"), d("10000"), true).unwrap();
        assert_eq!(matched2.tier, 2);
        assert_eq!(tracker.maker_fee_rate(), d("0.0012"));
        assert_eq!(tracker.worst_sell_fee_rate(), d("0.0025"));
    }

    #[test]
    fn test_compute_net_trade_pnl_matches_python_and_no_double_charge() {
        // Matches test_live_bot.py line 291: bought 100 @ 70.00, sold 99.85 @ 70.50
        let (net, _pct, gross, fees) = compute_net_trade_pnl(
            d("70.00"),
            d("70.50"),
            d("100"),
            d("99.85"),
            true,
            CRYPTO_TIER1_MAKER_FEE_RATE,
            CRYPTO_TIER1_MAKER_FEE_RATE,
            None,
            false,
        );
        assert_eq!(net, d("28.86"));
        assert_eq!(gross, d("50.00"));
        assert_eq!(fees, d("21.06"));

        // Adopted/orphan record with buy_fee_already_deducted = true must NOT deduct buy fee again
        let (net_adp, _, _, fees_adp) = compute_net_trade_pnl(
            d("70.00"),
            d("70.50"),
            d("99.85"),
            d("99.85"),
            true,
            CRYPTO_TIER1_MAKER_FEE_RATE,
            CRYPTO_TIER1_MAKER_FEE_RATE,
            Some(d("10.56")),
            true,
        );
        assert_eq!(fees_adp, d("10.56"));
        assert_eq!(net_adp, d("39.36")); // 99.85 * 0.50 = 49.925 - 10.56 = 39.365 -> 39.36
    }

    #[test]
    fn test_format_order_qty_no_scientific_and_strictly_floored() {
        assert_eq!(format_order_qty(d("332.3059805739"), true), "332.305980573");
        assert_eq!(format_order_qty(d("0.00001"), true), "0.00001");
        assert_eq!(format_order_qty(d("100.0000"), true), "100");
        assert_eq!(floor_qty(d("1.9999999999"), 9), d("1.999999999"));
    }

    #[test]
    fn test_ce11_multifill_cfee_buffer_and_stock_sub_dollar_rounding() {
        // CE-1.1: $12 tight trade covers up to k=7 adversarial fills with 2dp net_pnl >= $0.01
        let buy_qty = d("0.16667");
        let buy_px = d("72.00");
        let sell_qty = floor_qty(buy_qty * (Decimal::ONE - CRYPTO_TIER1_TAKER_FEE_RATE), 9);
        let exit_px = compute_guaranteed_profitable_exit_price(
            buy_qty,
            buy_px,
            sell_qty,
            d("72.10"),
            CRYPTO_TIER1_TAKER_FEE_RATE,
            MIN_NET_PROFIT_MARGIN,
            CFEE_CEIL_BUFFER_USD,
            true,
        )
        .unwrap();
        let gross = sell_qty * exit_px;
        let base_cfee = ceil_usd_cents(gross * CRYPTO_TIER1_TAKER_FEE_RATE);
        for k in 1..=7usize {
            let worst_cfee = base_cfee + Decimal::new((k - 1) as i64, 2);
            let (net, _, _, _) = compute_net_trade_pnl(
                buy_px,
                exit_px,
                buy_qty,
                sell_qty,
                true,
                CRYPTO_TIER1_TAKER_FEE_RATE,
                CRYPTO_TIER1_TAKER_FEE_RATE,
                Some(worst_cfee),
                false,
            );
            assert!(
                net >= d("0.01"),
                "k={} adversarial fills on $12 trade must remain >= $0.01, got {}",
                k,
                net
            );
        }

        // Stock sub-$1 boundary: 40 shares @ $0.50 -> exit $0.5002 -> 2dp net_pnl == $0.01 > 0
        let stock_exit = compute_guaranteed_profitable_exit_price(
            d("40"),
            d("0.50"),
            d("40"),
            d("0.45"),
            Decimal::ZERO,
            Decimal::ZERO,
            Decimal::ZERO,
            false,
        )
        .unwrap();
        assert_eq!(stock_exit, d("0.5002"));
        let (stock_net, _, _, _) = compute_net_trade_pnl(
            d("0.50"),
            stock_exit,
            d("40"),
            d("40"),
            false,
            Decimal::ZERO,
            Decimal::ZERO,
            None,
            false,
        );
        assert!(stock_net >= d("0.01"), "stock sub-$1 net_pnl={}", stock_net);
    }

    #[test]
    fn test_ce13_notional_aware_is_dust_qty() {
        // 0.001 BTC @ $90,000 = $90.00 is NOT dust!
        assert!(!is_dust_qty(d("0.001"), d("90000"), true));
        assert!(!is_dust_qty(d("0.0001"), d("90000"), true));
        // 0.00001 BTC @ $90,000 = $0.90 IS dust (< $1.00)
        assert!(is_dust_qty(d("0.00001"), d("90000"), true));
        // 0.00099 SOL @ $72.50 = $0.07 IS dust (< $1.00)
        assert!(is_dust_qty(d("0.00099"), d("72.50"), true));
        // 0.02 SOL @ $72.50 = $1.45 is NOT dust
        assert!(!is_dust_qty(d("0.02"), d("72.50"), true));
        // Zero/negative always dust; zero ref_price falls back to CRYPTO_DUST_QTY
        assert!(is_dust_qty(Decimal::ZERO, d("90000"), true));
        assert!(is_dust_qty(d("0.001"), Decimal::ZERO, true));
        assert!(!is_dust_qty(d("0.002"), Decimal::ZERO, true));
    }

    #[test]
    fn test_ce61_exact_decimal_no_buy_fee_double_count_high_price_crypto() {
        // BTC $100,000, $10 order at Tier 4 0.08%: diff = 8e-8 <= old 1e-7 epsilon
        let (reported_net, _, _, _) = compute_net_trade_pnl(
            d("100000.00"),
            d("100700.00"),
            d("0.0001"),
            d("0.00009992"),
            true,
            d("0.0008"),
            CRYPTO_TIER1_TAKER_FEE_RATE,
            Some(d("0.03")),
            false,
        );
        // True cash PnL = (0.00009992 * 100700 - 0.03) - 10.00 = +0.031944 -> $0.03
        assert_eq!(reported_net, d("0.03"));

        // $1,000,000 asset, buy_qty = 0.00005, sell_qty = 0.000049925 (diff = 7.5e-8)
        let hi_buy_qty = d("0.00005");
        let hi_sell_qty = floor_qty(hi_buy_qty * (Decimal::ONE - CRYPTO_TIER1_MAKER_FEE_RATE), 9);
        let hi_buy_px = d("1000000.00");
        let hi_exit_px = compute_guaranteed_profitable_exit_price(
            hi_buy_qty,
            hi_buy_px,
            hi_sell_qty,
            hi_buy_px,
            CRYPTO_TIER1_TAKER_FEE_RATE,
            MIN_NET_PROFIT_MARGIN,
            CFEE_CEIL_BUFFER_USD,
            true,
        )
        .unwrap();
        let hi_cfee = ceil_usd_cents(hi_sell_qty * hi_exit_px * CRYPTO_TIER1_TAKER_FEE_RATE);
        let (hi_net, _, _, _) = compute_net_trade_pnl(
            hi_buy_px,
            hi_exit_px,
            hi_buy_qty,
            hi_sell_qty,
            true,
            CRYPTO_TIER1_MAKER_FEE_RATE,
            CRYPTO_TIER1_TAKER_FEE_RATE,
            Some(hi_cfee),
            false,
        );
        assert!(hi_net >= d("0.01"), "hi_net={} must stay >= $0.01", hi_net);
    }
}
