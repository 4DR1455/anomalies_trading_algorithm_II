pub mod kalman;
pub mod pricing;
pub mod sizing;

pub use kalman::{AdaptiveKalmanFilter, KalmanState, KalmanUpdate, SignalType};
pub use pricing::{
    calculate_hidden_mm_buy_price, ceil_to_tick, ceil_usd_cents,
    compute_guaranteed_profitable_exit_price, compute_net_trade_pnl, decimal_from_f64_clean,
    decimal_to_f64, floor_qty, floor_to_tick, format_order_qty, format_tick_price, get_tick_size,
    is_dust_qty, round_to_tick, CryptoFeeTier, FeeTracker, PricingError, ALPACA_CRYPTO_FEE_TIERS,
    CFEE_CEIL_BUFFER_USD, CRYPTO_DUST_QTY, CRYPTO_TIER1_MAKER_FEE_RATE,
    CRYPTO_TIER1_TAKER_FEE_RATE, HALF_CENT_USD, MIN_NET_PROFIT_MARGIN, MIN_ORDER_NOTIONAL_USD,
    MIN_PROTECT_NOTIONAL_USD,
};
pub use sizing::calculate_dynamic_portfolio_shares;
