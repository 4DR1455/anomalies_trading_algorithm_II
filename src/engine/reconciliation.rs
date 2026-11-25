use crate::alpaca::client::{ACTIVITY_PAGE_SIZE, MAX_ACTIVITY_PAGES};
use crate::alpaca::{AlpacaApi, Order, OrderRequest};
use crate::config::StrategyParams;
use crate::model::{
    ceil_to_tick, compute_guaranteed_profitable_exit_price, compute_net_trade_pnl, floor_qty,
    format_order_qty, format_tick_price, get_tick_size, is_dust_qty, FeeTracker, PricingError,
    CFEE_CEIL_BUFFER_USD, CRYPTO_TIER1_TAKER_FEE_RATE, MIN_NET_PROFIT_MARGIN,
};
use crate::state::{HuntStatus, StateStore, TradeRecord};
use crate::telemetry::BrokerTelemetry;
use chrono::{DateTime, Utc};
use rust_decimal::{Decimal, RoundingStrategy};
use std::collections::HashSet;
use std::time::Duration;
use tokio::time::{sleep, Instant};
use tracing::{debug, error, info, warn};
use uuid::Uuid;

pub use crate::telemetry::build_broker_telemetry;

const DEFAULT_MAX_TERMINAL_HUNTS: usize = 50;

/// Configurable polling & settlement timings (production defaults vs deterministic fast tests).
#[derive(Debug, Clone, Copy)]
pub struct ReconciliationTiming {
    pub terminal_wait_cancel: Duration,
    pub terminal_wait_market: Duration,
    pub poll_interval: Duration,
    pub settle_retries: usize,
    pub settle_sleep: Duration,
}

impl Default for ReconciliationTiming {
    fn default() -> Self {
        Self {
            terminal_wait_cancel: Duration::from_secs(8),
            terminal_wait_market: Duration::from_secs(10),
            poll_interval: Duration::from_millis(500),
            settle_retries: 3,
            settle_sleep: Duration::from_millis(400),
        }
    }
}

impl ReconciliationTiming {
    pub fn fast_for_tests() -> Self {
        Self {
            terminal_wait_cancel: Duration::from_millis(50),
            terminal_wait_market: Duration::from_millis(50),
            poll_interval: Duration::from_millis(1),
            settle_retries: 2,
            settle_sleep: Duration::from_millis(0),
        }
    }
}

