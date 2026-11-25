use crate::alpaca::{AlpacaApi, OrderRequest};
use crate::config::StrategyParams;
use crate::engine::reconciliation::{
    calibrate_fee_tracker_from_sell, classify_filled_exit_status, compute_safe_exit_sell_qty,
    fetch_actual_cfee_usd, submit_exit_sell_with_retry, try_resolve_profitable_exit_price,
    wait_for_terminal_order,
};
use crate::model::{
    calculate_dynamic_portfolio_shares, calculate_hidden_mm_buy_price,
    calculate_notional_capped_shares, compute_guaranteed_profitable_exit_price,
    compute_net_trade_pnl, decimal_from_f64_clean, decimal_to_f64, floor_qty, format_order_qty,
    format_tick_price, FeeTracker, CFEE_CEIL_BUFFER_USD, MIN_NET_PROFIT_MARGIN,
    MIN_ORDER_NOTIONAL_USD,
};
use crate::state::{HuntStatus, StateStore, TradeRecord};
use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::time::{sleep, Instant};
use tracing::{info, warn};
use uuid::Uuid;

const DEFAULT_MAX_TERMINAL_HUNTS: usize = 50;

/// Timing configuration for HiddenMM entry polling, cancel confirmation, and balance settlement.
#[derive(Debug, Clone, Copy)]
pub struct HuntTiming {
    pub hidden_mm_timeout: Duration,
    pub hidden_mm_poll_interval: Duration,
    pub terminal_wait: Duration,
    pub terminal_poll_interval: Duration,
    pub settle_retries: usize,
    pub settle_sleep: Duration,
}

impl HuntTiming {
    pub fn from_params(params: &StrategyParams) -> Self {
        Self {
            hidden_mm_timeout: Duration::from_secs(params.hidden_mm_timeout_seconds),
            hidden_mm_poll_interval: Duration::from_millis(1500),
            terminal_wait: Duration::from_secs(8),
            terminal_poll_interval: Duration::from_millis(500),
            settle_retries: 3,
            settle_sleep: Duration::from_millis(400),
        }
    }