/// Parses an RFC-3339 / ISO-8601 timestamp string into UTC.
pub fn parse_iso_timestamp(ts: Option<&str>) -> Option<DateTime<Utc>> {
    let raw = ts?.trim();
    if raw.is_empty() {
        return None;
    }
    DateTime::parse_from_rfc3339(raw)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

/// Universal `Completed` Non-Loss Gate:
/// `HuntStatus::Completed` is ONLY emitted for non-adopted exits realizing strictly positive
/// 2dp net P&L (`net_pnl > Decimal::ZERO`, i.e. `>= $0.01`). Adopted exits always close as
/// `AdoptedClosed`; partial/interrupted exits that realize `net_pnl <= 0` classify as
/// `LiquidatedManual`.
pub fn classify_filled_exit_status(adopted_from_broker: bool, net_pnl: Decimal) -> HuntStatus {
    if adopted_from_broker {
        HuntStatus::AdoptedClosed
    } else if net_pnl > Decimal::ZERO {
        HuntStatus::Completed
    } else {
        HuntStatus::LiquidatedManual
    }
}

/// Fetches the exact total USD `CFEE` charged by Alpaca across one or more crypto sell order IDs,
/// paginating via `page_token` up to `MAX_ACTIVITY_PAGES` (5 pages = 500 activities) so even
/// heavily fragmented multi-fill orders (>100 partial fills) achieve penny-exact fee reconciliation.
/// Returns `None` for equities, when no matching `CFEE` activity is found, or if any page fails.
pub async fn fetch_actual_cfee_usd<C: AlpacaApi>(
    client: &C,
    order_ids: &[&str],
    is_crypto: bool,
) -> Option<Decimal> {
    if !is_crypto || order_ids.is_empty() {
        return None;
    }

    let id_set: HashSet<&str> = order_ids
        .iter()
        .copied()
        .filter(|id| !id.is_empty())
        .collect();
    if id_set.is_empty() {
        return None;
    }

    let mut total_fee = Decimal::ZERO;
    let mut matched_any = false;
    let mut seen_activity_ids: HashSet<String> = HashSet::new();
    let mut seen_tokens: HashSet<String> = HashSet::new();
    let mut page_token: Option<String> = None;

    for _page in 0..MAX_ACTIVITY_PAGES {
        match client
            .get_account_activities_page("CFEE", None, page_token.as_deref())
            .await
        {
            Ok(activities) => {
                let page_len = activities.len();
                let next_token = activities
                    .last()
                    .map(|a| a.id.clone())
                    .filter(|id| !id.is_empty());

                for act in activities {
                    if !act.id.is_empty() && !seen_activity_ids.insert(act.id.clone()) {
                        continue;
                    }
                    if !act.activity_type.is_empty()
                        && !act.activity_type.eq_ignore_ascii_case("CFEE")
                    {
                        continue;
                    }
                    let Some(ref oid) = act.order_id else {
                        continue;
                    };
                    if id_set.contains(oid.as_str()) {
                        if let Some(fee_abs) = act.fee_usd_abs() {
                            total_fee += fee_abs;
                            matched_any = true;
                        }
                    }
                }

                if page_len < ACTIVITY_PAGE_SIZE {
                    break;
                }
                match next_token {
                    Some(tok) if seen_tokens.insert(tok.clone()) => {
                        page_token = Some(tok);
                    }
                    _ => break,
                }
            }
            Err(e) => {
                debug!("Could not fetch CFEE activities for {:?}: {}", order_ids, e);
                return None;
            }
        }
    }

    if matched_any && total_fee > Decimal::ZERO {
        Some(total_fee)
    } else {
        None
    }
}

/// Calibrates `FeeTracker` from an observed sell-side `CFEE` USD amount and gross notional.
/// Market sells are always taker; limit sells infer maker vs taker cleanly without confusing
/// Tier 1 maker (0.15%) with Tier 5 taker (0.15%).
pub fn calibrate_fee_tracker_from_sell(
    fee_tracker: &mut FeeTracker,
    fee_usd: Decimal,
    gross_notional_usd: Decimal,
    is_market_order: bool,
) {
    if fee_usd <= Decimal::ZERO || gross_notional_usd <= Decimal::ZERO {
        return;
    }
    let observed = fee_usd / gross_notional_usd;
    let taker_threshold = Decimal::from_parts(165, 0, 0, false, 5); // 0.00165
    let is_maker = !is_market_order && observed <= taker_threshold;
    if let Some(matched) = fee_tracker.calibrate_from_cfee(fee_usd, gross_notional_usd, is_maker) {
        info!(
            "Calibrated crypto fee tier from CFEE (${} on ${}): Tier {} (maker={}, taker={}, worst_sell_floor={})",
            fee_usd,
            gross_notional_usd.round_dp(2),
            matched.tier,
            matched.maker_rate,
            matched.taker_rate,
            fee_tracker.worst_sell_fee_rate()
        );
    }
}

/// Polls `client.get_order(order_id)` until the order reaches a strictly terminal state
/// (`filled`, `canceled`, `expired`, `rejected`, `done_for_day`, `replaced`) or `max_wait` elapses.
/// Prevents cancel-vs-fill race conditions where late fills arrive immediately after `cancel_order`.
pub async fn wait_for_terminal_order<C: AlpacaApi>(
    client: &C,
    order_id: &str,
    max_wait: Duration,
    poll_interval: Duration,
    fallback_order: Option<Order>,
) -> Option<Order> {
    let mut latest = fallback_order;
    if latest.as_ref().is_some_and(Order::is_terminal) {
        return latest;
    }

    let deadline = Instant::now() + max_wait;
    loop {
        match client.get_order(order_id).await {
            Ok(ord) => {
                let done = ord.is_terminal();
                latest = Some(ord);
                if done {
                    return latest;
                }
            }
            Err(e) => {
                warn!("Polling terminal status for order {}: {}", order_id, e);
            }
        }

        let now = Instant::now();
        if now >= deadline {
            return latest;
        }
        if !poll_interval.is_zero() {
            let remaining = deadline.saturating_duration_since(now);
            sleep(poll_interval.min(remaining)).await;
        }
    }
}

/// Determines the exact fee-adjusted, balance-verified quantity to submit on an exit Limit SELL.
///
/// On Alpaca crypto pairs, buy fees (0.15% maker / up to 0.25% taker) are deducted immediately
/// in the base asset. Comparing live `qty_available` against `avail_before` yields the exact
/// credited net quantity (preventing HTTP 403 insufficient balance rejections).
/// If `get_position` is transiently unreachable, falls back conservatively using at least
/// `CRYPTO_TIER1_TAKER_FEE_RATE` (`0.0025`, worst-case buy fee), NEVER `maker_fee_rate()`.
///
/// Returns `(safe_sell_qty, live_qty_available)` where `live_qty_available == 0` signals
/// a fallback estimate (so callers do not miscalibrate `FeeTracker` from synthetic estimates).
pub async fn compute_safe_exit_sell_qty<C: AlpacaApi>(
    filled_qty: Decimal,
    symbol: &str,
    client: &C,
    avail_before: Option<Decimal>,
    worst_buy_fee_rate: Decimal,
    settle_retries: usize,
    settle_sleep: Duration,
) -> (Decimal, Decimal) {
    let is_crypto = symbol.contains('/');
    let decimals: u32 = if is_crypto { 9 } else { 4 };

    if filled_qty <= Decimal::ZERO {
        return (Decimal::ZERO, Decimal::ZERO);
    }

    let conservative_buy_fee = worst_buy_fee_rate.max(CRYPTO_TIER1_TAKER_FEE_RATE);
    let est_net = if is_crypto {
        floor_qty(filled_qty * (Decimal::ONE - conservative_buy_fee), decimals)
    } else {
        floor_qty(filled_qty, decimals)
    };

    let eps = Decimal::from_parts(1, 0, 0, false, 8); // 1e-8
    let attempts = settle_retries.max(1);
    let mut live_avail: Option<Decimal> = None;

    for attempt in 0..attempts {
        match client.get_position(symbol).await {
            Ok(Some(pos)) if pos.qty_available > Decimal::ZERO => {
                let avail = floor_qty(pos.qty_available, decimals);
                live_avail = Some(avail);
                let reflected = match avail_before {
                    None => true,
                    Some(before) => avail > before + eps,
                };
                if reflected || attempt + 1 == attempts {
                    break;
                }
            }
            Ok(None) if attempt + 1 < attempts => {
                // Position may still be settling immediately after a fill
            }
            Ok(_) => {
                live_avail = Some(Decimal::ZERO);
                break;
            }
            Err(e) => {
                warn!(
                    "Fetching live position qty_available for {} (attempt {}/{}): {}",
                    symbol,
                    attempt + 1,
                    attempts,
                    e
                );
            }
        }

        if attempt + 1 < attempts && !settle_sleep.is_zero() {
            sleep(settle_sleep).await;
        }
    }

    let Some(live_avail) = live_avail else {
        // Position endpoint unreachable: fall back to conservative worst-case (0.25%) estimate
        return (est_net, Decimal::ZERO);
    };

    if !is_crypto {
        return (floor_qty(filled_qty.min(live_avail), decimals), live_avail);
    }

    if let Some(before) = avail_before {
        let delta = floor_qty(live_avail - before, decimals);
        if delta > Decimal::ZERO && delta <= filled_qty {
            let safe_qty = floor_qty(delta.min(live_avail), decimals);
            return (safe_qty, live_avail);
        }
    }

    let safe_qty = floor_qty(est_net.min(live_avail), decimals);
    (safe_qty, live_avail)
}

/// Submits an exit Limit SELL order with automatic HTTP 403/422 balance reconciliation retry.
///
/// Safety invariants (CE-1.2 fix):
/// - Whenever a balance rejection shrinks `current_qty`, enforces the 2% drastic-shrink guard:
///   if `current_qty < buy_qty * 0.98`, aborts retry and returns `(None, Decimal::ZERO, current_exit_price)`
///   so the hunt safely remains `ExitSubmitPending` for clean next-cycle reconciliation instead of
///   massively inflating the exit price or selling partial shares at a loss.
/// - Recomputes `current_exit_price` dynamically via `exit_price_fn(current_qty)` on EVERY retry
///   so reduced quantities ALWAYS clear worst-case round-trip fees and realize strict net profit.
///
/// Returns `(Option<Order>, submitted_qty, actual_exit_price)`.
#[allow(clippy::too_many_arguments)]
pub async fn submit_exit_sell_with_retry<C, F>(
    client: &C,
    symbol: &str,
    buy_qty: Decimal,
    sell_qty: Decimal,
    initial_exit_price: Decimal,
    tif: &str,
    max_retries: usize,
    exit_price_fn: F,
) -> (Option<Order>, Decimal, Decimal)
where
    C: AlpacaApi,
    F: Fn(Decimal) -> Result<Decimal, PricingError>,
{
    let is_crypto = symbol.contains('/');
    let decimals: u32 = if is_crypto { 9 } else { 4 };
    let step = Decimal::new(1, decimals);
    let min_retry_qty = buy_qty * Decimal::new(98, 2); // 98% of buy_qty safety floor
    let mut current_qty = floor_qty(sell_qty, decimals);
    let mut current_exit_price = initial_exit_price;

    for attempt in 0..=max_retries {
        if current_qty <= Decimal::ZERO {
            return (None, Decimal::ZERO, current_exit_price);
        }

        let qty_str = format_order_qty(current_qty, is_crypto);
        if qty_str == "0" {
            return (None, Decimal::ZERO, current_exit_price);
        }

        let exit_price_str = format_tick_price(current_exit_price);
        let req = OrderRequest {
            symbol: symbol.to_string(),
            qty: qty_str.clone(),
            side: "sell".to_string(),
            order_type: "limit".to_string(),
            time_in_force: tif.to_string(),
            limit_price: Some(exit_price_str.clone()),
            client_order_id: Some(format!("hunt-sell-{}", Uuid::new_v4())),
            extended_hours: None,
        };

        match client.submit_order(&req).await {
            Ok(ord) if !ord.id.is_empty() => return (Some(ord), current_qty, current_exit_price),
            Ok(ord) => {
                warn!(
                    "[EXIT REJECTED] Order response missing id on attempt {} for {} {}: {:?}",
                    attempt + 1,
                    qty_str,
                    symbol,
                    ord
                );
                if attempt >= max_retries {
                    return (None, Decimal::ZERO, current_exit_price);
                }
            }
            Err(e) => {
                warn!(
                    "[EXIT ERROR] Attempt {} failed for {} {} @ ${}: {}",
                    attempt + 1,
                    qty_str,
                    symbol,
                    exit_price_str,
                    e
                );
                if e.is_balance_or_unprocessable() && attempt < max_retries {
                    let mut candidates: Vec<Decimal> = Vec::with_capacity(2);
                    if let Some(avail) = e.parsed_available_qty() {
                        candidates.push(floor_qty(avail, decimals));
                    }
                    match client.get_position(symbol).await {
                        Ok(Some(pos)) => {
                            candidates.push(floor_qty(pos.qty_available, decimals));
                        }
                        Ok(None) => {
                            candidates.push(Decimal::ZERO);
                        }
                        Err(pe) => {
                            warn!("Refreshing position after exit balance rejection: {}", pe);
                        }
                    }

                    if let Some(mut fresh_avail) = candidates.into_iter().min() {
                        if fresh_avail >= current_qty && is_crypto {
                            fresh_avail =
                                floor_qty((fresh_avail - step).max(Decimal::ZERO), decimals);
                        }
                        current_qty = floor_qty(current_qty.min(fresh_avail), decimals);
                        if current_qty <= Decimal::ZERO {
                            return (None, Decimal::ZERO, current_exit_price);
                        }
                        if buy_qty > Decimal::ZERO && current_qty < min_retry_qty {
                            warn!(
                                "[EXIT RETRY ABORT] Shrunk qty {} for {} fell >2% below buy_qty {} (min_retry_qty={}); deferring as ExitSubmitPending.",
                                format_order_qty(current_qty, is_crypto),
                                symbol,
                                format_order_qty(buy_qty, is_crypto),
                                format_order_qty(min_retry_qty, is_crypto),
                            );
                            return (None, Decimal::ZERO, current_exit_price);
                        }
                        match exit_price_fn(current_qty) {
                            Ok(recomputed_px) if recomputed_px > Decimal::ZERO => {
                                let new_px = recomputed_px.max(current_exit_price);
                                info!(
                                    "[EXIT RETRY] Adjusting exit SELL for {}: qty -> {}, exit_px ${} -> ${}",
                                    symbol,
                                    format_order_qty(current_qty, is_crypto),
                                    format_tick_price(current_exit_price),
                                    format_tick_price(new_px),
                                );
                                current_exit_price = new_px;
                                continue;
                            }
                            Ok(_) | Err(_) => {
                                warn!(
                                    "[EXIT RETRY ABORT] Failed to recompute guaranteed-profitable exit price for {} qty {}; deferring as ExitSubmitPending.",
                                    symbol,
                                    format_order_qty(current_qty, is_crypto),
                                );
                                return (None, Decimal::ZERO, current_exit_price);
                            }
                        }
                    }
                    return (None, Decimal::ZERO, current_exit_price);
                }
                return (None, Decimal::ZERO, current_exit_price);
            }
        }
    }

    (None, Decimal::ZERO, current_exit_price)
}

/// Fallibly computes a guaranteed-profitable exit limit price clearing round-trip worst-case fees,
/// preserving any higher pre-existing target price on the hunt record.
#[allow(clippy::too_many_arguments)]
pub fn try_resolve_profitable_exit_price(
    buy_qty: Decimal,
    buy_px: Decimal,
    sell_qty: Decimal,
    fair_price: Decimal,
    existing_target: Option<Decimal>,
    fee_tracker: &FeeTracker,
    is_crypto: bool,
) -> Result<Decimal, PricingError> {
    let guaranteed = compute_guaranteed_profitable_exit_price(
        buy_qty,
        buy_px,
        sell_qty,
        fair_price,
        fee_tracker.worst_sell_fee_rate(),
        MIN_NET_PROFIT_MARGIN,
        CFEE_CEIL_BUFFER_USD,
        is_crypto,
    )?;

    Ok(match existing_target {
        Some(prev) if prev > guaranteed => ceil_to_tick(prev),
        _ => guaranteed,
    })
}

/// Computes a safe exit limit price guaranteed to clear round-trip fees, preferring any higher
/// pre-existing target price on the hunt record.
#[allow(clippy::too_many_arguments)]
fn resolve_profitable_exit_price(
    buy_qty: Decimal,
    buy_px: Decimal,
    sell_qty: Decimal,
    fair_price: Decimal,
    existing_target: Option<Decimal>,
    fallback_ref_price: Decimal,
    fee_tracker: &FeeTracker,
    is_crypto: bool,
) -> Decimal {
    try_resolve_profitable_exit_price(
        buy_qty,
        buy_px,
        sell_qty,
        fair_price,
        existing_target,
        fee_tracker,
        is_crypto,
    )
    .unwrap_or_else(|_| {
        let min_tick = ceil_to_tick(buy_px + get_tick_size(buy_px));
        let fallback = if fair_price > Decimal::ZERO {
            min_tick.max(ceil_to_tick(fair_price))
        } else if fallback_ref_price > Decimal::ZERO {
            min_tick.max(ceil_to_tick(
                fallback_ref_price + get_tick_size(fallback_ref_price),
            ))
        } else {
            min_tick
        };
        match existing_target {
            Some(prev) if prev > fallback => ceil_to_tick(prev),
            _ => fallback,
        }
    })
}

/// Finalizes a filled exit sell on `hunt` with exact `CFEE` lookup (falling back to `FeeTracker`)
/// and enforces the Universal `Completed` Non-Loss Gate.
#[allow(clippy::too_many_arguments)]
async fn finalize_filled_exit<C: AlpacaApi>(
    client: &C,
    hunt: &mut TradeRecord,
    sell_order: &Order,
    fallback_exit_px: Decimal,
    fallback_qty: Decimal,
    fee_tracker: &mut FeeTracker,
    is_crypto: bool,
    now_str: &str,
) {
    let buy_px = hunt.buy_price;
    let buy_qty = if hunt.buy_qty > Decimal::ZERO {
        hunt.buy_qty
    } else {
        hunt.qty
    };
    let fill_px = sell_order
        .filled_avg_price
        .filter(|p| *p > Decimal::ZERO)
        .or(hunt.sell_price)
        .unwrap_or(fallback_exit_px);
    let fill_qty = if sell_order.filled_qty > Decimal::ZERO {
        sell_order.filled_qty
    } else {
        fallback_qty
    };
    let fill_time = sell_order
        .filled_at
        .clone()
        .or_else(|| sell_order.created_at.clone())
        .unwrap_or_else(|| now_str.to_string());

    let is_market = sell_order.order_type == "market";
    let actual_cfee = fetch_actual_cfee_usd(client, &[sell_order.id.as_str()], is_crypto).await;
    if let Some(fee) = actual_cfee {
        calibrate_fee_tracker_from_sell(fee_tracker, fee, fill_px * fill_qty, is_market);
    }

    let fallback_sell_rate = if is_market {
        fee_tracker.taker_fee_rate()
    } else {
        fee_tracker.maker_fee_rate()
    };

    let (net_pnl, pnl_pct, gross_pnl, fees_usd) = compute_net_trade_pnl(
        buy_px,
        fill_px,
        buy_qty,
        fill_qty,
        is_crypto,
        fee_tracker.maker_fee_rate(),
        fallback_sell_rate,
        actual_cfee,
        hunt.buy_fee_already_deducted.unwrap_or(false),
    );

    hunt.sell_price = Some(fill_px);
    hunt.qty = fill_qty;
    hunt.filled_exit_qty = fill_qty;
    hunt.sell_timestamp = Some(fill_time);
    hunt.pnl = Some(net_pnl);
    hunt.pnl_pct = Some(pnl_pct);
    hunt.gross_pnl = Some(gross_pnl);
    hunt.fees_usd = Some(fees_usd);
    hunt.status = classify_filled_exit_status(hunt.adopted_from_broker, net_pnl);

    if !hunt.adopted_from_broker && net_pnl <= Decimal::ZERO {
        warn!(
            "[EXIT NON-PROFITABLE CLOSURE] Hunt {} classified as {} instead of Completed (Net PnL: ${}, fill_qty={}, buy_qty={})",
            hunt.id,
            hunt.status,
            net_pnl,
            format_order_qty(fill_qty, is_crypto),
            format_order_qty(buy_qty, is_crypto),
        );
    }

    info!(
        "[EXIT FILLED] Hunt {} ({}): Sold {} @ ${} (Net PnL: ${}, Gross: ${}, Fees: ${}, CFEE exact: {})",
        hunt.id,
        hunt.status,
        format_order_qty(fill_qty, is_crypto),
        format_tick_price(fill_px),
        net_pnl,
        gross_pnl,
        fees_usd,
        actual_cfee.is_some()
    );
}

/// Per-cycle Broker Reconciliation & Order Hygiene Engine:
/// - **Step A (`exit_submit_pending`)**: retries protective limit sells with guaranteed fee-clearing pricing.
/// - **Step B (`open_exit_pending` / `exit_partially_filled`)**:
///   - Finalizes filled exits using exact Alpaca `CFEE` (with `FeeTracker` fallback), setting `AdoptedClosed`
///     if adopted from broker else `Completed`.
///   - Safely handles canceled/rejected/expired exits (deferring on position network failure, never falsely closing).
///   - Enforces `stale_order_max_age_seconds` via cancel -> wait terminal -> market liquidation -> wait terminal,
///     recording BOTH order IDs in `cycle_closed_order_ids`.
/// - **Step C (Hardened Adoption Gate)**: adopts untracked broker open sell orders ONLY after verifying all 6
///   safety gates (`side == "sell"`, `order_type == "limit"`, `limit_price > 0`, not in `all_known_sell_ids`,
///   authoritative `get_order` returns OPEN, and `position.qty > CRYPTO_DUST_QTY`).
/// - **Step D (Orphan Auto-Protection)**: protects unencumbered `qty_available > CRYPTO_DUST_QTY` (`>= $1.00`)
///   by reconstructing conservative gross cost basis, pricing strictly above round-trip break-even, and persisting
///   `exit_submit_pending` BEFORE order submission.
#[allow(clippy::too_many_arguments)]
pub async fn reconcile_and_maintain_positions<C: AlpacaApi>(
    client: &C,
    params: &StrategyParams,
    trades_history: &mut Vec<TradeRecord>,
    fee_tracker: &mut FeeTracker,
    fair_price: Decimal,
    bid: Decimal,
    now_dt: DateTime<Utc>,
    store: Option<&StateStore>,
    timing: ReconciliationTiming,
) -> (bool, BrokerTelemetry) {
    let symbol = params.symbol.as_str();
    let is_crypto = params.is_crypto();
    let decimals: u32 = if is_crypto { 9 } else { 4 };
    let limit_tif = params.limit_tif();
    let market_tif = params.market_tif();
    let stale_max_age = params.stale_order_max_age_seconds as i64;
    let now_str = now_dt.to_rfc3339();

    let mut history_changed = false;
    let mut deferred_exit_recovery = false;
    let mut cycle_closed_order_ids: HashSet<String> = HashSet::new();

    // Initial broker snapshot
    let account = match client.get_account().await {
        Ok(acc) => Some(acc),
        Err(e) => {
            warn!("Reconciliation get_account failed: {}", e);
            None
        }
    };

    let (mut position, mut position_fetch_ok) = match client.get_position(symbol).await {
        Ok(pos) => (pos, true),
        Err(e) => {
            warn!("Reconciliation get_position({}) failed: {}", symbol, e);
            (None, false)
        }
    };

    let (mut open_orders, mut open_orders_fetch_ok) =
        match client.get_open_orders(Some(symbol)).await {
            Ok(orders) => (orders, true),
            Err(e) => {
                warn!("Reconciliation get_open_orders({}) failed: {}", symbol, e);
                (Vec::new(), false)
            }
        };

    let mut avail_pool = position
        .as_ref()
        .map(|p| floor_qty(p.qty_available, decimals))
        .unwrap_or(Decimal::ZERO);

    // =========================================================================
    // STEP A: Retry hunts stuck in `ExitSubmitPending`
    // =========================================================================
    for hunt in trades_history.iter_mut() {
        if hunt.status != HuntStatus::ExitSubmitPending {
            continue;
        }
        if !position_fetch_ok {
            continue;
        }

        let desired_qty = floor_qty(hunt.qty, decimals);
        let sell_qty = floor_qty(desired_qty.min(avail_pool), decimals);
        let ref_price = if bid > Decimal::ZERO {
            bid
        } else {
            position
                .as_ref()
                .and_then(|p| p.current_price.filter(|c| *c > Decimal::ZERO))
                .or_else(|| {
                    position
                        .as_ref()
                        .map(|p| p.avg_entry_price)
                        .filter(|p| *p > Decimal::ZERO)
                })
                .or_else(|| (hunt.buy_price > Decimal::ZERO).then_some(hunt.buy_price))
                .unwrap_or(Decimal::ZERO)
        };
        let pos_total_qty = position
            .as_ref()
            .map(|p| floor_qty(p.qty, decimals))
            .unwrap_or(Decimal::ZERO);

        if is_dust_qty(sell_qty, ref_price, is_crypto) {
            let has_open_sell = open_orders.iter().any(|o| o.side == "sell");
            if is_dust_qty(avail_pool, ref_price, is_crypto)
                && is_dust_qty(pos_total_qty, ref_price, is_crypto)
                && open_orders_fetch_ok
                && !has_open_sell
            {
                warn!(
                    "[EXIT RECOVERY] Hunt {} was exit_submit_pending with 0 {} remaining at broker; closing record.",
                    hunt.id, symbol
                );
                hunt.status = if hunt.adopted_from_broker {
                    HuntStatus::AdoptedClosed
                } else {
                    HuntStatus::LiquidatedManual
                };
                hunt.sell_timestamp = Some(now_str.clone());
                hunt.pnl = Some(Decimal::ZERO);
                hunt.pnl_pct = Some(Decimal::ZERO);
                history_changed = true;
            }
            continue;
        }

        let buy_px = if hunt.buy_price > Decimal::ZERO {
            hunt.buy_price
        } else {
            ref_price
        };
        let buy_qty_eff = if hunt.buy_qty > Decimal::ZERO {
            hunt.buy_qty
        } else {
            sell_qty
        };
        let existing_target = hunt.sell_price;
        let exit_px = resolve_profitable_exit_price(
            buy_qty_eff,
            buy_px,
            sell_qty,
            fair_price,
            existing_target,
            ref_price,
            fee_tracker,
            is_crypto,
        );

        info!(
            "[EXIT RECOVERY] Submitting pending Limit SELL for Hunt {}: {} @ ${}",
            hunt.id,
            format_order_qty(sell_qty, is_crypto),
            format_tick_price(exit_px)
        );
        let (sell_ord_opt, submitted_qty, actual_exit_px) = submit_exit_sell_with_retry(
            client,
            symbol,
            buy_qty_eff,
            sell_qty,
            exit_px,
            limit_tif,
            2,
            |adj_qty| {
                try_resolve_profitable_exit_price(
                    buy_qty_eff,
                    buy_px,
                    adj_qty,
                    fair_price,
                    existing_target,
                    fee_tracker,
                    is_crypto,
                )
            },
        )
        .await;

        if let Some(sell_ord) = sell_ord_opt {
            hunt.sell_order_id = Some(sell_ord.id.clone());
            hunt.qty = submitted_qty;
            hunt.sell_price = Some(actual_exit_px);

            if sell_ord.is_filled() {
                cycle_closed_order_ids.insert(sell_ord.id.clone());
                finalize_filled_exit(
                    client,
                    hunt,
                    &sell_ord,
                    actual_exit_px,
                    submitted_qty,
                    fee_tracker,
                    is_crypto,
                    &now_str,
                )
                .await;
            } else {
                hunt.filled_exit_qty = Decimal::ZERO;
                hunt.status = HuntStatus::OpenExitPending;
            }

            avail_pool = floor_qty((avail_pool - submitted_qty).max(Decimal::ZERO), decimals);
            history_changed = true;
        }
    }

    // =========================================================================
    // STEP B: Monitor `OpenExitPending` / `ExitPartiallyFilled` + 12h Stale Liq
    // =========================================================================
    for hunt in trades_history.iter_mut() {
        match hunt.status {
            HuntStatus::OpenExitPending | HuntStatus::ExitPartiallyFilled => {}
            HuntStatus::ExitSubmitPending
            | HuntStatus::Completed
            | HuntStatus::LiquidatedStale
            | HuntStatus::LiquidatedManual
            | HuntStatus::LiquidatedOrphan
            | HuntStatus::AdoptedClosed => continue,
        }

        let Some(sell_oid) = hunt.sell_order_id.clone().filter(|id| !id.is_empty()) else {
            hunt.status = HuntStatus::ExitSubmitPending;
            history_changed = true;
            continue;
        };

        let chk_sell = match client.get_order(&sell_oid).await {
            Ok(ord) => ord,
            Err(e) => {
                warn!("Polling pending exit order {}: {} (deferring Step D orphan scan)", sell_oid, e);
                deferred_exit_recovery = true;
                continue;
            }
        };

        let buy_px = hunt.buy_price;
        let buy_qty = if hunt.buy_qty > Decimal::ZERO {
            hunt.buy_qty
        } else {
            hunt.qty
        };
        let buy_fee_deducted = hunt.buy_fee_already_deducted.unwrap_or(false);

        if chk_sell.is_filled() {
            cycle_closed_order_ids.insert(sell_oid.clone());
            let fallback_px = hunt.sell_price.unwrap_or(buy_px);
            let fallback_qty = hunt.qty;
            finalize_filled_exit(
                client,
                hunt,
                &chk_sell,
                fallback_px,
                fallback_qty,
                fee_tracker,
                is_crypto,
                &now_str,
            )
            .await;
            history_changed = true;
        } else if chk_sell.is_canceled_or_failed() || chk_sell.status == "done_for_day" {
            let filled_so_far = chk_sell.filled_qty;
            let order_qty = if chk_sell.qty > Decimal::ZERO {
                chk_sell.qty
            } else {
                hunt.qty
            };
            let rem_qty = floor_qty((order_qty - filled_so_far).max(Decimal::ZERO), decimals);
            let step_b_ref_px = if bid > Decimal::ZERO {
                bid
            } else {
                chk_sell
                    .filled_avg_price
                    .filter(|p| *p > Decimal::ZERO)
                    .or(chk_sell.limit_price.filter(|p| *p > Decimal::ZERO))
                    .or(hunt.sell_price.filter(|p| *p > Decimal::ZERO))
                    .unwrap_or(buy_px)
            };

            warn!(
                "[EXIT {}] Hunt {} order {} ended as {} (filled={}, remaining={}). Evaluating recovery...",
                chk_sell.status.to_uppercase(),
                hunt.id,
                sell_oid,
                chk_sell.status,
                filled_so_far,
                rem_qty
            );

            // CRITICAL INVARIANT: Never close an unverified partial/canceled exit during a network outage!
            if !is_dust_qty(rem_qty, step_b_ref_px, is_crypto) && !position_fetch_ok {
                warn!(
                    "[EXIT DEFER] Hunt {} order {} has remaining {} (non-dust) but broker position fetch failed; deferring to next cycle.",
                    hunt.id, sell_oid, rem_qty
                );
                deferred_exit_recovery = true;
                continue;
            }

            cycle_closed_order_ids.insert(sell_oid.clone());

            if !is_dust_qty(rem_qty, step_b_ref_px, is_crypto) && position_fetch_ok {
                // CE-5.2 fix: NEVER fall back to stale pre-cancel avail_pool on mid-cycle Err!
                let fresh_avail = match client.get_position(symbol).await {
                    Ok(Some(pos)) => floor_qty(pos.qty_available, decimals),
                    Ok(None) => Decimal::ZERO,
                    Err(e) => {
                        warn!(
                            "[EXIT DEFER] Re-querying position for Hunt {} during canceled-exit recovery failed ({}); deferring to next cycle.",
                            hunt.id, e
                        );
                        deferred_exit_recovery = true;
                        continue;
                    }
                };

                let re_qty = floor_qty(rem_qty.min(fresh_avail), decimals);
                if !is_dust_qty(re_qty, step_b_ref_px, is_crypto) {
                    let rem_buy_qty = if filled_so_far > Decimal::ZERO && order_qty > Decimal::ZERO
                    {
                        floor_qty((buy_qty * (re_qty / order_qty)).max(re_qty), decimals)
                    } else if filled_so_far > Decimal::ZERO {
                        floor_qty((buy_qty - filled_so_far).max(re_qty), decimals)
                    } else {
                        buy_qty
                    };

                    let existing_target = hunt.sell_price;
                    let exit_px = resolve_profitable_exit_price(
                        rem_buy_qty,
                        buy_px,
                        re_qty,
                        fair_price,
                        existing_target,
                        bid,
                        fee_tracker,
                        is_crypto,
                    );

                    let (sell_ord_opt, submitted_qty, actual_exit_px) =
                        submit_exit_sell_with_retry(
                            client,
                            symbol,
                            rem_buy_qty,
                            re_qty,
                            exit_px,
                            limit_tif,
                            2,
                            |adj_qty| {
                                try_resolve_profitable_exit_price(
                                    rem_buy_qty,
                                    buy_px,
                                    adj_qty,
                                    fair_price,
                                    existing_target,
                                    fee_tracker,
                                    is_crypto,
                                )
                            },
                        )
                        .await;

                    if let Some(sell_ord) = sell_ord_opt {
                        if filled_so_far > Decimal::ZERO {
                            hunt.buy_qty = rem_buy_qty.max(submitted_qty);
                        }
                        hunt.sell_order_id = Some(sell_ord.id.clone());
                        hunt.qty = submitted_qty;
                        hunt.sell_price = Some(actual_exit_px);
                        hunt.filled_exit_qty = Decimal::ZERO;

                        if sell_ord.is_filled() {
                            cycle_closed_order_ids.insert(sell_ord.id.clone());
                            finalize_filled_exit(
                                client,
                                hunt,
                                &sell_ord,
                                actual_exit_px,
                                submitted_qty,
                                fee_tracker,
                                is_crypto,
                                &now_str,
                            )
                            .await;
                        } else {
                            hunt.status = HuntStatus::OpenExitPending;
                            info!(
                                "[EXIT RE-RAISED] Hunt {}: New SELL {} @ ${} ({})",
                                hunt.id,
                                format_order_qty(submitted_qty, is_crypto),
                                format_tick_price(actual_exit_px),
                                sell_ord.id
                            );
                        }
                        avail_pool =
                            floor_qty((avail_pool - submitted_qty).max(Decimal::ZERO), decimals);
                        history_changed = true;
                    } else {
                        if filled_so_far > Decimal::ZERO {
                            hunt.buy_qty = rem_buy_qty.max(re_qty);
                        }
                        hunt.sell_order_id = None;
                        hunt.qty = re_qty;
                        hunt.sell_price = Some(exit_px);
                        hunt.filled_exit_qty = Decimal::ZERO;
                        hunt.status = HuntStatus::ExitSubmitPending;
                        history_changed = true;
                    }
                } else {
                    // Shares genuinely no longer exist at broker (liquidated externally/manually)
                    let fill_px = chk_sell
                        .filled_avg_price
                        .filter(|p| *p > Decimal::ZERO)
                        .unwrap_or_else(|| if bid > Decimal::ZERO { bid } else { buy_px });
                    let sold_qty = if filled_so_far > Decimal::ZERO {
                        filled_so_far
                    } else {
                        buy_qty
                    };
                    let actual_cfee = if filled_so_far > Decimal::ZERO {
                        fetch_actual_cfee_usd(client, &[sell_oid.as_str()], is_crypto).await
                    } else {
                        None
                    };
                    let (net_pnl, pnl_pct, gross_pnl, fees_usd) = compute_net_trade_pnl(
                        buy_px,
                        fill_px,
                        buy_qty,
                        sold_qty,
                        is_crypto,
                        fee_tracker.maker_fee_rate(),
                        fee_tracker.maker_fee_rate(),
                        actual_cfee,
                        buy_fee_deducted,
                    );
                    hunt.sell_price = Some(fill_px);
                    hunt.sell_timestamp = Some(
                        chk_sell
                            .canceled_at
                            .clone()
                            .unwrap_or_else(|| now_str.clone()),
                    );
                    hunt.pnl = Some(net_pnl);
                    hunt.pnl_pct = Some(pnl_pct);
                    hunt.gross_pnl = Some(gross_pnl);
                    hunt.fees_usd = Some(fees_usd);
                    hunt.status = if hunt.adopted_from_broker {
                        HuntStatus::AdoptedClosed
                    } else {
                        HuntStatus::LiquidatedManual
                    };
                    history_changed = true;
                }
            } else {
                // Remainder is un-sellable notional dust (< $1.00): finalize based on realized fills
                // and enforce the Universal `Completed` Non-Loss Gate (CE-1.3a fix)
                let fill_px = chk_sell
                    .filled_avg_price
                    .filter(|p| *p > Decimal::ZERO)
                    .or(hunt.sell_price)
                    .unwrap_or(buy_px);
                let sold_qty = if filled_so_far > Decimal::ZERO {
                    filled_so_far
                } else {
                    buy_qty
                };
                let actual_cfee = if filled_so_far > Decimal::ZERO {
                    fetch_actual_cfee_usd(client, &[sell_oid.as_str()], is_crypto).await
                } else {
                    None
                };
                let (net_pnl, pnl_pct, gross_pnl, fees_usd) = compute_net_trade_pnl(
                    buy_px,
                    fill_px,
                    buy_qty,
                    sold_qty,
                    is_crypto,
                    fee_tracker.maker_fee_rate(),
                    fee_tracker.maker_fee_rate(),
                    actual_cfee,
                    buy_fee_deducted,
                );
                hunt.sell_price = Some(fill_px);
                hunt.qty = sold_qty;
                hunt.filled_exit_qty = sold_qty;
                hunt.sell_timestamp = Some(
                    chk_sell
                        .filled_at
                        .clone()
                        .or_else(|| chk_sell.canceled_at.clone())
                        .unwrap_or_else(|| now_str.clone()),
                );
                hunt.pnl = Some(net_pnl);
                hunt.pnl_pct = Some(pnl_pct);
                hunt.gross_pnl = Some(gross_pnl);
                hunt.fees_usd = Some(fees_usd);
                hunt.status = if hunt.adopted_from_broker {
                    HuntStatus::AdoptedClosed
                } else if filled_so_far > Decimal::ZERO && net_pnl > Decimal::ZERO {
                    HuntStatus::Completed
                } else {
                    HuntStatus::LiquidatedManual
                };
                history_changed = true;
            }
        } else if chk_sell.is_open() {
            // Track partial exit fills
            if chk_sell.status == "partially_filled" {
                let partial_qty = chk_sell.filled_qty;
                let eps = Decimal::from_parts(1, 0, 0, false, 8);
                if hunt.status != HuntStatus::ExitPartiallyFilled
                    || (hunt.filled_exit_qty - partial_qty).abs() > eps
                {
                    hunt.filled_exit_qty = partial_qty;
                    hunt.status = HuntStatus::ExitPartiallyFilled;
                    history_changed = true;
                }
            }

            // Enforce 12-hour Stale Order Liquidation timeout
            let created_dt = parse_iso_timestamp(chk_sell.created_at.as_deref())
                .or_else(|| parse_iso_timestamp(Some(&hunt.buy_timestamp)));

            if let Some(created) = created_dt {
                let age_secs = (now_dt - created).num_seconds();
                if stale_max_age > 0 && age_secs >= stale_max_age {
                    info!(
                        "[STALE LIQUIDATION] Hunt {} exit order {} age {:.2}h >= {:.2}h. Canceling & liquidating at MARKET...",
                        hunt.id,
                        sell_oid,
                        (age_secs as f64) / 3600.0,
                        (stale_max_age as f64) / 3600.0
                    );

                    if let Err(ce) = client.cancel_order(&sell_oid).await {
                        warn!("Failed canceling stale order {}: {}", sell_oid, ce);
                        continue;
                    }

                    let term_sell = wait_for_terminal_order(
                        client,
                        &sell_oid,
                        timing.terminal_wait_cancel,
                        timing.poll_interval,
                        Some(chk_sell.clone()),
                    )
                    .await
                    .unwrap_or_else(|| chk_sell.clone());

                    // CE-5.1 fix: NEVER proceed to check fresh_avail or close hunt while cancel is still non-terminal!
                    if !term_sell.is_terminal() {
                        warn!(
                            "[STALE DEFER] Hunt {} exit order {} still non-terminal ('{}') after cancel wait; deferring stale liquidation to next cycle.",
                            hunt.id, sell_oid, term_sell.status
                        );
                        deferred_exit_recovery = true;
                        continue;
                    }

                    // Always record the terminal canceled stale limit order ID to block stale-index re-adoption
                    cycle_closed_order_ids.insert(sell_oid.clone());

                    let partial_qty = term_sell.filled_qty;
                    let partial_px = term_sell
                        .filled_avg_price
                        .filter(|p| *p > Decimal::ZERO)
                        .or(hunt.sell_price)
                        .unwrap_or(Decimal::ZERO);
                    let order_qty = if term_sell.qty > Decimal::ZERO {
                        term_sell.qty
                    } else {
                        hunt.qty
                    };
                    let rem_to_liq =
                        floor_qty((order_qty - partial_qty).max(Decimal::ZERO), decimals);
                    let stale_ref_px = if partial_px > Decimal::ZERO {
                        partial_px
                    } else if bid > Decimal::ZERO {
                        bid
                    } else {
                        buy_px
                    };

                    // CE-1.3b fix: NEVER pass a 0-fill canceled order into finalize_filled_exit;
                    // use notional-aware is_dust_qty so 0.001 BTC ($90) is liquidated, NOT treated as dust!
                    if term_sell.is_filled()
                        || (partial_qty > Decimal::ZERO
                            && is_dust_qty(rem_to_liq, stale_ref_px, is_crypto))
                    {
                        // Filled right before cancel completed (or filled down to un-sellable dust)
                        let fallback_px = if partial_px > Decimal::ZERO {
                            partial_px
                        } else {
                            buy_px
                        };
                        let fallback_qty = if partial_qty > Decimal::ZERO {
                            partial_qty
                        } else {
                            order_qty
                        };
                        finalize_filled_exit(
                            client,
                            hunt,
                            &term_sell,
                            fallback_px,
                            fallback_qty,
                            fee_tracker,
                            is_crypto,
                            &now_str,
                        )
                        .await;
                        history_changed = true;
                        continue;
                    }

                    // Query fresh available balance after cancel releases locked shares
                    let fresh_avail = match client.get_position(symbol).await {
                        Ok(Some(pos)) => floor_qty(pos.qty_available, decimals),
                        Ok(None) => Decimal::ZERO,
                        Err(pe) => {
                            warn!("Fetching position for stale liquidation failed: {}", pe);
                            rem_to_liq
                        }
                    };

                    let liq_qty = floor_qty(rem_to_liq.min(fresh_avail), decimals);
                    if !is_dust_qty(liq_qty, stale_ref_px, is_crypto) {
                        let mkt_req = OrderRequest {
                            symbol: symbol.to_string(),
                            qty: format_order_qty(liq_qty, is_crypto),
                            side: "sell".to_string(),
                            order_type: "market".to_string(),
                            time_in_force: market_tif.to_string(),
                            limit_price: None,
                            client_order_id: Some(format!("stale-liq-{}", Uuid::new_v4())),
                            extended_hours: None,
                        };

                        match client.submit_order(&mkt_req).await {
                            Ok(mkt_ord) => {
                                let mkt_id = mkt_ord.id.clone();
                                let mkt_term = if !mkt_id.is_empty() {
                                    wait_for_terminal_order(
                                        client,
                                        &mkt_id,
                                        timing.terminal_wait_market,
                                        timing.poll_interval,
                                        Some(mkt_ord.clone()),
                                    )
                                    .await
                                    .unwrap_or(mkt_ord)
                                } else {
                                    mkt_ord
                                };

                                // CE-5.3 fix (non-terminal): defer cleanly without fabricating a fill
                                if !mkt_term.is_terminal() {
                                    warn!(
                                        "[STALE LIQUIDATION PENDING] Market sell {} for Hunt {} still non-terminal ('{}'); deferring to next cycle.",
                                        mkt_id, hunt.id, mkt_term.status
                                    );
                                    if !mkt_id.is_empty() {
                                        if partial_qty > Decimal::ZERO && order_qty > Decimal::ZERO
                                        {
                                            hunt.buy_qty = floor_qty(
                                                (buy_qty * (liq_qty / order_qty)).max(liq_qty),
                                                decimals,
                                            );
                                        }
                                        hunt.sell_order_id = Some(mkt_id);
                                        hunt.qty = liq_qty;
                                        hunt.filled_exit_qty =
                                            floor_qty(mkt_term.filled_qty, decimals);
                                        hunt.status = if mkt_term.filled_qty > Decimal::ZERO {
                                            HuntStatus::ExitPartiallyFilled
                                        } else {
                                            HuntStatus::OpenExitPending
                                        };
                                    } else {
                                        hunt.sell_order_id = None;
                                        hunt.qty = liq_qty;
                                        hunt.filled_exit_qty = Decimal::ZERO;
                                        hunt.status = HuntStatus::ExitSubmitPending;
                                    }
                                    deferred_exit_recovery = true;
                                    history_changed = true;
                                    continue;
                                }

                                if !mkt_id.is_empty() {
                                    // Record BOTH canceled limit OID and terminal market liquidation OID!
                                    cycle_closed_order_ids.insert(mkt_id.clone());
                                }

                                // CE-5.3 fix (rejected/canceled/expired with 0 fill): NEVER fabricate LiquidatedStale!
                                if !mkt_term.is_filled() && mkt_term.filled_qty <= Decimal::ZERO {
                                    error!(
                                        "[STALE LIQUIDATION REJECTED] Market sell {} for Hunt {} ended '{}' with 0 fills; transitioning to ExitSubmitPending.",
                                        mkt_id, hunt.id, mkt_term.status
                                    );
                                    if partial_qty > Decimal::ZERO && order_qty > Decimal::ZERO {
                                        hunt.buy_qty = floor_qty(
                                            (buy_qty * (liq_qty / order_qty)).max(liq_qty),
                                            decimals,
                                        );
                                    }
                                    hunt.sell_order_id = None;
                                    hunt.qty = liq_qty;
                                    hunt.filled_exit_qty = Decimal::ZERO;
                                    hunt.status = HuntStatus::ExitSubmitPending;
                                    history_changed = true;
                                    continue;
                                }

                                // Terminal partial market fill leaving non-dust remainder: keep active!
                                if !mkt_term.is_filled() && mkt_term.filled_qty > Decimal::ZERO {
                                    let mkt_rem = floor_qty(
                                        (liq_qty - mkt_term.filled_qty).max(Decimal::ZERO),
                                        decimals,
                                    );
                                    if !is_dust_qty(mkt_rem, stale_ref_px, is_crypto) {
                                        warn!(
                                            "[STALE LIQUIDATION PARTIAL] Market sell {} for Hunt {} ended '{}' filling {}/{}; keeping remaining {} as ExitSubmitPending.",
                                            mkt_id,
                                            hunt.id,
                                            mkt_term.status,
                                            mkt_term.filled_qty,
                                            liq_qty,
                                            mkt_rem
                                        );
                                        if order_qty > Decimal::ZERO {
                                            hunt.buy_qty = floor_qty(
                                                (buy_qty * (mkt_rem / order_qty)).max(mkt_rem),
                                                decimals,
                                            );
                                        }
                                        hunt.sell_order_id = None;
                                        hunt.qty = mkt_rem;
                                        hunt.filled_exit_qty = Decimal::ZERO;
                                        hunt.status = HuntStatus::ExitSubmitPending;
                                        history_changed = true;
                                        continue;
                                    }
                                }

                                let mkt_filled_qty = if mkt_term.filled_qty > Decimal::ZERO {
                                    mkt_term.filled_qty
                                } else {
                                    liq_qty
                                };
                                let mkt_fill_px = mkt_term
                                    .filled_avg_price
                                    .filter(|p| *p > Decimal::ZERO)
                                    .unwrap_or_else(|| {
                                        if bid > Decimal::ZERO {
                                            bid
                                        } else {
                                            buy_px
                                        }
                                    });
                                let mkt_time = mkt_term
                                    .filled_at
                                    .clone()
                                    .unwrap_or_else(|| now_str.clone());

                                let total_sold_qty = partial_qty + mkt_filled_qty;
                                let blended_px = if total_sold_qty > Decimal::ZERO {
                                    (partial_qty * partial_px + mkt_filled_qty * mkt_fill_px)
                                        / total_sold_qty
                                } else {
                                    mkt_fill_px
                                };

                                let actual_cfee = fetch_actual_cfee_usd(
                                    client,
                                    &[sell_oid.as_str(), mkt_id.as_str()],
                                    is_crypto,
                                )
                                .await;
                                if let Some(fee) = actual_cfee {
                                    if partial_qty <= Decimal::ZERO {
                                        calibrate_fee_tracker_from_sell(
                                            fee_tracker,
                                            fee,
                                            mkt_fill_px * mkt_filled_qty,
                                            true,
                                        );
                                    }
                                }

                                let (net_pnl, pnl_pct, gross_pnl, fees_usd) = compute_net_trade_pnl(
                                    buy_px,
                                    blended_px,
                                    buy_qty,
                                    total_sold_qty,
                                    is_crypto,
                                    fee_tracker.maker_fee_rate(),
                                    fee_tracker.taker_fee_rate(),
                                    actual_cfee,
                                    buy_fee_deducted,
                                );

                                hunt.sell_order_id = if !mkt_id.is_empty() {
                                    Some(mkt_id)
                                } else {
                                    Some(sell_oid)
                                };
                                hunt.sell_price = Some(blended_px.round_dp_with_strategy(
                                    4,
                                    RoundingStrategy::MidpointNearestEven,
                                ));
                                hunt.qty = floor_qty(total_sold_qty, decimals);
                                hunt.filled_exit_qty = floor_qty(total_sold_qty, decimals);
                                hunt.sell_timestamp = Some(mkt_time);
                                hunt.pnl = Some(net_pnl);
                                hunt.pnl_pct = Some(pnl_pct);
                                hunt.gross_pnl = Some(gross_pnl);
                                hunt.fees_usd = Some(fees_usd);
                                hunt.status = HuntStatus::LiquidatedStale;
                                history_changed = true;

                                info!(
                                    "[LIQUIDATED STALE] Hunt {}: Market sold {} @ ${} (Net PnL: ${}, Fees: ${})",
                                    hunt.id,
                                    format_order_qty(mkt_filled_qty, is_crypto),
                                    format_tick_price(mkt_fill_px),
                                    net_pnl,
                                    fees_usd
                                );
                            }
                            Err(le) => {
                                error!(
                                    "[STALE LIQUIDATION ERROR] Market sell failed for Hunt {}: {}",
                                    hunt.id, le
                                );
                                if partial_qty > Decimal::ZERO && order_qty > Decimal::ZERO {
                                    hunt.buy_qty = floor_qty(
                                        (buy_qty * (liq_qty / order_qty)).max(liq_qty),
                                        decimals,
                                    );
                                }
                                hunt.sell_order_id = None;
                                hunt.qty = liq_qty;
                                hunt.filled_exit_qty = Decimal::ZERO;
                                hunt.status = HuntStatus::ExitSubmitPending;
                                history_changed = true;
                            }
                        }
                    } else {
                        // Nothing left at broker after terminal cancel
                        let fill_px = if partial_px > Decimal::ZERO {
                            partial_px
                        } else if bid > Decimal::ZERO {
                            bid
                        } else {
                            buy_px
                        };
                        let sold_qty = if partial_qty > Decimal::ZERO {
                            partial_qty
                        } else {
                            buy_qty
                        };
                        let actual_cfee = if partial_qty > Decimal::ZERO {
                            fetch_actual_cfee_usd(client, &[sell_oid.as_str()], is_crypto).await
                        } else {
                            None
                        };
                        let (net_pnl, pnl_pct, gross_pnl, fees_usd) = compute_net_trade_pnl(
                            buy_px,
                            fill_px,
                            buy_qty,
                            sold_qty,
                            is_crypto,
                            fee_tracker.maker_fee_rate(),
                            fee_tracker.maker_fee_rate(),
                            actual_cfee,
                            buy_fee_deducted,
                        );
                        hunt.sell_price = Some(fill_px);
                        hunt.qty = floor_qty(sold_qty, decimals);
                        hunt.filled_exit_qty = floor_qty(sold_qty, decimals);
                        hunt.sell_timestamp = Some(now_str.clone());
                        hunt.pnl = Some(net_pnl);
                        hunt.pnl_pct = Some(pnl_pct);
                        hunt.gross_pnl = Some(gross_pnl);
                        hunt.fees_usd = Some(fees_usd);
                        hunt.status = if hunt.adopted_from_broker {
                            HuntStatus::AdoptedClosed
                        } else {
                            HuntStatus::LiquidatedManual
                        };
                        history_changed = true;
                    }
                }
            }
        } else if chk_sell.is_terminal() {
            cycle_closed_order_ids.insert(sell_oid);
        }
    }

    // =========================================================================
    // STEP C: Refresh Broker State + Hardened 6-Gate Adoption
    // =========================================================================
    match client.get_position(symbol).await {
        Ok(pos) => {
            position = pos;
            position_fetch_ok = true;
        }
        Err(e) => {
            warn!("Post-hygiene get_position({}) failed: {}", symbol, e);
        }
    }

    match client.get_open_orders(Some(symbol)).await {
        Ok(orders) => {
            open_orders = orders;
            open_orders_fetch_ok = true;
        }
        Err(e) => {
            warn!("Post-hygiene get_open_orders({}) failed: {}", symbol, e);
        }
    }

    let broker_pos_qty = position
        .as_ref()
        .map(|p| floor_qty(p.qty, decimals))
        .unwrap_or(Decimal::ZERO);
    let pos_ref_px = if bid > Decimal::ZERO {
        bid
    } else {
        position
            .as_ref()
            .and_then(|p| p.current_price.filter(|c| *c > Decimal::ZERO))
            .or_else(|| {
                position
                    .as_ref()
                    .map(|p| p.avg_entry_price)
                    .filter(|p| *p > Decimal::ZERO)
            })
            .unwrap_or(Decimal::ZERO)
    };

    if open_orders_fetch_ok
        && position_fetch_ok
        && !is_dust_qty(broker_pos_qty, pos_ref_px, is_crypto)
    {
        // ALL sell_order_ids across EVERY hunt (active AND terminal) UNION cycle_closed_order_ids
        let mut all_known_sell_ids: HashSet<String> = trades_history
            .iter()
            .filter_map(|h| h.sell_order_id.clone())
            .filter(|id| !id.is_empty())
            .collect();
        all_known_sell_ids.extend(cycle_closed_order_ids.iter().cloned());

        let avg_entry_ref = position
            .as_ref()
            .map(|p| p.avg_entry_price)
            .filter(|p| *p > Decimal::ZERO)
            .unwrap_or(bid);

        for o in &open_orders {
            // Gate 1: Must be a sell order
            if o.side != "sell" {
                continue;
            }
            // Gate 2: Must be a limit order (NEVER adopt market liquidation orders)
            if o.order_type != "limit" {
                debug!(
                    "Adoption gate rejected non-limit sell order {} (type={})",
                    o.id, o.order_type
                );
                continue;
            }
            // Gate 3: Must have a strictly positive limit_price
            let Some(limit_px) = o.limit_price.filter(|p| *p > Decimal::ZERO) else {
                debug!(
                    "Adoption gate rejected order {} with missing/non-positive limit_price",
                    o.id
                );
                continue;
            };
            // Gate 4: Must NOT be known in ANY active or terminal hunt OR closed this cycle
            if o.id.is_empty() || all_known_sell_ids.contains(&o.id) {
                continue;
            }
            // Gate 5: Authoritative point-lookup `get_order` MUST confirm order is OPEN (not terminal!)
            let confirmed = match client.get_order(&o.id).await {
                Ok(ord) => ord,
                Err(e) => {
                    warn!(
                        "Adoption gate point-check get_order({}) failed ({}); skipping adoption",
                        o.id, e
                    );
                    continue;
                }
            };
            if !confirmed.is_open()
                || confirmed.is_terminal()
                || confirmed.side != "sell"
                || confirmed.order_type != "limit"
            {
                info!(
                    "Adoption gate rejected stale-index order {} (list_status='{}', authoritative_status='{}', type='{}')",
                    o.id, o.status, confirmed.status, confirmed.order_type
                );
                cycle_closed_order_ids.insert(o.id.clone());
                continue;
            }

            let confirmed_limit = confirmed
                .limit_price
                .filter(|p| *p > Decimal::ZERO)
                .unwrap_or(limit_px);
            let o_qty = confirmed.qty;
            let o_filled = confirmed.filled_qty;
            let adopted_status = if confirmed.status == "partially_filled" {
                HuntStatus::ExitPartiallyFilled
            } else {
                HuntStatus::OpenExitPending
            };
            let short_id: String = confirmed.id.chars().take(4).collect();

            let adopted_rec = TradeRecord {
                id: format!("adp-{}", short_id),
                symbol: symbol.to_string(),
                buy_order_id: None,
                sell_order_id: Some(confirmed.id.clone()),
                buy_qty: o_qty,
                qty: o_qty,
                filled_exit_qty: o_filled,
                buy_price: {
                    let rounded = avg_entry_ref
                        .round_dp_with_strategy(4, RoundingStrategy::MidpointNearestEven);
                    if rounded > Decimal::ZERO {
                        rounded
                    } else {
                        avg_entry_ref
                    }
                },
                sell_price: Some(confirmed_limit),
                pnl: None,
                pnl_pct: None,
                gross_pnl: None,
                fees_usd: None,
                buy_timestamp: confirmed
                    .created_at
                    .clone()
                    .unwrap_or_else(|| now_str.clone()),
                sell_timestamp: None,
                status: adopted_status,
                adopted_from_broker: true,
                recovered_orphan: false,
                buy_fee_already_deducted: if is_crypto { Some(true) } else { None },
            };

            info!(
                "[ADOPTED OPEN ORDER] Tracking broker Limit SELL {}: {} {} @ ${}",
                confirmed.id,
                format_order_qty(o_qty, is_crypto),
                symbol,
                format_tick_price(confirmed_limit)
            );
            trades_history.push(adopted_rec);
            all_known_sell_ids.insert(confirmed.id);
            history_changed = true;
        }
    }

    // =========================================================================
    // STEP D: Orphan Auto-Protection (Conservative Gross Basis + Pre-Persist)
    // =========================================================================
    let has_submit_pending = trades_history
        .iter()
        .any(|h| h.status == HuntStatus::ExitSubmitPending);

    if position_fetch_ok && !has_submit_pending && !deferred_exit_recovery {
        if let Some(pos) = position.clone() {
            let orphan_avail = floor_qty(pos.qty_available, decimals);
            let cur_px = if bid > Decimal::ZERO {
                bid
            } else {
                pos.current_price.unwrap_or(Decimal::ZERO)
            };
            let avg_entry = if pos.avg_entry_price > Decimal::ZERO {
                pos.avg_entry_price
            } else {
                cur_px
            };
            let orphan_ref_px = if cur_px > Decimal::ZERO {
                cur_px
            } else {
                avg_entry
            };

            if !is_dust_qty(orphan_avail, orphan_ref_px, is_crypto) && avg_entry > Decimal::ZERO {
                // Reconstruct conservative gross buy_qty so break-even clears BOTH worst-case buy & sell fees
                let reconstructed_buy_qty = if is_crypto {
                    let denom = Decimal::ONE - fee_tracker.worst_buy_fee_rate();
                    if denom > Decimal::ZERO {
                        (orphan_avail / denom)
                            .round_dp_with_strategy(decimals, RoundingStrategy::AwayFromZero)
                    } else {
                        orphan_avail
                    }
                } else {
                    orphan_avail
                };

                // CE-1.5 fix: round stored_buy_px BEFORE pricing and price against max(avg_entry, stored_buy_px)
                // so recorded cost basis can NEVER exceed the priced cost basis!
                let rounded_buy_px =
                    avg_entry.round_dp_with_strategy(4, RoundingStrategy::MidpointNearestEven);
                let stored_buy_px = if rounded_buy_px > Decimal::ZERO {
                    rounded_buy_px
                } else {
                    avg_entry
                };
                let pricing_buy_px = avg_entry.max(stored_buy_px);

                let exit_px = resolve_profitable_exit_price(
                    reconstructed_buy_qty,
                    pricing_buy_px,
                    orphan_avail,
                    fair_price,
                    None,
                    cur_px,
                    fee_tracker,
                    is_crypto,
                );

                warn!(
                    "[ORPHAN DETECTED] Found {} unprotected {} (avg_entry=${}, fair=${}); placing guaranteed-profitable Limit SELL @ ${}",
                    format_order_qty(orphan_avail, is_crypto),
                    symbol,
                    format_tick_price(pricing_buy_px),
                    format_tick_price(fair_price),
                    format_tick_price(exit_px)
                );

                let rec_short: String = Uuid::new_v4().to_string().chars().take(4).collect();
                let orphan_id = format!("rec-{}", rec_short);
                let orphan_rec = TradeRecord {
                    id: orphan_id.clone(),
                    symbol: symbol.to_string(),
                    buy_order_id: None,
                    sell_order_id: None,
                    buy_qty: reconstructed_buy_qty,
                    qty: orphan_avail,
                    filled_exit_qty: Decimal::ZERO,
                    buy_price: stored_buy_px,
                    sell_price: Some(exit_px),
                    pnl: None,
                    pnl_pct: None,
                    gross_pnl: None,
                    fees_usd: None,
                    buy_timestamp: now_str.clone(),
                    sell_timestamp: None,
                    status: HuntStatus::ExitSubmitPending,
                    adopted_from_broker: false,
                    recovered_orphan: true,
                    buy_fee_already_deducted: if is_crypto { Some(true) } else { None },
                };

                // CRITICAL: Persist ExitSubmitPending to disk BEFORE submitting protective exit
                trades_history.push(orphan_rec);
                history_changed = true;
                if let Some(st) = store {
                    if let Err(e) = st.save_hunts(trades_history, DEFAULT_MAX_TERMINAL_HUNTS) {
                        warn!(
                            "Failed to pre-persist orphan {} as ExitSubmitPending: {}",
                            orphan_id, e
                        );
                    }
                }

                let (sell_ord_opt, submitted_qty, actual_exit_px) = submit_exit_sell_with_retry(
                    client,
                    symbol,
                    reconstructed_buy_qty,
                    orphan_avail,
                    exit_px,
                    limit_tif,
                    2,
                    |adj_qty| {
                        try_resolve_profitable_exit_price(
                            reconstructed_buy_qty,
                            pricing_buy_px,
                            adj_qty,
                            fair_price,
                            None,
                            fee_tracker,
                            is_crypto,
                        )
                    },
                )
                .await;

                if let Some(sell_ord) = sell_ord_opt {
                    if let Some(rec) = trades_history
                        .iter_mut()
                        .rev()
                        .find(|h| h.id == orphan_id)
                    {
                        rec.sell_order_id = Some(sell_ord.id.clone());
                        rec.qty = submitted_qty;
                        rec.sell_price = Some(actual_exit_px);
                        if sell_ord.is_filled() {
                            cycle_closed_order_ids.insert(sell_ord.id.clone());
                            finalize_filled_exit(
                                client,
                                rec,
                                &sell_ord,
                                actual_exit_px,
                                submitted_qty,
                                fee_tracker,
                                is_crypto,
                                &now_str,
                            )
                            .await;
                        } else {
                            rec.status = HuntStatus::OpenExitPending;
                            info!(
                                "[ORPHAN PROTECTED] Placed protective SELL {} @ ${} (Order ID: {})",
                                format_order_qty(submitted_qty, is_crypto),
                                format_tick_price(actual_exit_px),
                                sell_ord.id
                            );
                        }
                    }

                    if let Ok(p) = client.get_position(symbol).await {
                        position = p;
                    }
                    if let Ok(o) = client.get_open_orders(Some(symbol)).await {
                        open_orders = o;
                    }
                }
            }
        }
    }

    // Filter out any stale-index open orders confirmed terminal in this cycle for clean telemetry
    let clean_open_orders: Vec<Order> = open_orders
        .into_iter()
        .filter(|o| !cycle_closed_order_ids.contains(&o.id))
        .collect();

    let broker_telemetry = build_broker_telemetry(
        account.as_ref(),
        position.as_ref(),
        &clean_open_orders,
        trades_history,
    );

    (history_changed, broker_telemetry)
}