    pub fn fast_for_tests() -> Self {
        Self {
            hidden_mm_timeout: Duration::from_millis(25),
            hidden_mm_poll_interval: Duration::from_millis(1),
            terminal_wait: Duration::from_millis(50),
            terminal_poll_interval: Duration::from_millis(1),
            settle_retries: 2,
            settle_sleep: Duration::from_millis(0),
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum HuntSizingMode {
    DynamicPortfolio,
    FixedNotionalCap {
        hunt_cap_usd: Decimal,
        remaining_exposure_usd: Decimal,
    },
}

/// Executes a complete, crash-safe, fee-aware Market Hunt using `calculate_dynamic_portfolio_shares`.
/// Preserved with its 9-argument signature for full backward compatibility.
#[allow(clippy::too_many_arguments)]
pub async fn execute_hunt<C: AlpacaApi>(
    client: &C,
    params: &StrategyParams,
    fair_price_f64: f64,
    trades_history: &mut Vec<TradeRecord>,
    fee_tracker: &mut FeeTracker,
    store: Option<&StateStore>,
    now_dt: DateTime<Utc>,
    timing: HuntTiming,
    shutdown_flag: Option<&AtomicBool>,
) -> Result<Option<TradeRecord>, String> {
    execute_hunt_inner(
        client,
        params,
        fair_price_f64,
        trades_history,
        fee_tracker,
        store,
        now_dt,
        timing,
        shutdown_flag,
        HuntSizingMode::DynamicPortfolio,
    )
    .await
}

/// Executes a complete, crash-safe, fee-aware Market Hunt using fixed-USD regime notional caps
/// (`calculate_notional_capped_shares`) constrained by both `hunt_cap_usd` and `remaining_exposure_usd`.
#[allow(clippy::too_many_arguments)]
pub async fn execute_hunt_with_notional_cap<C: AlpacaApi>(
    client: &C,
    params: &StrategyParams,
    fair_price_f64: f64,
    trades_history: &mut Vec<TradeRecord>,
    fee_tracker: &mut FeeTracker,
    store: Option<&StateStore>,
    now_dt: DateTime<Utc>,
    timing: HuntTiming,
    shutdown_flag: Option<&AtomicBool>,
    hunt_cap_usd: Decimal,
    remaining_exposure_usd: Decimal,
) -> Result<Option<TradeRecord>, String> {
    execute_hunt_inner(
        client,
        params,
        fair_price_f64,
        trades_history,
        fee_tracker,
        store,
        now_dt,
        timing,
        shutdown_flag,
        HuntSizingMode::FixedNotionalCap {
            hunt_cap_usd,
            remaining_exposure_usd,
        },
    )
    .await
}

/// Shared crash-safe HiddenMM Market Hunt implementation:
/// 1. Fetches a **fresh** quote immediately before sizing & order submission to prevent stale-quote crossing.
/// 2. Computes strictly passive `buy_price = calculate_hidden_mm_buy_price(fresh_bid, fresh_ask)`.
/// 3. Re-verifies executable margin `(fair_price - buy_price) / fair_price >= min_margin` (and `<= max_margin`).
/// 4. Sizes via `HuntSizingMode` (`qty * buy_price <= min(cap, cash)`), submits passive limit BUY,
///    polls up to `hidden_mm_timeout_seconds`, cancels remainder if open, and waits for strictly terminal state.
/// 5. On fill (`filled_qty > 0`):
///    - Calls `compute_safe_exit_sell_qty` FIRST to learn exact net credited balance.
///    - Calibrates `FeeTracker` from realized buy-fee delta (`1 - net_qty / buy_qty`).
///    - Computes `exit_price = compute_guaranteed_profitable_exit_price(...)`.
///    - Persists `TradeRecord` (`ExitSubmitPending`) atomically to disk BEFORE calling `submit_exit_sell_with_retry`.
///    - Finalizes to `Completed` (fetching actual `CFEE`) if immediately filled, else `OpenExitPending`.
#[allow(clippy::too_many_arguments)]
async fn execute_hunt_inner<C: AlpacaApi>(
    client: &C,
    params: &StrategyParams,
    fair_price_f64: f64,
    trades_history: &mut Vec<TradeRecord>,
    fee_tracker: &mut FeeTracker,
    store: Option<&StateStore>,
    now_dt: DateTime<Utc>,
    timing: HuntTiming,
    shutdown_flag: Option<&AtomicBool>,
    sizing_mode: HuntSizingMode,
) -> Result<Option<TradeRecord>, String> {
    let Some(fair_price) = decimal_from_f64_clean(fair_price_f64).filter(|d| *d > Decimal::ZERO)
    else {
        return Ok(None);
    };

    if let HuntSizingMode::FixedNotionalCap {
        hunt_cap_usd,
        remaining_exposure_usd,
    } = sizing_mode
    {
        if hunt_cap_usd < MIN_ORDER_NOTIONAL_USD
            || remaining_exposure_usd < MIN_ORDER_NOTIONAL_USD
        {
            info!(
                "Skipping hunt on {}: exposure/cap budget below minimum notional (hunt_cap=${}, remaining_exposure=${})",
                params.symbol,
                hunt_cap_usd.round_dp(2),
                remaining_exposure_usd.round_dp(2)
            );
            return Ok(None);
        }
    }

    let symbol = params.symbol.as_str();
    let is_crypto = params.is_crypto();
    let decimals: u32 = if is_crypto { 9 } else { 4 };
    let limit_tif = params.limit_tif();
    let now_str = now_dt.to_rfc3339();

    // 1. Fetch fresh quote immediately before sizing & submission
    let fresh_quote = client
        .get_latest_quote(symbol)
        .await
        .map_err(|e| format!("Failed to fetch fresh quote for {}: {}", symbol, e))?;
    let fresh_bid = fresh_quote.bid_price;
    let fresh_ask = fresh_quote.ask_price;

    // 2. Strictly passive HiddenMM limit BUY price (never crosses ask)
    let Some(buy_price) = calculate_hidden_mm_buy_price(fresh_bid, fresh_ask) else {
        warn!(
            "Aborting hunt on {}: invalid/locked/crossed fresh quote (bid={}, ask={})",
            symbol, fresh_bid, fresh_ask
        );
        return Ok(None);
    };

    // 3. Re-verify executable margin against fresh passive buy_price
    let exec_margin_dec = (fair_price - buy_price) / fair_price;
    let min_margin_dec = params.min_margin_decimal();
    let max_margin_dec = params.max_margin_decimal();
    let exec_margin_f64 = decimal_to_f64(exec_margin_dec);

    if exec_margin_dec < min_margin_dec {
        info!(
            "Aborting hunt on {}: fresh executable margin {:.4}% < min_margin {:.4}% (price ran away: buy_px=${}, fair=${})",
            symbol,
            exec_margin_f64 * 100.0,
            params.min_margin * 100.0,
            format_tick_price(buy_price),
            format_tick_price(fair_price)
        );
        return Ok(None);
    }

    if exec_margin_dec > max_margin_dec {
        warn!(
            "Aborting hunt on {}: fresh executable margin {:.4}% > max_margin {:.4}% (circuit breaker)",
            symbol,
            exec_margin_f64 * 100.0,
            params.max_margin * 100.0
        );
        return Ok(None);
    }

    // 4. Fetch fresh account equity/cash and pre-buy available position
    let account = client
        .get_account()
        .await
        .map_err(|e| format!("Failed to fetch pre-buy account: {}", e))?;
    let equity = account.equity;
    let cash = account.cash;

    let avail_before = match client.get_position(symbol).await {
        Ok(Some(pos)) => Some(floor_qty(pos.qty_available, decimals)),
        Ok(None) => Some(Decimal::ZERO),
        Err(e) => {
            warn!("Failed to fetch pre-buy position for {}: {}", symbol, e);
            None
        }
    };

    // 5. Position sizing with strict cash & cap invariants
    let shares = match sizing_mode {
        HuntSizingMode::DynamicPortfolio => calculate_dynamic_portfolio_shares(
            exec_margin_f64,
            params.min_margin,
            params.max_margin,
            params.level,
            params.max_shares_decimal(),
            equity,
            cash,
            buy_price,
            is_crypto,
        ),
        HuntSizingMode::FixedNotionalCap {
            hunt_cap_usd,
            remaining_exposure_usd,
        } => calculate_notional_capped_shares(
            exec_margin_f64,
            params.min_margin,
            params.max_margin,
            params.level,
            hunt_cap_usd,
            remaining_exposure_usd,
            cash,
            buy_price,
            is_crypto,
        ),
    };

    if shares <= Decimal::ZERO {
        info!(
            "Hunt sizing returned 0 shares for {} (cash=${}, equity=${}, buy_px=${}, exec_margin={:.4}%)",
            symbol,
            cash.round_dp(2),
            equity.round_dp(2),
            format_tick_price(buy_price),
            exec_margin_f64 * 100.0
        );
        return Ok(None);
    }

    let buy_price_str = format_tick_price(buy_price);
    let buy_qty_str = format_order_qty(shares, is_crypto);

    info!(
        "[HiddenMM] Placing passive limit BUY {} {} @ ${} (notional=${}, bid=${}, ask=${}, fair=${}, exec_margin={:.4}%)",
        buy_qty_str,
        symbol,
        buy_price_str,
        (shares * buy_price).round_dp(2),
        format_tick_price(fresh_bid),
        format_tick_price(fresh_ask),
        format_tick_price(fair_price),
        exec_margin_f64 * 100.0
    );

    let buy_req = OrderRequest {
        symbol: symbol.to_string(),
        qty: buy_qty_str.clone(),
        side: "buy".to_string(),
        order_type: "limit".to_string(),
        time_in_force: limit_tif.to_string(),
        limit_price: Some(buy_price_str.clone()),
        client_order_id: Some(format!("hunt-buy-{}", Uuid::new_v4())),
        extended_hours: None,
    };

    let submitted_buy = match client.submit_order(&buy_req).await {
        Ok(ord) => ord,
        Err(e) => {
            warn!(
                "[BUY REJECTED] Could not submit BUY {} {} @ ${}: {}",
                buy_qty_str, symbol, buy_price_str, e
            );
            return Ok(None);
        }
    };

    // 6. Poll up to hidden_mm_timeout, then cancel if not terminal and wait for terminal state
    let ord_id = submitted_buy.id.clone();
    let mut latest_buy = submitted_buy;
    let start_wait = Instant::now();

    while !latest_buy.is_terminal() && start_wait.elapsed() < timing.hidden_mm_timeout {
        if shutdown_flag.is_some_and(|f| f.load(Ordering::Relaxed)) {
            info!(
                "Shutdown requested during HiddenMM buy wait for {}; canceling open buy order.",
                ord_id
            );
            break;
        }
        let remaining = timing.hidden_mm_timeout.saturating_sub(start_wait.elapsed());
        let nap = timing.hidden_mm_poll_interval.min(remaining);
        if !nap.is_zero() {
            sleep(nap).await;
        }
        match client.get_order(&ord_id).await {
            Ok(chk) => {
                latest_buy = chk;
                if latest_buy.is_terminal() {
                    break;
                }
            }
            Err(e) => {
                warn!("Error polling buy order {}: {}", ord_id, e);
            }
        }
    }

    if !latest_buy.is_terminal() {
        info!(
            "HiddenMM timeout reached for buy order {}. Canceling remainder.",
            ord_id
        );
        if let Err(e) = client.cancel_order(&ord_id).await {
            warn!("Error canceling buy order {}: {}", ord_id, e);
        }
        if let Some(term) = wait_for_terminal_order(
            client,
            &ord_id,
            timing.terminal_wait,
            timing.terminal_poll_interval,
            Some(latest_buy.clone()),
        )
        .await
        {
            latest_buy = term;
        }
    }

    let filled_qty = latest_buy.filled_qty;
    if filled_qty <= Decimal::ZERO {
        info!(
            "[UNFILLED] HiddenMM buy order {} finished with 0 fills. Hunt ended.",
            ord_id
        );
        return Ok(None);
    }

    let avg_buy_price = latest_buy
        .filled_avg_price
        .filter(|p| *p > Decimal::ZERO)
        .unwrap_or(buy_price);
    let buy_fill_time = latest_buy
        .filled_at
        .clone()
        .unwrap_or_else(|| now_str.clone());

    // 7. Compute safe exit sell quantity FIRST to observe exact net post-fee crypto delta
    let (safe_sell_qty, live_avail) = compute_safe_exit_sell_qty(
        filled_qty,
        symbol,
        client,
        avail_before,
        fee_tracker.worst_buy_fee_rate(),
        timing.settle_retries,
        timing.settle_sleep,
    )
    .await;

    // 8. Calibrate FeeTracker from realized buy-fill base-asset fee delta (only when observed live)
    if is_crypto
        && live_avail > Decimal::ZERO
        && safe_sell_qty > Decimal::ZERO
        && safe_sell_qty < filled_qty
    {
        if let Some(matched_tier) = fee_tracker.calibrate_from_buy_fill(filled_qty, safe_sell_qty) {
            info!(
                "Calibrated crypto fee tier from buy fill (gross={}, net={}): Tier {} (maker={}, taker={}, worst_sell_floor={})",
                filled_qty,
                safe_sell_qty,
                matched_tier.tier,
                matched_tier.maker_rate,
                matched_tier.taker_rate,
                fee_tracker.worst_sell_fee_rate()
            );
        }
    }

    let target_sell_qty = if safe_sell_qty > Decimal::ZERO {
        safe_sell_qty
    } else if is_crypto {
        floor_qty(
            filled_qty * (Decimal::ONE - fee_tracker.worst_buy_fee_rate()),
            decimals,
        )
    } else {
        floor_qty(filled_qty, decimals)
    };

    // 9. Compute mathematically guaranteed net-profitable exit price clearing worst-case fees
    let exit_price = compute_guaranteed_profitable_exit_price(
        filled_qty,
        avg_buy_price,
        target_sell_qty,
        fair_price,
        fee_tracker.worst_sell_fee_rate(),
        MIN_NET_PROFIT_MARGIN,
        CFEE_CEIL_BUFFER_USD,
        is_crypto,
    )
    .map_err(|e| format!("Failed to compute guaranteed profitable exit price: {}", e))?;

    info!(
        "[FILLED] Bought {} {} @ ${} | Net sellable: {} (live_avail={}) -> Guaranteed Exit SELL @ ${} (fair=${})",
        format_order_qty(filled_qty, is_crypto),
        symbol,
        format_tick_price(avg_buy_price),
        format_order_qty(target_sell_qty, is_crypto),
        format_order_qty(live_avail, is_crypto),
        format_tick_price(exit_price),
        format_tick_price(fair_price)
    );

    // 10. CRITICAL: Persist `ExitSubmitPending` atomically BEFORE calling `submit_exit_sell_with_retry`
    let hunt_id: String = Uuid::new_v4().to_string().chars().take(8).collect();
    let trade_rec = TradeRecord {
        id: hunt_id.clone(),
        symbol: symbol.to_string(),
        buy_order_id: Some(ord_id),
        sell_order_id: None,
        buy_qty: filled_qty,
        qty: target_sell_qty,
        filled_exit_qty: Decimal::ZERO,
        buy_price: avg_buy_price,
        sell_price: Some(exit_price),
        pnl: None,
        pnl_pct: None,
        gross_pnl: None,
        fees_usd: None,
        buy_timestamp: buy_fill_time,
        sell_timestamp: None,
        status: HuntStatus::ExitSubmitPending,
        adopted_from_broker: false,
        recovered_orphan: false,
        buy_fee_already_deducted: if is_crypto { Some(true) } else { None },
    };

    trades_history.push(trade_rec);
    if let Some(st) = store {
        if let Err(e) = st.save_hunts(trades_history, DEFAULT_MAX_TERMINAL_HUNTS) {
            warn!(
                "Failed to pre-persist hunt {} as ExitSubmitPending: {}",
                hunt_id, e
            );
        }
    }

    // 11. Submit protective exit limit SELL with balance-reconciliation retry & dynamic re-pricing
    let (sell_ord_opt, submitted_qty, actual_exit_price) = submit_exit_sell_with_retry(
        client,
        symbol,
        filled_qty,
        target_sell_qty,
        exit_price,
        limit_tif,
        2,
        |adj_qty| {
            try_resolve_profitable_exit_price(
                filled_qty,
                avg_buy_price,
                adj_qty,
                fair_price,
                Some(exit_price),
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
            .find(|h| h.id == hunt_id)
        {
            rec.sell_order_id = Some(sell_ord.id.clone());
            rec.qty = submitted_qty;
            rec.sell_price = Some(actual_exit_price);

            if sell_ord.is_filled() {
                let fill_px = sell_ord
                    .filled_avg_price
                    .filter(|p| *p > Decimal::ZERO)
                    .unwrap_or(actual_exit_price);
                let fill_s_qty = if sell_ord.filled_qty > Decimal::ZERO {
                    sell_ord.filled_qty
                } else {
                    submitted_qty
                };
                let actual_cfee =
                    fetch_actual_cfee_usd(client, &[sell_ord.id.as_str()], is_crypto).await;
                if let Some(fee) = actual_cfee {
                    calibrate_fee_tracker_from_sell(fee_tracker, fee, fill_px * fill_s_qty, false);
                }
                let (net_pnl, pnl_pct, gross_pnl, fees_usd) = compute_net_trade_pnl(
                    avg_buy_price,
                    fill_px,
                    filled_qty,
                    fill_s_qty,
                    is_crypto,
                    fee_tracker.maker_fee_rate(),
                    fee_tracker.maker_fee_rate(),
                    actual_cfee,
                    rec.buy_fee_already_deducted.unwrap_or(false),
                );
                rec.sell_price = Some(fill_px);
                rec.qty = fill_s_qty;
                rec.filled_exit_qty = fill_s_qty;
                rec.sell_timestamp = Some(
                    sell_ord
                        .filled_at
                        .clone()
                        .unwrap_or_else(|| now_str.clone()),
                );
                rec.pnl = Some(net_pnl);
                rec.pnl_pct = Some(pnl_pct);
                rec.gross_pnl = Some(gross_pnl);
                rec.fees_usd = Some(fees_usd);
                rec.status = classify_filled_exit_status(rec.adopted_from_broker, net_pnl);

                info!(
                    "[EXIT IMMEDIATELY FILLED] Hunt {} ({}): Sold {} @ ${} (Net PnL: ${}, Fees: ${})",
                    hunt_id,
                    rec.status,
                    format_order_qty(fill_s_qty, is_crypto),
                    format_tick_price(fill_px),
                    net_pnl,
                    fees_usd
                );
            } else {
                rec.status = HuntStatus::OpenExitPending;
                info!(
                    "[EXIT PLACED] Hunt {}: SELL {} @ ${} (Order ID: {})",
                    hunt_id,
                    format_order_qty(submitted_qty, is_crypto),
                    format_tick_price(actual_exit_price),
                    sell_ord.id
                );
            }
        }
    } else {
        warn!(
            "[EXIT DEFERRED] Hunt {} persisted as ExitSubmitPending; will auto-retry next cycle.",
            hunt_id
        );
    }

    if let Some(st) = store {
        if let Err(e) = st.save_hunts(trades_history, DEFAULT_MAX_TERMINAL_HUNTS) {
            warn!("Failed to persist finalized hunt {}: {}", hunt_id, e);
        }
    }

    let created = trades_history
        .iter()
        .rev()
        .find(|h| h.id == hunt_id)
        .cloned();
    Ok(created)
}
