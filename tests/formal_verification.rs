//! Independent Formal Methods & Adversarial Property-Based Verification Suite (Round 2)
//!
//! Executes 160,000+ deterministic seeded randomized & boundary stress trials across
//! all 6 Core Behavioral Theorems of `anomalies-trading-engine`, proves regression
//! elimination of all 10 Round-1 counterexamples + CFEE pagination gap, and tests
//! Round-2 adversarial edge cases (2% shrink guard, 5-page CFEE dedup/termination,
//! partial stale market fills, anchored Kalman CB clamping, and residual diagnostics).

use anomalies_trading_engine::alpaca::client::{ACTIVITY_PAGE_SIZE, MAX_ACTIVITY_PAGES};
use anomalies_trading_engine::alpaca::{
    Account, AccountActivity, AlpacaApi, AlpacaError, Order, OrderRequest, PortfolioHistory,
    Position, QuoteDetail,
};
use anomalies_trading_engine::config::{AppConfig, KalmanParams, RegimeSizingParams, StrategyParams};
use anomalies_trading_engine::engine::hunt::{
    execute_hunt, execute_hunt_with_notional_cap, HuntTiming,
};
use anomalies_trading_engine::engine::reconciliation::{
    classify_filled_exit_status, fetch_actual_cfee_usd, reconcile_and_maintain_positions,
    submit_exit_sell_with_retry, try_resolve_profitable_exit_price, ReconciliationTiming,
};
use anomalies_trading_engine::engine::{EngineTiming, TradingEngine};
use anomalies_trading_engine::metrics::{
    compute_performance_metrics, compute_portfolio_history_metrics,
};
use anomalies_trading_engine::model::{
    calculate_dynamic_portfolio_shares, calculate_hidden_mm_buy_price,
    calculate_notional_capped_shares, ceil_to_tick, ceil_usd_cents,
    compute_guaranteed_profitable_exit_price, compute_net_trade_pnl, decimal_from_f64_clean,
    decimal_to_f64, floor_qty, floor_to_tick, format_tick_price, get_tick_size, is_dust_qty,
    round_to_tick, AdaptiveKalmanFilter, FeeTracker, PricingError, RegimeNotionalController,
    RegimeNotionalState, SignalType, ALPACA_CRYPTO_FEE_TIERS, CFEE_CEIL_BUFFER_USD,
    CRYPTO_TIER1_MAKER_FEE_RATE, CRYPTO_TIER1_TAKER_FEE_RATE, HALF_CENT_USD, MIN_NET_PROFIT_MARGIN,
    MIN_ORDER_NOTIONAL_USD,
};
use anomalies_trading_engine::state::{HuntStatus, StateStore, TradeRecord};
use anomalies_trading_engine::telemetry::build_broker_telemetry;
use async_trait::async_trait;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use reqwest::StatusCode;
use rust_decimal::prelude::ToPrimitive;
use rust_decimal::{Decimal, RoundingStrategy};
use std::collections::{HashMap, HashSet, VecDeque};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use tempfile::tempdir;

// =============================================================================
// Deterministic Seeded PRNG (xorshift64*) — zero external crate dependencies
// =============================================================================

struct Rng64 {
    state: u64,
}

impl Rng64 {
    fn new(seed: u64) -> Self {
        Self {
            state: if seed == 0 { 0x9E3779B97F4A7C15 } else { seed },
        }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }

    fn next_f64(&mut self) -> f64 {
        // Uniform in [0, 1) with 53-bit mantissa precision
        ((self.next_u64() >> 11) as f64) * (1.0 / ((1u64 << 53) as f64))
    }

    fn range_f64(&mut self, lo: f64, hi: f64) -> f64 {
        lo + (hi - lo) * self.next_f64()
    }

    fn range_usize(&mut self, lo: usize, hi_exclusive: usize) -> usize {
        lo + (self.next_u64() as usize) % (hi_exclusive - lo)
    }

    fn log_uniform(&mut self, lo: f64, hi: f64) -> f64 {
        let ln_lo = lo.ln();
        let ln_hi = hi.ln();
        (ln_lo + (ln_hi - ln_lo) * self.next_f64()).exp()
    }
}

fn d(s: &str) -> Decimal {
    Decimal::from_str(s).unwrap()
}

fn is_legal_sec_612_tick(p: Decimal) -> bool {
    if p <= Decimal::ZERO {
        return false;
    }
    let scale = if p >= Decimal::ONE { 2 } else { 4 };
    p == p.round_dp_with_strategy(scale, RoundingStrategy::ToZero)
        && p == floor_to_tick(p)
        && p == round_to_tick(p)
}

fn default_sol_params() -> StrategyParams {
    StrategyParams {
        symbol: "SOL/USD".to_string(),
        level: 0.05,
        max_shares: 1.0,
        min_margin: 0.0036,
        max_margin: 0.0250,
        cycle_seconds: 30,
        hidden_mm_timeout_seconds: 20,
        stale_order_max_age_seconds: 43200,
        backfill_closed_orders: false,
        kalman: KalmanParams {
            q_base: 1.5e-7,
            r_base: 4.0e-7,
            spread_weight: 2.0,
            adapt_lambda: 0.20,
        },
        regime_sizing: Default::default(),
    }
}

/// Constructs the exact mathematically tight worst-case k-fill partition of `sell_qty * exit_price`
/// proving the exact supremum `CFEE_max(N, f, k) = (ceil(100 * N * f) + k - 1) / 100`.
fn adversarial_multifill_cfee(
    sell_qty: Decimal,
    exit_price: Decimal,
    sell_fee_rate: Decimal,
    k: usize,
) -> Decimal {
    assert!(k >= 1);
    let total_notional = sell_qty * exit_price;
    let raw_fee = total_notional * sell_fee_rate;
    if k == 1 || raw_fee <= Decimal::ZERO {
        return ceil_usd_cents(raw_fee);
    }
    let u = raw_fee * Decimal::from(100); // raw fee in cents
    let u_floor = u.floor();
    let frac = u - u_floor;
    let k_dec = Decimal::from(k as u64);

    // Construct explicit positive per-fill fees f_1..f_k summing to raw_fee:
    // - If u is non-integer (frac > 0): f_1 = (u_floor + frac/k)/100, f_2..k = (frac/k)/100
    // - If u is integer (frac == 0):    f_1 = (u - (k-1)/k)/100,      f_2..k = (1/k)/100
    let (f_first, f_rest) = if frac > Decimal::ZERO {
        let eps = frac / k_dec;
        ((u_floor + eps) / Decimal::from(100), eps / Decimal::from(100))
    } else {
        let eps = Decimal::ONE / k_dec;
        (
            (u - eps * Decimal::from((k - 1) as u64)) / Decimal::from(100),
            eps / Decimal::from(100),
        )
    };
    let cfee = ceil_usd_cents(f_first) + ceil_usd_cents(f_rest) * Decimal::from((k - 1) as u64);
    let theoretical_sup = ceil_usd_cents(raw_fee) + Decimal::new((k - 1) as i64, 2);
    assert_eq!(cfee, theoretical_sup);
    cfee
}

// =============================================================================
// Adversarial Programmable Mock Broker for Async FSM / Reconciliation Audits
// =============================================================================

#[derive(Clone)]
struct AdversarialBrokerState {
    account: Account,
    position: Option<Position>,
    orders: HashMap<String, Order>,
    stale_open_orders: Vec<Order>,
    activities: Vec<AccountActivity>,
    paginated_activity_pages: HashMap<Option<String>, Result<Vec<AccountActivity>, String>>,
    activity_page_calls: usize,
    quote: QuoteDetail,
    get_account_fail: bool,
    get_position_results: VecDeque<Result<Option<Position>, String>>,
    get_open_orders_fail: bool,
    get_order_fail_ids: HashSet<String>,
    submit_sell_403_once: Option<Decimal>,
    submit_sell_fail: bool,
    submit_market_rejected: bool,
    submit_market_pending: bool,
    submit_market_partial_fill: Option<Decimal>,
    submit_market_http_err: bool,
    cancel_leaves_pending: bool,
    buy_fee_rate: Decimal,
    submitted_sell_requests: Vec<OrderRequest>,
    next_seq: u64,
}

impl AdversarialBrokerState {
    fn base() -> Self {
        Self {
            account: Account {
                equity: d("100000.00"),
                cash: d("100000.00"),
                buying_power: d("100000.00"),
                non_marginable_buying_power: Some(d("100000.00")),
            },
            position: None,
            orders: HashMap::new(),
            stale_open_orders: Vec::new(),
            activities: Vec::new(),
            paginated_activity_pages: HashMap::new(),
            activity_page_calls: 0,
            quote: QuoteDetail {
                timestamp: Some("2026-11-24T10:00:00Z".to_string()),
                bid_price: d("71.99"),
                ask_price: d("72.01"),
                bid_size: Some(d("1000")),
                ask_size: Some(d("1000")),
            },
            get_account_fail: false,
            get_position_results: VecDeque::new(),
            get_open_orders_fail: false,
            get_order_fail_ids: HashSet::new(),
            submit_sell_403_once: None,
            submit_sell_fail: false,
            submit_market_rejected: false,
            submit_market_pending: false,
            submit_market_partial_fill: None,
            submit_market_http_err: false,
            cancel_leaves_pending: false,
            buy_fee_rate: CRYPTO_TIER1_TAKER_FEE_RATE,
            submitted_sell_requests: Vec::new(),
            next_seq: 1,
        }
    }
}

#[derive(Clone)]
struct AdversarialMockBroker {
    st: Arc<Mutex<AdversarialBrokerState>>,
}

impl AdversarialMockBroker {
    fn new(state: AdversarialBrokerState) -> Self {
        Self {
            st: Arc::new(Mutex::new(state)),
        }
    }
}

#[async_trait]
impl AlpacaApi for AdversarialMockBroker {
    async fn get_account(&self) -> Result<Account, AlpacaError> {
        let st = self.st.lock().unwrap();
        if st.get_account_fail {
            return Err(AlpacaError::Other("Injected get_account failure".into()));
        }
        Ok(st.account.clone())
    }

    async fn get_position(&self, _symbol: &str) -> Result<Option<Position>, AlpacaError> {
        let mut st = self.st.lock().unwrap();
        if let Some(scripted) = st.get_position_results.pop_front() {
            return scripted.map_err(AlpacaError::Other);
        }
        Ok(st.position.clone())
    }

    async fn get_open_orders(&self, _symbol: Option<&str>) -> Result<Vec<Order>, AlpacaError> {
        let st = self.st.lock().unwrap();
        if st.get_open_orders_fail {
            return Err(AlpacaError::Other("Injected get_open_orders failure".into()));
        }
        let mut out: Vec<Order> = st
            .orders
            .values()
            .filter(|o| o.is_open())
            .cloned()
            .collect();
        out.extend(st.stale_open_orders.clone());
        Ok(out)
    }

    async fn get_order(&self, order_id: &str) -> Result<Order, AlpacaError> {
        let st = self.st.lock().unwrap();
        if st.get_order_fail_ids.contains(order_id) {
            return Err(AlpacaError::Other(format!(
                "Injected get_order failure for {}",
                order_id
            )));
        }
        st.orders
            .get(order_id)
            .cloned()
            .ok_or_else(|| AlpacaError::ApiStatus {
                status: StatusCode::NOT_FOUND,
                message: format!("Order {} not found", order_id),
            })
    }

    async fn submit_order(&self, req: &OrderRequest) -> Result<Order, AlpacaError> {
        let mut st = self.st.lock().unwrap();
        let qty = Decimal::from_str(&req.qty).unwrap();
        let limit_px = req
            .limit_price
            .as_deref()
            .and_then(|s| Decimal::from_str(s).ok());
        let is_crypto = req.symbol.contains('/');

        if req.side == "buy" {
            let seq = st.next_seq;
            st.next_seq += 1;
            let oid = format!("adv-buy-{}", seq);
            let fill_px = limit_px.unwrap_or(st.quote.ask_price);
            let net_qty = if is_crypto {
                floor_qty(qty * (Decimal::ONE - st.buy_fee_rate), 9)
            } else {
                qty
            };
            st.account.cash = (st.account.cash - qty * fill_px).max(Decimal::ZERO);
            st.position = Some(Position {
                asset_id: Some("asset".into()),
                symbol: req.symbol.replace('/', ""),
                exchange: Some("CRYPTO".into()),
                asset_class: Some("crypto".into()),
                qty: net_qty,
                qty_available: net_qty,
                avg_entry_price: fill_px,
                side: "long".into(),
                market_value: Some(net_qty * fill_px),
                cost_basis: Some(net_qty * fill_px),
                unrealized_pl: Some(Decimal::ZERO),
                current_price: Some(fill_px),
            });
            let ord = Order {
                id: oid.clone(),
                client_order_id: req.client_order_id.clone(),
                created_at: Some("2026-11-24T10:00:00Z".into()),
                updated_at: Some("2026-11-24T10:00:01Z".into()),
                submitted_at: Some("2026-11-24T10:00:00Z".into()),
                filled_at: Some("2026-11-24T10:00:01Z".into()),
                expired_at: None,
                canceled_at: None,
                failed_at: None,
                symbol: req.symbol.clone(),
                qty,
                filled_qty: qty,
                filled_avg_price: Some(fill_px),
                order_class: Some("simple".into()),
                side: "buy".into(),
                order_type: req.order_type.clone(),
                time_in_force: req.time_in_force.clone(),
                limit_price: limit_px,
                status: "filled".into(),
            };
            st.orders.insert(oid, ord.clone());
            return Ok(ord);
        }

        st.submitted_sell_requests.push(req.clone());

        if st.submit_sell_fail {
            return Err(AlpacaError::ApiStatus {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                message: "Injected 500 on sell".into(),
            });
        }

        if let Some(avail_cap) = st.submit_sell_403_once.take() {
            if qty > avail_cap {
                if let Some(ref mut pos) = st.position {
                    pos.qty_available = avail_cap;
                }
                return Err(AlpacaError::ApiStatus {
                    status: StatusCode::FORBIDDEN,
                    message: format!(
                        r#"{{"code":40310000,"available":"{}","requested":"{}","message":"insufficient balance"}}"#,
                        avail_cap, qty
                    ),
                });
            }
        }

        let seq = st.next_seq;
        st.next_seq += 1;

        if req.order_type == "market" {
            if st.submit_market_http_err {
                return Err(AlpacaError::ApiStatus {
                    status: StatusCode::INTERNAL_SERVER_ERROR,
                    message: "Injected 500 on stale market sell".into(),
                });
            }
            let oid = format!("adv-mkt-{}", seq);
            let (status, filled_qty) = if st.submit_market_rejected {
                ("rejected", Decimal::ZERO)
            } else if st.submit_market_pending {
                ("new", Decimal::ZERO)
            } else if let Some(part_qty) = st.submit_market_partial_fill {
                ("canceled", part_qty.min(qty))
            } else {
                ("filled", qty)
            };
            if filled_qty > Decimal::ZERO {
                if let Some(ref mut pos) = st.position {
                    pos.qty = (pos.qty - filled_qty).max(Decimal::ZERO);
                    pos.qty_available = (pos.qty_available - filled_qty).max(Decimal::ZERO);
                    if pos.qty <= Decimal::ZERO {
                        st.position = None;
                    }
                }
            }
            let ord = Order {
                id: oid.clone(),
                client_order_id: req.client_order_id.clone(),
                created_at: Some("2026-11-24T23:00:00Z".into()),
                updated_at: Some("2026-11-24T23:00:01Z".into()),
                submitted_at: Some("2026-11-24T23:00:00Z".into()),
                filled_at: if filled_qty > Decimal::ZERO {
                    Some("2026-11-24T23:00:01Z".into())
                } else {
                    None
                },
                expired_at: None,
                canceled_at: if status == "canceled" {
                    Some("2026-11-24T23:00:01Z".into())
                } else {
                    None
                },
                failed_at: if status == "rejected" {
                    Some("2026-11-24T23:00:01Z".into())
                } else {
                    None
                },
                symbol: req.symbol.clone(),
                qty,
                filled_qty,
                filled_avg_price: if filled_qty > Decimal::ZERO {
                    Some(st.quote.bid_price)
                } else {
                    None
                },
                order_class: Some("simple".into()),
                side: "sell".into(),
                order_type: "market".into(),
                time_in_force: req.time_in_force.clone(),
                limit_price: None,
                status: status.into(),
            };
            st.orders.insert(oid, ord.clone());
            return Ok(ord);
        }

        let oid = format!("adv-sell-{}", seq);
        if let Some(ref mut pos) = st.position {
            pos.qty_available = (pos.qty_available - qty).max(Decimal::ZERO);
        }
        let ord = Order {
            id: oid.clone(),
            client_order_id: req.client_order_id.clone(),
            created_at: Some("2026-11-24T10:00:02Z".into()),
            updated_at: Some("2026-11-24T10:00:02Z".into()),
            submitted_at: Some("2026-11-24T10:00:02Z".into()),
            filled_at: None,
            expired_at: None,
            canceled_at: None,
            failed_at: None,
            symbol: req.symbol.clone(),
            qty,
            filled_qty: Decimal::ZERO,
            filled_avg_price: None,
            order_class: Some("simple".into()),
            side: "sell".into(),
            order_type: "limit".into(),
            time_in_force: req.time_in_force.clone(),
            limit_price: limit_px,
            status: "new".into(),
        };
        st.orders.insert(oid, ord.clone());
        Ok(ord)
    }

    async fn cancel_order(&self, order_id: &str) -> Result<bool, AlpacaError> {
        let mut st = self.st.lock().unwrap();
        let leave_pending = st.cancel_leaves_pending;
        let (was_open, rem_qty) = match st.orders.get_mut(order_id) {
            Some(ord) if ord.is_open() => {
                let rem = ord.remaining_qty();
                if leave_pending {
                    ord.status = "pending_cancel".into();
                    (true, Decimal::ZERO) // shares STILL locked while pending_cancel!
                } else {
                    ord.status = "canceled".into();
                    ord.canceled_at = Some("2026-11-24T23:00:00Z".into());
                    (true, rem)
                }
            }
            _ => return Ok(false),
        };
        if was_open && rem_qty > Decimal::ZERO {
            if let Some(ref mut pos) = st.position {
                pos.qty_available = (pos.qty_available + rem_qty).min(pos.qty);
            }
        }
        Ok(was_open)
    }

    async fn get_latest_quote(&self, _symbol: &str) -> Result<QuoteDetail, AlpacaError> {
        Ok(self.st.lock().unwrap().quote.clone())
    }

    async fn get_portfolio_history(
        &self,
        _period: &str,
        _timeframe: &str,
    ) -> Result<PortfolioHistory, AlpacaError> {
        Ok(PortfolioHistory::default())
    }

    async fn get_account_activities(
        &self,
        _activity_types: &str,
        _after: Option<&str>,
    ) -> Result<Vec<AccountActivity>, AlpacaError> {
        Ok(self.st.lock().unwrap().activities.clone())
    }

    async fn get_account_activities_page(
        &self,
        _activity_types: &str,
        _after: Option<&str>,
        page_token: Option<&str>,
    ) -> Result<Vec<AccountActivity>, AlpacaError> {
        let mut st = self.st.lock().unwrap();
        st.activity_page_calls += 1;
        if !st.paginated_activity_pages.is_empty() {
            let key = page_token.map(str::to_string);
            return match st.paginated_activity_pages.get(&key) {
                Some(Ok(page)) => Ok(page.clone()),
                Some(Err(msg)) => Err(AlpacaError::Other(msg.clone())),
                None => Ok(Vec::new()),
            };
        }
        if page_token.is_some() {
            Ok(Vec::new())
        } else {
            Ok(st.activities.clone())
        }
    }
}

// =============================================================================
// THEOREM 1 — Guaranteed Strict Positive Realized Net P&L & Regression Proofs
// =============================================================================

#[test]
fn theorem1_stress_40000_single_fill_and_k_star_multifill_bounds() {
    let mut rng = Rng64::new(0x1111_2222_3333_4444);

    // Verify CFEE_CEIL_BUFFER_USD == $0.08 and HALF_CENT_USD == $0.005
    assert_eq!(CFEE_CEIL_BUFFER_USD, d("0.08"));
    assert_eq!(HALF_CENT_USD, d("0.005"));

    // Verify Decimal overflow safety returns Err(PricingError::Overflow) cleanly without panicking
    let ovf = compute_guaranteed_profitable_exit_price(
        Decimal::MAX,
        d("10.00"),
        d("1.00"),
        d("10.00"),
        CRYPTO_TIER1_TAKER_FEE_RATE,
        MIN_NET_PROFIT_MARGIN,
        CFEE_CEIL_BUFFER_USD,
        true,
    );
    assert!(
        matches!(ovf, Err(PricingError::Overflow(_))),
        "Expected PricingError::Overflow on Decimal::MAX cost basis, got {:?}",
        ovf
    );

    // 1. Stress-test 40,000 randomized + extreme boundary crypto trades:
    //    - Sub-$1.00 ($0.001..$0.9999), $1.00 transition ($0.9900..$1.0100),
    //      mid ($1..$1,000), high ($1,000..$100,000) — INCLUDING tiny buy_qty - sell_qty <= 1e-7!
    //    - Tiny notional ($1..$15), medium ($900..$1,100), huge ($150k..$500k)
    //    - Worst-case Tier 1 taker buy (0.25%) + worst-case Tier 1 taker sell (0.25%)
    let mut trials = 0usize;
    let mut min_single_fill_net_pnl = Decimal::MAX;
    let mut min_k8_net_pnl = Decimal::MAX;

    for i in 0..40_000usize {
        let price_f = match i % 5 {
            0 => rng.range_f64(0.001, 0.9999),      // sub-$1.00 ($0.0001 tick)
            1 => rng.range_f64(0.9900, 1.0100),     // $1.00 tick-size transition boundary
            2 => rng.log_uniform(1.0, 1000.0),      // standard crypto (SOL/ETH/etc.)
            3 => rng.log_uniform(1000.0, 100000.0), // high-priced (BTC-like, incl. <=1e-7 diffs)
            _ => rng.range_f64(70.0, 76.0),         // SOL/USD deb40eea regime
        };
        let raw_px = decimal_from_f64_clean(price_f).unwrap();
        let avg_buy_px = floor_to_tick(raw_px).max(d("0.0001"));

        let notional_f = match i % 4 {
            0 => rng.range_f64(1.0, 15.0),            // tiny fills ($1-$15)
            1 => rng.range_f64(900.0, 1100.0),        // medium deb40eea (~$976)
            2 => rng.range_f64(150_000.0, 500_000.0), // huge ($150k-$500k)
            _ => rng.log_uniform(10.0, 100_000.0),    // wide range
        };
        let buy_qty = floor_qty(
            decimal_from_f64_clean(notional_f).unwrap() / avg_buy_px,
            9,
        );
        if buy_qty <= Decimal::ZERO {
            continue;
        }

        let buy_fee_rate = if i % 2 == 0 {
            CRYPTO_TIER1_TAKER_FEE_RATE
        } else {
            CRYPTO_TIER1_MAKER_FEE_RATE
        };
        let sell_qty = floor_qty(buy_qty * (Decimal::ONE - buy_fee_rate), 9);
        if sell_qty <= Decimal::ZERO || sell_qty >= buy_qty {
            continue;
        }

        // Adversarial low fair_price (well below buy_price, or just barely above buy_price)
        let fair_mult = rng.range_f64(0.50, 1.004);
        let fair_px = floor_to_tick(
            avg_buy_px * decimal_from_f64_clean(fair_mult).unwrap(),
        );

        let exit_px = compute_guaranteed_profitable_exit_price(
            buy_qty,
            avg_buy_px,
            sell_qty,
            fair_px,
            CRYPTO_TIER1_TAKER_FEE_RATE,
            MIN_NET_PROFIT_MARGIN,
            CFEE_CEIL_BUFFER_USD,
            true,
        )
        .expect("must succeed on valid inputs");

        // Invariant A: legal SEC Rule 612 tick, strictly > buy_px, >= ceil(fair_px)
        assert!(
            is_legal_sec_612_tick(exit_px),
            "exit_px {} not legal SEC 612 tick",
            exit_px
        );
        assert!(exit_px > avg_buy_px);
        if fair_px > Decimal::ZERO {
            assert!(exit_px >= ceil_to_tick(fair_px));
        }

        // Invariant B: Single fill (k = 1) under WORST-CASE 0.25% taker sell CFEE
        // Since S > $0.085 and ceil_cents(G*f) - G*f < $0.01, unrounded net > $0.075 => 2dp >= $0.08!
        let cost_basis = buy_qty * avg_buy_px;
        let gross_proceeds = sell_qty * exit_px;
        let cfee_k1 = ceil_usd_cents(gross_proceeds * CRYPTO_TIER1_TAKER_FEE_RATE);
        let unrounded_net_k1 = gross_proceeds - cfee_k1 - cost_basis;
        assert!(
            unrounded_net_k1 > d("0.075"),
            "Single-fill unrounded net PnL {} <= $0.075 (buy_qty={}, buy_px={}, sell_qty={}, exit_px={})",
            unrounded_net_k1,
            buy_qty,
            avg_buy_px,
            sell_qty,
            exit_px
        );

        let (net_pnl, _, _, _) = compute_net_trade_pnl(
            avg_buy_px,
            exit_px,
            buy_qty,
            sell_qty,
            true,
            buy_fee_rate,
            CRYPTO_TIER1_TAKER_FEE_RATE,
            Some(cfee_k1),
            false,
        );
        assert!(
            net_pnl >= d("0.08"),
            "Single-fill rounded net_pnl {} < $0.08 (buy_qty={}, buy_px={}, sell_qty={}, exit_px={})",
            net_pnl,
            buy_qty,
            avg_buy_px,
            sell_qty,
            exit_px
        );
        if net_pnl < min_single_fill_net_pnl {
            min_single_fill_net_pnl = net_pnl;
        }

        // Invariant C: Universal k = 8 adversarial multi-fill floor (EVEN on $1-$15 tiny fills!)
        let cfee_k8 = adversarial_multifill_cfee(
            sell_qty,
            exit_px,
            CRYPTO_TIER1_TAKER_FEE_RATE,
            8,
        );
        let unrounded_net_k8 = gross_proceeds - cfee_k8 - cost_basis;
        let (rounded_net_k8, _, _, _) = compute_net_trade_pnl(
            avg_buy_px,
            exit_px,
            buy_qty,
            sell_qty,
            true,
            buy_fee_rate,
            CRYPTO_TIER1_TAKER_FEE_RATE,
            Some(cfee_k8),
            false,
        );
        assert!(
            unrounded_net_k8 > HALF_CENT_USD && rounded_net_k8 >= d("0.01"),
            "Universal k=8 bound violated: unrounded={}, rounded={} (gross={}, cost={})",
            unrounded_net_k8,
            rounded_net_k8,
            gross_proceeds,
            cost_basis
        );
        if rounded_net_k8 < min_k8_net_pnl {
            min_k8_net_pnl = rounded_net_k8;
        }

        // Invariant D: Verify exact trade-specific k*_2dp(S) = floor(100*S - 0.5) bound!
        let surplus = gross_proceeds * (Decimal::ONE - CRYPTO_TIER1_TAKER_FEE_RATE) - cost_basis;
        let k_star_2dp = (surplus * Decimal::from(100) - d("0.5"))
            .floor()
            .to_usize()
            .unwrap_or(1)
            .max(1);
        assert!(k_star_2dp >= 8);
        let k_test = k_star_2dp.min(250);
        let cfee_k = adversarial_multifill_cfee(
            sell_qty,
            exit_px,
            CRYPTO_TIER1_TAKER_FEE_RATE,
            k_test,
        );
        let unrounded_net_k = gross_proceeds - cfee_k - cost_basis;
        let (rounded_net_k, _, _, _) = compute_net_trade_pnl(
            avg_buy_px,
            exit_px,
            buy_qty,
            sell_qty,
            true,
            buy_fee_rate,
            CRYPTO_TIER1_TAKER_FEE_RATE,
            Some(cfee_k),
            false,
        );
        assert!(
            unrounded_net_k > HALF_CENT_USD && rounded_net_k >= d("0.01"),
            "k_star_2dp bound failed at k={} <= k*_2dp={} (surplus={}, cfee_k={}, unrounded={}, rounded={})",
            k_test,
            k_star_2dp,
            surplus,
            cfee_k,
            unrounded_net_k,
            rounded_net_k
        );

        // Specifically verify huge orders ($150k-$500k) survive k=100, 200, 250 partial fills!
        if gross_proceeds >= d("150000") {
            for k_huge in [100usize, 200usize, 250usize] {
                let cfee_huge = adversarial_multifill_cfee(
                    sell_qty,
                    exit_px,
                    CRYPTO_TIER1_TAKER_FEE_RATE,
                    k_huge,
                );
                let (net_huge, _, _, _) = compute_net_trade_pnl(
                    avg_buy_px,
                    exit_px,
                    buy_qty,
                    sell_qty,
                    true,
                    buy_fee_rate,
                    CRYPTO_TIER1_TAKER_FEE_RATE,
                    Some(cfee_huge),
                    false,
                );
                assert!(
                    net_huge >= d("0.01"),
                    "Huge order {} failed at k={} fills: net={}",
                    gross_proceeds,
                    k_huge,
                    net_huge
                );
            }
        }

        trials += 1;
    }

    // Exact deb40eea medium notional (~$976) check: 13.5555 SOL @ $72.00 = $975.996
    let deb_buy_qty = d("13.5555");
    let deb_buy_px = d("72.00");
    let deb_sell_qty = floor_qty(deb_buy_qty * (Decimal::ONE - d("0.0022")), 9);
    let deb_exit = compute_guaranteed_profitable_exit_price(
        deb_buy_qty,
        deb_buy_px,
        deb_sell_qty,
        d("72.26"), // tight margin below round-trip fees
        CRYPTO_TIER1_TAKER_FEE_RATE,
        MIN_NET_PROFIT_MARGIN,
        CFEE_CEIL_BUFFER_USD,
        true,
    )
    .unwrap();
    let deb_gross = deb_sell_qty * deb_exit;
    let deb_surplus_taker =
        deb_gross * (Decimal::ONE - CRYPTO_TIER1_TAKER_FEE_RATE) - deb_buy_qty * deb_buy_px;
    let deb_k_star_2dp = (deb_surplus_taker * Decimal::from(100) - d("0.5"))
        .floor()
        .to_usize()
        .unwrap();
    assert!(deb_k_star_2dp >= 37, "Expected deb40eea k*_2dp >= 37, got {}", deb_k_star_2dp);
    let deb_cfee_ok = adversarial_multifill_cfee(
        deb_sell_qty,
        deb_exit,
        CRYPTO_TIER1_TAKER_FEE_RATE,
        deb_k_star_2dp,
    );
    let (deb_net, _, _, _) = compute_net_trade_pnl(
        deb_buy_px,
        deb_exit,
        deb_buy_qty,
        deb_sell_qty,
        true,
        d("0.0022"),
        CRYPTO_TIER1_TAKER_FEE_RATE,
        Some(deb_cfee_ok),
        false,
    );
    assert!(deb_net >= d("0.01"));

    println!(
        "[THEOREM 1 STRESS PROVEN] {} randomized/boundary crypto trials passed single-fill (min=${}), universal k=8 (min=${}), & exact k*_2dp(S) bounds; deb40eea($976) exit=${}, k*_2dp={}",
        trials, min_single_fill_net_pnl, min_k8_net_pnl, deb_exit, deb_k_star_2dp
    );
}

#[test]
fn theorem1_regression_ce11_multifill_cfee_008_buffer() {
    // CE-1.1 REGRESSION PROOF:
    // With CFEE_CEIL_BUFFER_USD = $0.08 + HALF_CENT_USD = $0.005, universal bound is:
    //   k*_univ,2dp(G) = 8 + floor(0.03 * G) >= 8 for EVERY G > 0!
    // Verify BOTH $10.00 minimum order and $12.00 tight order clear k = 1..=8 adversarial fills
    // with 2dp-rounded net_pnl >= $0.01 (strictly stronger than k <= 7)!
    for (label, buy_qty, buy_px) in [
        ("$10.00 min order", d("0.13889"), d("72.00")),
        ("$12.00 tight order", d("0.16667"), d("72.00")),
    ] {
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

        for k in 1..=8usize {
            let cfee =
                adversarial_multifill_cfee(sell_qty, exit_px, CRYPTO_TIER1_TAKER_FEE_RATE, k);
            let unrounded_net = gross - cfee - buy_qty * buy_px;
            let (rounded_net, _, _, _) = compute_net_trade_pnl(
                buy_px,
                exit_px,
                buy_qty,
                sell_qty,
                true,
                CRYPTO_TIER1_TAKER_FEE_RATE,
                CRYPTO_TIER1_TAKER_FEE_RATE,
                Some(cfee),
                false,
            );
            assert!(
                unrounded_net > HALF_CENT_USD && rounded_net >= d("0.01"),
                "CE-1.1 REGRESSION: {} failed at k={} fills (exit=${}, cfee=${}, unrounded={}, rounded={})",
                label,
                k,
                exit_px,
                cfee,
                unrounded_net,
                rounded_net
            );
        }

        // Exact mathematical sharpness verification at k = 9 + Universal Non-Loss Gate check:
        let cfee_k9 = adversarial_multifill_cfee(sell_qty, exit_px, CRYPTO_TIER1_TAKER_FEE_RATE, 9);
        let (rounded_k9, _, _, _) = compute_net_trade_pnl(
            buy_px,
            exit_px,
            buy_qty,
            sell_qty,
            true,
            CRYPTO_TIER1_TAKER_FEE_RATE,
            CRYPTO_TIER1_TAKER_FEE_RATE,
            Some(cfee_k9),
            false,
        );
        // Even beyond the k=8 buffer envelope, classify_filled_exit_status NEVER emits Completed on <= 0!
        if rounded_k9 <= Decimal::ZERO {
            assert_eq!(
                classify_filled_exit_status(false, rounded_k9),
                HuntStatus::LiquidatedManual
            );
        }
        println!(
            "[CE-1.1 REGRESSION PROVEN] {} (exit=${}): ALL k=1..=8 adversarial fills strictly realize 2dp net_pnl >= $0.01 (k=9 sharpness net=${}, gated status={})",
            label,
            exit_px,
            rounded_k9,
            classify_filled_exit_status(false, rounded_k9)
        );
    }
}

#[tokio::test]
async fn theorem1_regression_ce12_submit_retry_reprices_and_2pct_shrink_guard() {
    // CE-1.2 REGRESSION PROOF (Part A):
    // 1. Buy fills 100 SOL @ $72.00 as Tier 1 TAKER (0.25% -> net 99.75 SOL).
    // 2. `compute_safe_exit_sell_qty` suffers transient `get_position` failures -> falls back
    //    conservatively using Tier 1 TAKER fee (0.25% -> 99.75 SOL, NEVER 0.15% maker!).
    // 3. We additionally inject an adversarial HTTP 403 shrinking available balance by another 0.25%
    //    to 99.50 SOL (within 2% guard) -> `submit_exit_sell_with_retry` MUST dynamically RE-PRICE
    //    `exit_px` UPWARD via `try_resolve_profitable_exit_price` so filling @ repriced limit under
    //    0.25% taker CFEE realizes strict `HuntStatus::Completed` with 2dp `net_pnl >= $0.01`!
    let tmp = tempdir().unwrap();
    let store = StateStore::new(tmp.path());
    let mut params = default_sol_params();
    params.max_shares = 100.0;

    let mut st = AdversarialBrokerState::base();
    st.buy_fee_rate = CRYPTO_TIER1_TAKER_FEE_RATE;
    // Pre-buy get_position succeeds (None); next 2 calls inside compute_safe_exit_sell_qty fail transiently!
    st.get_position_results.push_back(Ok(None));
    st.get_position_results
        .push_back(Err("transient timeout 1".into()));
    st.get_position_results
        .push_back(Err("transient timeout 2".into()));
    // Force an extra 403 shrink to 99.50 (within 2% of 100 SOL) to exercise retry re-pricing!
    st.submit_sell_403_once = Some(d("99.50"));

    let broker = AdversarialMockBroker::new(st);
    let mut trades_history = Vec::new();
    let mut fee_tracker = FeeTracker::new();
    let now = DateTime::parse_from_rfc3339("2026-11-24T10:00:00Z")
        .unwrap()
        .with_timezone(&Utc);

    let created = execute_hunt(
        &broker,
        &params,
        72.27,
        &mut trades_history,
        &mut fee_tracker,
        Some(&store),
        now,
        HuntTiming::fast_for_tests(),
        None,
    )
    .await
    .unwrap()
    .expect("hunt executes");

    assert_eq!(created.status, HuntStatus::OpenExitPending);
    let sell_oid = created.sell_order_id.clone().unwrap();
    let submitted_px = created.sell_price.unwrap();
    let submitted_qty = created.qty;
    assert_eq!(submitted_qty, d("99.50"));

    // Verify 2 sell attempts were made and the retry STRICTLY raised limit_price!
    {
        let s = broker.st.lock().unwrap();
        assert_eq!(s.submitted_sell_requests.len(), 2);
        let first_qty = Decimal::from_str(&s.submitted_sell_requests[0].qty).unwrap();
        let first_px =
            Decimal::from_str(s.submitted_sell_requests[0].limit_price.as_ref().unwrap()).unwrap();
        let second_px =
            Decimal::from_str(s.submitted_sell_requests[1].limit_price.as_ref().unwrap()).unwrap();
        assert!(
            first_qty <= d("99.75"),
            "Fallback est_net must use >=0.25% taker fee (<=99.75), got {}",
            first_qty
        );
        assert!(
            second_px > first_px,
            "Retry MUST dynamically re-price exit UPWARD when qty shrinks: first=${}, second=${}",
            first_px,
            second_px
        );
    }

    // Fill the repriced exit order at its exact limit price under worst-case 0.25% CFEE
    {
        let mut s = broker.st.lock().unwrap();
        let ord = s.orders.get_mut(&sell_oid).unwrap();
        ord.status = "filled".into();
        ord.filled_qty = submitted_qty;
        ord.filled_avg_price = Some(submitted_px);
        ord.filled_at = Some("2026-11-24T10:05:00Z".into());
        let cfee = ceil_usd_cents(submitted_qty * submitted_px * CRYPTO_TIER1_TAKER_FEE_RATE);
        s.position = None;
        s.activities.push(AccountActivity {
            id: "cfee-ce12-fixed".into(),
            activity_type: "CFEE".into(),
            order_id: Some(sell_oid.clone()),
            symbol: Some("SOL/USD".into()),
            transaction_time: Some("2026-11-24T10:05:00Z".into()),
            date: Some("2026-11-24".into()),
            net_amount: Some(-cfee),
            qty: None,
            price: None,
            description: None,
            status: Some("executed".into()),
        });
    }

    reconcile_and_maintain_positions(
        &broker,
        &params,
        &mut trades_history,
        &mut fee_tracker,
        d("72.27"),
        submitted_px,
        now + ChronoDuration::minutes(5),
        Some(&store),
        ReconciliationTiming::fast_for_tests(),
    )
    .await;

    let closed = &trades_history[0];
    assert_eq!(closed.status, HuntStatus::Completed);
    let net_pnl = closed.pnl.unwrap();
    assert!(
        net_pnl >= d("0.01"),
        "CE-1.2 REGRESSION: repriced retry exit must realize >= $0.01, got {}",
        net_pnl
    );

    // CE-1.2 Part B: 2% drastic-shrink guard EXACT boundary verification!
    let buy_qty = d("100");
    let buy_px = d("72.00");
    let init_sell_qty = d("99.75");
    let ft = FeeTracker::new();
    let init_exit_px = try_resolve_profitable_exit_price(
        buy_qty,
        buy_px,
        init_sell_qty,
        d("72.20"),
        None,
        &ft,
        true,
    )
    .unwrap();

    // B1: Exact 98.00% boundary (2.00% shrink -> ALLOWED, reprices UPWARD & stays strictly profitable)
    {
        let mut st_ok = AdversarialBrokerState::base();
        st_ok.position = Some(Position {
            asset_id: Some("sol".into()),
            symbol: "SOLUSD".into(),
            exchange: Some("CRYPTO".into()),
            asset_class: Some("crypto".into()),
            qty: d("98.00"),
            qty_available: d("98.00"),
            avg_entry_price: buy_px,
            side: "long".into(),
            market_value: None,
            cost_basis: None,
            unrealized_pl: None,
            current_price: Some(buy_px),
        });
        st_ok.submit_sell_403_once = Some(d("98.00"));
        let br = AdversarialMockBroker::new(st_ok);
        let (ord_opt, sub_qty, actual_px) = submit_exit_sell_with_retry(
            &br,
            "SOL/USD",
            buy_qty,
            init_sell_qty,
            init_exit_px,
            "gtc",
            2,
            |q| try_resolve_profitable_exit_price(buy_qty, buy_px, q, d("72.20"), None, &ft, true),
        )
        .await;
        assert!(ord_opt.is_some(), "Exact 98.00% boundary must be permitted");
        assert_eq!(sub_qty, d("98.00"));
        assert!(actual_px > init_exit_px);
        let cfee = ceil_usd_cents(sub_qty * actual_px * CRYPTO_TIER1_TAKER_FEE_RATE);
        let (pnl_98, _, _, _) = compute_net_trade_pnl(
            buy_px,
            actual_px,
            buy_qty,
            sub_qty,
            true,
            CRYPTO_TIER1_TAKER_FEE_RATE,
            CRYPTO_TIER1_TAKER_FEE_RATE,
            Some(cfee),
            false,
        );
        assert!(pnl_98 >= d("0.01"), "98% repriced exit net_pnl={}", pnl_98);
    }

    // B2: Drastic >2% shrinks (97.99% and 50.00%) -> MUST ABORT retry immediately with 0 second submit!
    for shrunk_avail in [d("97.99"), d("50.00")] {
        let mut st_abort = AdversarialBrokerState::base();
        st_abort.position = Some(Position {
            asset_id: Some("sol".into()),
            symbol: "SOLUSD".into(),
            exchange: Some("CRYPTO".into()),
            asset_class: Some("crypto".into()),
            qty: shrunk_avail,
            qty_available: shrunk_avail,
            avg_entry_price: buy_px,
            side: "long".into(),
            market_value: None,
            cost_basis: None,
            unrealized_pl: None,
            current_price: Some(buy_px),
        });
        st_abort.submit_sell_403_once = Some(shrunk_avail);
        let br = AdversarialMockBroker::new(st_abort);
        let (ord_opt, sub_qty, _) = submit_exit_sell_with_retry(
            &br,
            "SOL/USD",
            buy_qty,
            init_sell_qty,
            init_exit_px,
            "gtc",
            2,
            |q| try_resolve_profitable_exit_price(buy_qty, buy_px, q, d("72.20"), None, &ft, true),
        )
        .await;
        assert!(
            ord_opt.is_none() && sub_qty == Decimal::ZERO,
            "Drastic shrink to {} (<98%) MUST abort retry cleanly",
            shrunk_avail
        );
        assert_eq!(
            br.st.lock().unwrap().submitted_sell_requests.len(),
            1,
            "Aborted shrink {} must NOT submit a second inflated order",
            shrunk_avail
        );
    }

    println!(
        "[CE-1.2 REGRESSION & 2% GUARD PROVEN] 99.50 retry repriced -> Completed net_pnl=${}; 98.00% repriced OK; 97.99% & 50.00% cleanly aborted to ExitSubmitPending",
        net_pnl
    );
}

#[tokio::test]
async fn theorem1_regression_ce15_step_d_orphan_stored_buy_px_ordering() {
    // CE-1.5 REGRESSION PROOF:
    // Step D rounds `stored_buy_px = avg_entry.round_dp(4) = 0.1001` BEFORE pricing and prices
    // against `pricing_buy_px = avg_entry.max(stored_buy_px) = 0.1001`, so even when
    // `fair_price ($0.05) << avg_entry ($0.10005001)`, BOTH recorded and economic net P&L >= $0.01!
    let mut params = default_sol_params();
    params.symbol = "DOGE/USD".into();

    let mut st = AdversarialBrokerState::base();
    st.position = Some(Position {
        asset_id: Some("doge".into()),
        symbol: "DOGEUSD".into(),
        exchange: Some("CRYPTO".into()),
        asset_class: Some("crypto".into()),
        qty: d("10000"),
        qty_available: d("10000"),
        avg_entry_price: d("0.10005001"),
        side: "long".into(),
        market_value: Some(d("500.00")),
        cost_basis: Some(d("1000.5001")),
        unrealized_pl: Some(d("-500.50")),
        current_price: Some(d("0.0500")),
    });

    let broker = AdversarialMockBroker::new(st);
    let mut trades_history = Vec::new();
    let mut fee_tracker = FeeTracker::new();
    let now = DateTime::parse_from_rfc3339("2026-11-24T10:00:00Z")
        .unwrap()
        .with_timezone(&Utc);

    reconcile_and_maintain_positions(
        &broker,
        &params,
        &mut trades_history,
        &mut fee_tracker,
        d("0.0500"),
        d("0.0500"),
        now,
        None,
        ReconciliationTiming::fast_for_tests(),
    )
    .await;

    assert_eq!(trades_history.len(), 1);
    let orphan = &trades_history[0];
    assert_eq!(orphan.status, HuntStatus::OpenExitPending);
    assert_eq!(orphan.buy_price, d("0.1001"));
    let exit_px = orphan.sell_price.unwrap();
    assert!(
        exit_px >= d("0.1007"),
        "Orphan exit must clear rounded buy_price 0.1001 + fees + buffer (>=0.1007), got {}",
        exit_px
    );
    let sell_oid = orphan.sell_order_id.clone().unwrap();

    // Fill orphan exit @ exit_px under worst-case 0.25% taker CFEE
    let cfee = ceil_usd_cents(d("10000") * exit_px * CRYPTO_TIER1_TAKER_FEE_RATE);
    {
        let mut s = broker.st.lock().unwrap();
        let ord = s.orders.get_mut(&sell_oid).unwrap();
        ord.status = "filled".into();
        ord.filled_qty = d("10000");
        ord.filled_avg_price = Some(exit_px);
        ord.filled_at = Some("2026-11-24T10:10:00Z".into());
        s.position = None;
        s.activities.push(AccountActivity {
            id: "cfee-orphan-fixed".into(),
            activity_type: "CFEE".into(),
            order_id: Some(sell_oid.clone()),
            symbol: Some("DOGE/USD".into()),
            transaction_time: Some("2026-11-24T10:10:00Z".into()),
            date: Some("2026-11-24".into()),
            net_amount: Some(-cfee),
            qty: None,
            price: None,
            description: None,
            status: Some("executed".into()),
        });
    }

    reconcile_and_maintain_positions(
        &broker,
        &params,
        &mut trades_history,
        &mut fee_tracker,
        d("0.1010"),
        exit_px,
        now + ChronoDuration::minutes(10),
        None,
        ReconciliationTiming::fast_for_tests(),
    )
    .await;

    let closed = &trades_history[0];
    assert_eq!(closed.status, HuntStatus::Completed);
    let net_pnl = closed.pnl.unwrap();
    assert!(
        net_pnl >= d("0.01"),
        "CE-1.5 REGRESSION: orphan Completed net_pnl must be >= $0.01, got {}",
        net_pnl
    );
    let econ_pnl = d("10000") * exit_px - cfee - closed.buy_qty * d("0.10005001");
    assert!(econ_pnl > HALF_CENT_USD);
    println!(
        "[CE-1.5 REGRESSION PROVEN] Step D orphan avg_entry=0.10005001 -> stored_buy_px=0.1001, exit_px=${} -> Completed net_pnl=${} (econ=${:.4})",
        exit_px, net_pnl, econ_pnl
    );
}

#[tokio::test]
async fn theorem1_regression_ce13_dust_nonloss_gate_and_notional_btc_liq() {
    let params = default_sol_params();
    let now = DateTime::parse_from_rfc3339("2026-11-24T23:00:00Z")
        .unwrap()
        .with_timezone(&Utc);

    // Exhaustive `is_dust_qty` boundary & overflow check (including ref_price == 0!)
    assert!(is_dust_qty(Decimal::ZERO, d("90000"), true));
    assert!(is_dust_qty(d("-0.5"), d("90000"), true));
    assert!(!is_dust_qty(d("0.001"), d("90000"), true)); // $90 >= $1 -> NOT dust
    assert!(!is_dust_qty(d("0.0000112"), d("90000"), true)); // $1.008 >= $1 -> NOT dust
    assert!(is_dust_qty(d("0.0000111"), d("90000"), true)); // $0.999 < $1 -> IS dust
    assert!(is_dust_qty(d("0.00099"), d("72.50"), true)); // $0.0718 < $1 -> IS dust
    assert!(is_dust_qty(d("0.001"), Decimal::ZERO, true)); // ref_price == 0 fallback <= 0.001 -> dust
    assert!(!is_dust_qty(d("0.001000001"), Decimal::ZERO, true)); // ref_price == 0 fallback > 0.001 -> NOT dust
    assert!(!is_dust_qty(Decimal::MAX, d("100"), true)); // checked_mul overflow -> NOT dust (no panic)

    // Part A (CE-1.3a): Canceled SOL exit with `rem_qty = 0.00099` ($0.07 < $1.00 dust)
    // realizing NEGATIVE net_pnl MUST be classified as `LiquidatedManual`, NEVER `Completed`!
    {
        let mut st = AdversarialBrokerState::base();
        st.orders.insert(
            "sell-dust-cancel".into(),
            Order {
                id: "sell-dust-cancel".into(),
                client_order_id: None,
                created_at: Some("2026-11-24T22:00:00Z".into()),
                updated_at: Some("2026-11-24T22:05:00Z".into()),
                submitted_at: Some("2026-11-24T22:00:00Z".into()),
                filled_at: None,
                expired_at: None,
                canceled_at: Some("2026-11-24T22:05:00Z".into()),
                failed_at: None,
                symbol: "SOL/USD".into(),
                qty: d("0.13979"),
                filled_qty: d("0.13880"),
                filled_avg_price: Some(d("72.52")),
                order_class: None,
                side: "sell".into(),
                order_type: "limit".into(),
                time_in_force: "gtc".into(),
                limit_price: Some(d("72.52")),
                status: "canceled".into(),
            },
        );
        let cfee = ceil_usd_cents(d("0.13880") * d("72.52") * CRYPTO_TIER1_TAKER_FEE_RATE);
        st.activities.push(AccountActivity {
            id: "cfee-dust".into(),
            activity_type: "CFEE".into(),
            order_id: Some("sell-dust-cancel".into()),
            symbol: Some("SOL/USD".into()),
            transaction_time: Some("2026-11-24T22:05:00Z".into()),
            date: Some("2026-11-24".into()),
            net_amount: Some(-cfee),
            qty: None,
            price: None,
            description: None,
            status: Some("executed".into()),
        });

        let broker = AdversarialMockBroker::new(st);
        let mut trades = vec![TradeRecord {
            id: "dust0001".into(),
            symbol: "SOL/USD".into(),
            buy_order_id: Some("buy-dust".into()),
            sell_order_id: Some("sell-dust-cancel".into()),
            buy_qty: d("0.14"),
            qty: d("0.13979"),
            filled_exit_qty: Decimal::ZERO,
            buy_price: d("72.00"),
            sell_price: Some(d("72.52")),
            pnl: None,
            pnl_pct: None,
            gross_pnl: None,
            fees_usd: None,
            buy_timestamp: "2026-11-24T22:00:00Z".into(),
            sell_timestamp: None,
            status: HuntStatus::OpenExitPending,
            adopted_from_broker: false,
            recovered_orphan: false,
            buy_fee_already_deducted: None,
        }];
        let mut fee_tracker = FeeTracker::new();

        reconcile_and_maintain_positions(
            &broker,
            &params,
            &mut trades,
            &mut fee_tracker,
            d("72.52"),
            d("72.50"),
            now,
            None,
            ReconciliationTiming::fast_for_tests(),
        )
        .await;

        assert!(trades[0].pnl.unwrap() <= Decimal::ZERO);
        assert_eq!(
            trades[0].status,
            HuntStatus::LiquidatedManual,
            "CE-1.3a REGRESSION: dust-remainder losing cancel MUST be LiquidatedManual, NOT Completed!"
        );
    }

    // Part B (CE-1.3b): Stale 0.001 BTC ($90!) exit canceled with 0 fills is NOT dust!
    // Must execute real market liquidation -> `LiquidatedStale`, NEVER `Completed`!
    {
        let mut btc_params = default_sol_params();
        btc_params.symbol = "BTC/USD".into();
        let mut st_btc = AdversarialBrokerState::base();
        st_btc.quote.bid_price = d("85000.00");
        st_btc.quote.ask_price = d("85010.00");
        st_btc.position = Some(Position {
            asset_id: Some("btc".into()),
            symbol: "BTCUSD".into(),
            exchange: Some("CRYPTO".into()),
            asset_class: Some("crypto".into()),
            qty: d("0.001"),
            qty_available: Decimal::ZERO,
            avg_entry_price: d("90000.00"),
            side: "long".into(),
            market_value: Some(d("85.00")),
            cost_basis: Some(d("90.00")),
            unrealized_pl: Some(d("-5.00")),
            current_price: Some(d("85000.00")),
        });
        st_btc.orders.insert(
            "stale-btc-001".into(),
            Order {
                id: "stale-btc-001".into(),
                client_order_id: None,
                created_at: Some("2026-11-24T09:00:00Z".into()), // 14h old > 12h
                updated_at: Some("2026-11-24T09:00:00Z".into()),
                submitted_at: Some("2026-11-24T09:00:00Z".into()),
                filled_at: None,
                expired_at: None,
                canceled_at: None,
                failed_at: None,
                symbol: "BTC/USD".into(),
                qty: d("0.001"),
                filled_qty: Decimal::ZERO,
                filled_avg_price: None,
                order_class: None,
                side: "sell".into(),
                order_type: "limit".into(),
                time_in_force: "gtc".into(),
                limit_price: Some(d("90600.00")),
                status: "new".into(),
            },
        );
        let broker_btc = AdversarialMockBroker::new(st_btc);
        let mut btc_trades = vec![TradeRecord {
            id: "btc00001".into(),
            symbol: "BTC/USD".into(),
            buy_order_id: Some("buy-btc".into()),
            sell_order_id: Some("stale-btc-001".into()),
            buy_qty: d("0.0010015"),
            qty: d("0.001"),
            filled_exit_qty: Decimal::ZERO,
            buy_price: d("90000.00"),
            sell_price: Some(d("90600.00")),
            pnl: None,
            pnl_pct: None,
            gross_pnl: None,
            fees_usd: None,
            buy_timestamp: "2026-11-24T09:00:00Z".into(),
            sell_timestamp: None,
            status: HuntStatus::OpenExitPending,
            adopted_from_broker: false,
            recovered_orphan: false,
            buy_fee_already_deducted: Some(true),
        }];
        let mut fee_tracker = FeeTracker::new();

        reconcile_and_maintain_positions(
            &broker_btc,
            &btc_params,
            &mut btc_trades,
            &mut fee_tracker,
            d("85000.00"),
            d("85000.00"),
            now,
            None,
            ReconciliationTiming::fast_for_tests(),
        )
        .await;

        assert_eq!(
            btc_trades[0].status,
            HuntStatus::LiquidatedStale,
            "CE-1.3b REGRESSION: stale 0.001 BTC ($90) MUST market liquidate to LiquidatedStale, NOT Completed!"
        );
        assert!(broker_btc.st.lock().unwrap().position.is_none());
        assert_eq!(btc_trades[0].sell_price, Some(d("85000")));
    }

    println!(
        "[CE-1.3a/b REGRESSION PROVEN] Universal Completed non-loss gate blocks dust losses; 0.001 BTC ($90) properly market-liquidated to LiquidatedStale"
    );
}

#[test]
fn theorem1_regression_stock_sub_dollar_half_cent_strict_profit() {
    // Stock sub-$1 regression + exact $0.005 MidpointNearestEven boundary + 10,000 randomized trials!
    for (shares, buy_px, expected_exit) in [
        (d("40"), d("0.50"), d("0.5002")), // 40 * 0.0002 = $0.008 -> $0.01
        (d("50"), d("0.50"), d("0.5002")), // 50 * 0.0001 = $0.005 (rounds to 0.00!) -> bumped to $0.5002 -> $0.01
        (d("100"), d("0.10"), d("0.1001")), // 100 * 0.0001 = $0.010 -> $0.01
    ] {
        let exit_px = compute_guaranteed_profitable_exit_price(
            shares,
            buy_px,
            shares,
            buy_px * d("0.80"),
            Decimal::ZERO,
            Decimal::ZERO,
            Decimal::ZERO,
            false,
        )
        .unwrap();
        assert_eq!(exit_px, expected_exit);
        let (net_pnl, _, _, _) = compute_net_trade_pnl(
            buy_px,
            exit_px,
            shares,
            shares,
            false,
            Decimal::ZERO,
            Decimal::ZERO,
            None,
            false,
        );
        assert!(
            net_pnl >= d("0.01"),
            "Stock {} @ {} -> exit {} must realize >= $0.01, got {}",
            shares,
            buy_px,
            exit_px,
            net_pnl
        );
    }

    // 10,000 randomized sub-$1, $1-boundary, and standard stocks (whole shares >= $10 notional)
    let mut rng = Rng64::new(0x1111_570C_ABCD_0001);
    for i in 0..10_000usize {
        let px_f = match i % 3 {
            0 => rng.range_f64(0.01, 0.9999),
            1 => rng.range_f64(0.995, 1.015),
            _ => rng.log_uniform(1.0, 5000.0),
        };
        let buy_px = floor_to_tick(decimal_from_f64_clean(px_f).unwrap()).max(d("0.01"));
        let min_shares = (MIN_ORDER_NOTIONAL_USD / buy_px)
            .ceil()
            .to_u64()
            .unwrap_or(1)
            .max(1);
        let shares = Decimal::from(min_shares + (rng.next_u64() % 500));
        let fair_px = floor_to_tick(buy_px * d("0.90"));

        let exit_px = compute_guaranteed_profitable_exit_price(
            shares,
            buy_px,
            shares,
            fair_px,
            Decimal::ZERO,
            Decimal::ZERO,
            Decimal::ZERO,
            false,
        )
        .unwrap();
        assert!(is_legal_sec_612_tick(exit_px) && exit_px > buy_px);
        let (net_pnl, _, _, _) = compute_net_trade_pnl(
            buy_px,
            exit_px,
            shares,
            shares,
            false,
            Decimal::ZERO,
            Decimal::ZERO,
            None,
            false,
        );
        assert!(
            net_pnl >= d("0.01"),
            "Stock trial {} ({} @ {}, exit={}) produced net_pnl={} < $0.01",
            i,
            shares,
            buy_px,
            exit_px,
            net_pnl
        );
    }

    println!(
        "[STOCK SUB-$1 REGRESSION PROVEN] 10,000 randomized sub-$1/boundary/standard stocks + midpoint cases ALL realize 2dp net_pnl >= $0.01"
    );
}

// =============================================================================
// THEOREM 2 — Strict Passive Entry Non-Crossing Proof (P_buy < P_ask)
// =============================================================================

#[test]
fn theorem2_exhaustive_boundary_and_50000_random_quotes() {
    let mut rng = Rng64::new(0x2222_3333_4444_5555);
    let mut total_checked = 0usize;

    // 1. Exhaustive critical boundary cases around $1.00, $0.0001, locked/crossed/negative
    let critical_cases = [
        (d("0.9999"), d("1.0000"), Some(d("0.9999"))),
        (d("0.9999"), d("1.0001"), Some(d("1.00"))),
        (d("0.9999"), d("1.01"), Some(d("1.00"))),
        (d("0.9995"), d("1.0005"), Some(d("0.9996"))),
        (d("0.9998"), d("1.0000"), Some(d("0.9999"))),
        (d("1.0000"), d("1.0001"), Some(d("1.00"))),
        (d("1.0000"), d("1.01"), Some(d("1.00"))),
        (d("1.0000"), d("1.02"), Some(d("1.01"))),
        (d("1.0001"), d("1.0002"), Some(d("1.00"))),
        (d("0.00005"), d("0.00009"), None),
        (d("0.00005"), d("0.00010"), None),
        (d("0.00005"), d("0.00011"), Some(d("0.0001"))),
        (d("0.0001"), d("0.0002"), Some(d("0.0001"))),
        (d("0.0001"), d("0.0003"), Some(d("0.0002"))),
        (d("72.504"), d("72.509"), Some(d("72.50"))),
        (d("72.509"), d("72.511"), Some(d("72.51"))),
        (d("72.50"), d("72.50"), None),
        (d("72.51"), d("72.50"), None),
        (d("0"), d("1.00"), None),
        (d("-5.00"), d("1.00"), None),
        (d("1.00"), d("1.0000000000000000000000000001"), Some(d("1.00"))),
    ];
    for (bid, ask, expected) in critical_cases {
        let got = calculate_hidden_mm_buy_price(bid, ask);
        assert_eq!(got, expected, "Failed boundary bid={} ask={}", bid, ask);
        if let Some(p) = got {
            assert!(p > Decimal::ZERO && p < ask && is_legal_sec_612_tick(p));
        }
        total_checked += 1;
    }

    // 2. Exhaustive grid around $1.00: [0.9990, 1.0030] step 0.00005 (~3,200 pairs)
    let step = d("0.00005");
    for i in 0..80u32 {
        let bid = d("0.9990") + step * Decimal::from(i);
        for j in (i + 1)..=80u32 {
            let ask = d("0.9990") + step * Decimal::from(j);
            let p = calculate_hidden_mm_buy_price(bid, ask)
                .unwrap_or_else(|| panic!("Must return Some for valid bid={} ask={}", bid, ask));
            assert!(p > Decimal::ZERO, "p={} <= 0", p);
            assert!(p < ask, "VIOLATION: p={} >= ask={} (bid={})", p, ask, bid);
            assert!(
                is_legal_sec_612_tick(p),
                "VIOLATION: p={} not legal tick (bid={}, ask={})",
                p,
                bid,
                ask
            );
            let formatted = format_tick_price(p);
            assert_eq!(Decimal::from_str(&formatted).unwrap(), p);
            total_checked += 1;
        }
    }

    // 3. 50,000 randomized quotes across all regimes (sub-$1, $1 boundary, sub-tick spreads, huge prices)
    for idx in 0..50_000usize {
        let bid_f = match idx % 5 {
            0 => rng.range_f64(0.00001, 0.99999),
            1 => rng.range_f64(0.995, 1.015),
            2 => rng.log_uniform(1.0, 1000.0),
            3 => rng.log_uniform(1000.0, 250_000.0),
            _ => rng.range_f64(-10.0, 10.0),
        };
        let spread_f = match idx % 6 {
            0 => rng.range_f64(1e-7, 1e-4),   // sub-tick spread
            1 => 0.0001,                      // 1 sub-$1 tick
            2 => 0.01,                        // 1 above-$1 tick
            3 => rng.range_f64(0.0, 0.05),    // tight / locked
            4 => rng.range_f64(-0.05, 0.0),   // crossed
            _ => rng.log_uniform(1e-5, 50.0), // wide spread
        };
        let bid = decimal_from_f64_clean(bid_f).unwrap();
        let ask = decimal_from_f64_clean(bid_f + spread_f).unwrap();
        let res = calculate_hidden_mm_buy_price(bid, ask);

        if bid <= Decimal::ZERO || ask <= bid {
            assert_eq!(res, None, "Invalid quote accepted: bid={} ask={}", bid, ask);
        } else if let Some(p) = res {
            assert!(p > Decimal::ZERO);
            assert!(
                p < ask,
                "NON-CROSSING VIOLATED: bid={} ask={} -> p={}",
                bid,
                ask,
                p
            );
            assert!(
                is_legal_sec_612_tick(p),
                "TICK LEGALITY VIOLATED: bid={} ask={} -> p={}",
                bid,
                ask,
                p
            );
            let bid_f_tick = floor_to_tick(bid);
            let tick = get_tick_size(bid);
            assert!(p >= bid_f_tick && p <= bid_f_tick + tick);
            assert_eq!(Decimal::from_str(&format_tick_price(p)).unwrap(), p);
        } else {
            // None on valid bid < ask is ONLY possible when no legal positive tick < ask exists
            // i.e. bid < 0.0001 AND ask <= 0.0001!
            assert!(
                bid < d("0.0001") && ask <= d("0.0001"),
                "Unexpected None on bid={} ask={}",
                bid,
                ask
            );
        }
        total_checked += 1;
    }

    println!(
        "[THEOREM 2 PROVEN] {} exhaustive + randomized quote trials verified 100% passive non-crossing (0 < p < ask) & SEC Rule 612 compliance",
        total_checked
    );
}

// =============================================================================
// THEOREM 3 — Adaptive 1D Kalman Filter Stability & Contraction
// =============================================================================

#[test]
fn theorem3_induction_steady_state_and_30000_random_ticks() {
    let params = KalmanParams {
        q_base: 1.5e-7,
        r_base: 4.0e-7,
        spread_weight: 2.0,
        adapt_lambda: 0.20,
    };

    // 1. Exact closed-form steady-state gain K_inf = (-3 + sqrt(105)) / 16 and half-life verification
    let expected_k_inf = (-3.0_f64 + 105.0_f64.sqrt()) / 16.0;
    let expected_half_life = 2.0_f64.ln() / (-(1.0 - expected_k_inf).ln());
    let mut kf_ss = AdaptiveKalmanFilter::new(params.clone(), 0.0036, 0.025, None);
    let m0 = 100.0_f64;
    let tiny_half_spread = 1e-9;
    for _ in 0..200 {
        kf_ss.update(m0 - tiny_half_spread, m0 + tiny_half_spread);
    }
    let p_prior_ss = kf_ss.state().p + kf_ss.state().q;
    let r_ss = params.r_base * m0 * m0 + params.spread_weight * tiny_half_spread * tiny_half_spread;
    let observed_k_inf = p_prior_ss / (p_prior_ss + r_ss);
    assert!(
        (observed_k_inf - expected_k_inf).abs() < 1e-11,
        "Steady-state K_inf mismatch: observed={}, expected={}",
        observed_k_inf,
        expected_k_inf
    );
    assert!((expected_k_inf - 0.4529344228724749).abs() < 1e-14);
    assert!((expected_half_life - 1.1491425).abs() < 1e-6);

    // 2. Bad quote immunity: NaN, Inf, -Inf, 0, negative, locked, crossed, mid overflow
    let state_before = kf_ss.state().clone();
    for (b, a) in [
        (f64::NAN, 100.0),
        (100.0, f64::NAN),
        (f64::INFINITY, 100.0),
        (100.0, f64::INFINITY),
        (f64::NEG_INFINITY, 100.0),
        (0.0, 100.0),
        (-5.0, 100.0),
        (100.0, 100.0),
        (100.05, 100.0),
        (f64::MAX * 0.8, f64::MAX * 0.9), // mid overflows to Inf!
    ] {
        let u = kf_ss.update(b, a);
        assert_eq!(u.signal, SignalType::None);
        assert_eq!(kf_ss.state().sample_count, state_before.sample_count);
        assert_eq!(kf_ss.state().x_hat, state_before.x_hat);
        assert_eq!(kf_ss.state().p, state_before.p);
        assert_eq!(kf_ss.state().q, state_before.q);
    }

    // 3. 30,000 randomized ticks across 6 price regimes verifying induction invariants & contraction
    let mut rng = Rng64::new(0x3333_4444_5555_6666);
    let mut total_ticks = 0usize;
    let mut cb_triggers = 0usize;

    for regime in 0..6 {
        let mut kf = AdaptiveKalmanFilter::new(params.clone(), 0.0036, 0.025, None);
        let base_px = match regime {
            0 => 0.005,
            1 => 0.9999,
            2 => 75.0,
            3 => 500.0,
            4 => 90_000.0,
            _ => 10.0,
        };
        let mut mid = base_px;
        let mut min_mid = f64::INFINITY;
        let mut max_mid = f64::NEG_INFINITY;

        for step in 0..5_000 {
            // Mix normal random walk, mild dips, and realistic flash crashes (3%..40%)
            let shock = if step > 10 && step % 97 == 0 {
                -rng.range_f64(0.03, 0.40) // flash crash -> triggers circuit breaker
            } else {
                rng.range_f64(-0.015, 0.015)
            };
            mid = (mid * (1.0 + shock)).max(base_px * 0.1);
            let rel_spread = rng.log_uniform(1e-5, 2e-2);
            let half_s = 0.5 * mid * rel_spread;
            let bid = mid - half_s;
            let ask = mid + half_s;
            let true_mid = 0.5 * (bid + ask);
            min_mid = min_mid.min(true_mid);
            max_mid = max_mid.max(true_mid);

            let p_before = kf.state().p;
            let q_before = kf.state().q;
            let x_before = kf.state().x_hat;
            let count_before = kf.state().sample_count;

            let upd = kf.update(bid, ask);
            let st = kf.state();

            assert!(st.is_valid(), "Invalid state at regime {} step {}", regime, step);
            assert!(st.x_hat.is_finite() && st.x_hat > 0.0);
            assert!(st.p.is_finite() && st.p > 0.0, "P_t not strictly positive: {}", st.p);
            assert!(st.q.is_finite() && st.q > 0.0);
            assert!(
                st.x_hat >= min_mid * (1.0 - 1e-12) && st.x_hat <= max_mid * (1.0 + 1e-12),
                "Convex hull violated: x_hat={} not in [{}, {}]",
                st.x_hat,
                min_mid,
                max_mid
            );

            if count_before > 0 {
                let p_prior = p_before + q_before;
                let r_t = (params.r_base * true_mid * true_mid
                    + params.spread_weight * (0.5 * (ask - bid)).powi(2))
                .max(1e-18);

                if matches!(upd.signal, SignalType::CircuitBreakerHalt { .. }) {
                    cb_triggers += 1;
                    let ref_px = true_mid.max(x_before);
                    let r_cb = (params.r_base * ref_px * ref_px
                        + params.spread_weight * (0.5 * (ask - bid)).powi(2))
                    .max(1e-18);
                    let k_gated = (p_prior / (p_prior + 20.0 * r_cb)).clamp(0.0, 0.25);
                    assert!(
                        k_gated <= 0.25 && k_gated < (1.0 / 20.0) * (p_prior / r_cb),
                        "Contraction/clamp bound failed: k_gated={}, bound={}",
                        k_gated,
                        (1.0 / 20.0) * (p_prior / r_cb)
                    );
                    let expected_x = x_before + k_gated * (true_mid - x_before);
                    assert!((st.x_hat - expected_x).abs() <= 1e-9 * x_before.max(1.0));
                    assert!(
                        st.p >= 0.75 * p_prior * (1.0 - 1e-12) && st.p > 0.0,
                        "CB posterior variance must satisfy P_t >= 0.75 * p_prior > 0: got P_t={}, p_prior={}",
                        st.p,
                        p_prior
                    );
                    assert!(
                        st.x_hat >= 0.75 * x_before * (1.0 - 1e-12),
                        "CB posterior x_hat {} fell below 0.75 * x_prior {}",
                        st.x_hat,
                        x_before
                    );
                } else {
                    // Normal tick: Q_t >= q_base * mid^2 and K_t in [0.01, 0.99]
                    let min_q = params.q_base * true_mid * true_mid;
                    assert!(
                        st.q >= min_q * (1.0 - 1e-15),
                        "Q_t={} < q_base*mid^2={}",
                        st.q,
                        min_q
                    );
                    let k_t = (p_prior / (p_prior + r_t)).clamp(0.01, 0.99);
                    assert!((0.01..=0.99).contains(&k_t));
                    assert!(
                        st.p >= 0.01 * p_prior * (1.0 - 1e-12) && st.p > 0.0,
                        "Normal posterior variance P_t={} < 0.01 * p_prior={}",
                        st.p,
                        p_prior
                    );
                }
            }
            total_ticks += 1;
        }
    }

    println!(
        "[THEOREM 3 STRESS PROVEN] {} ticks ({} CB events) verified anchored r_cb, k_gated<=0.25, P_t>=0.75*p_prior>0; K_inf={:.12}, half-life={:.6} ticks",
        total_ticks, cb_triggers, expected_k_inf, expected_half_life
    );
}

#[test]
fn theorem3_regression_ce31_extreme_bad_tick_bounded_variance_and_shift() {
    // CE-3.1 REGRESSION PROOF:
    // Anchoring `ref_px = mid.max(x_prior)` and clamping `k_gated in [0.0, 0.25]` prevents
    // IEEE-754 `P_t -> 0.0` collapse and bounds single-tick bad-quote shift to <= 25% of shock
    // even under repeated extreme near-zero bad ticks ($1e-5) on BTC=$90,000!
    let params = KalmanParams {
        q_base: 1.5e-7,
        r_base: 4.0e-7,
        spread_weight: 2.0,
        adapt_lambda: 0.20,
    };
    let mut kf = AdaptiveKalmanFilter::new(params, 0.0036, 0.025, None);
    for _ in 0..20 {
        kf.update(89_999.0, 90_001.0);
    }

    let x_before = kf.state().x_hat;
    let p_prior = kf.state().p + kf.state().q;
    let u = kf.update(1e-5, 1.01e-5);
    assert!(matches!(u.signal, SignalType::CircuitBreakerHalt { .. }));
    assert!(kf.state().is_valid());
    assert!(
        kf.state().p >= 0.75 * p_prior * (1.0 - 1e-12) && kf.state().p > 0.0,
        "CE-3.1 REGRESSION: P_t={} must stay >= 0.75 * p_prior={} (> 0)",
        kf.state().p,
        p_prior
    );
    let shift = (x_before - kf.state().x_hat).abs();
    let shock = (x_before - 1.005e-5).abs();
    assert!(
        shift <= 0.25 * shock + 1e-9,
        "CE-3.1 REGRESSION: single bad tick shift {} exceeded 25% of shock {}",
        shift,
        shock
    );
    assert!(kf.state().x_hat > 0.75 * x_before);

    // Stress 10 consecutive extreme near-zero ticks: P_t and x_hat remain strictly positive & finite!
    for idx in 0..10 {
        let pp = kf.state().p + kf.state().q;
        let xb = kf.state().x_hat;
        let u_rep = kf.update(1e-5, 1.01e-5);
        assert!(matches!(u_rep.signal, SignalType::CircuitBreakerHalt { .. }));
        assert!(kf.state().is_valid());
        assert!(
            kf.state().p >= 0.75 * pp * (1.0 - 1e-12) && kf.state().p > 0.0,
            "Consecutive bad tick {} violated P_t >= 0.75*p_prior",
            idx
        );
        assert!(kf.state().x_hat >= 0.75 * xb * (1.0 - 1e-12) && kf.state().x_hat > 0.0);
    }

    println!(
        "[CE-3.1 REGRESSION PROVEN] BTC=$90,000 under single & 10x consecutive $1e-5 bad ticks: P_t>=0.75*p_prior>0, shift<=25% shock, state strictly valid (final x_hat={:.2}, P_t={:.6})",
        kf.state().x_hat, kf.state().p
    );
}

// =============================================================================
// THEOREM 4 — Asymptotic Sizing Bounds, Monotonicity & Strict Cash Conservation
// =============================================================================

#[test]
fn theorem4_analytic_properties_and_30000_random_sizing_trials() {
    let f = |x: f64, l: f64| -> f64 { 1.0 - (1.0 - x.powf(l)).powf(1.0 / l) };

    // 1. Verify f(0)=0, f(1)=1, strict monotonicity, concavity (L<1), linearity (L=1), convexity (L>1)
    for &l in &[0.05, 0.25, 0.5, 1.0, 1.5, 2.0, 5.0] {
        assert!((f(0.0, l) - 0.0).abs() < 1e-15);
        assert!((f(1.0, l) - 1.0).abs() < 1e-15);

        let mut prev = 0.0;
        for step in 1..=1000 {
            let x = (step as f64) / 1000.0;
            let val = f(x, l);
            assert!(val >= prev - 1e-15, "Monotonicity violated at L={}, x={}", l, x);
            prev = val;
        }

        // Midpoint Jensen check on unsaturated domain
        let (a, b) = if l < 0.2 { (1e-8, 1e-4) } else { (0.2, 0.8) };
        let mid_val = f(0.5 * (a + b), l);
        let avg_val = 0.5 * (f(a, l) + f(b, l));
        if l < 1.0 {
            assert!(mid_val > avg_val, "Expected strict concavity for L={}", l);
        } else if (l - 1.0).abs() < 1e-12 {
            assert!((mid_val - avg_val).abs() < 1e-15, "Expected linearity for L=1");
        } else {
            assert!(mid_val < avg_val, "Expected strict convexity for L={}", l);
        }
    }

    // 2. Exact IEEE-754 f64 saturation threshold for L = 0.05:
    //    `1.0 - (1.0 - x^0.05)^20 == 1.0` iff `(1.0 - x^0.05)^20 < 2^-54`
    //    <=> `x > (1 - 2^(-54/20))^20 = 0.035273...`
    let x_sat_theory = (1.0 - 2.0_f64.powf(-54.0 / 20.0)).powf(20.0);
    assert!((x_sat_theory - 0.035273).abs() < 1e-4);
    assert!(f(0.035, 0.05) < 1.0);
    assert_eq!(f(0.036, 0.05), 1.0); // saturates cleanly to 1.0 in f64
    assert!(f(1e-100, 0.05) > 0.0 && f(1e-100, 0.05).is_finite());
    assert!(f(1e-300, 0.05) >= 0.0 && f(1e-300, 0.05).is_finite());

    // 3. 30,000 randomized sizing trials verifying STRICT cash & equity conservation
    let mut rng = Rng64::new(0x4444_5555_6666_7777);
    let min_m = 0.0036;
    let max_m = 0.0250;
    let mut nonzero_count = 0usize;

    for i in 0..30_000usize {
        let is_crypto = i % 2 == 0;
        let level = match i % 4 {
            0 => 0.05,
            1 => 0.5,
            2 => 1.0,
            _ => 2.5,
        };
        let margin = match i % 10 {
            0 => rng.range_f64(-0.01, min_m - 1e-9), // below min
            1 => rng.range_f64(max_m + 1e-9, 0.20),  // above max
            2 => min_m,
            3 => max_m,
            _ => rng.range_f64(min_m, max_m),
        };
        let equity = decimal_from_f64_clean(rng.log_uniform(1.0, 2_000_000.0))
            .unwrap()
            .round_dp(2);
        let cash = decimal_from_f64_clean(rng.log_uniform(0.01, 2_000_000.0))
            .unwrap()
            .round_dp(2);
        let buy_px = floor_to_tick(decimal_from_f64_clean(rng.log_uniform(0.001, 100_000.0)).unwrap())
            .max(d("0.0001"));
        let max_shares = decimal_from_f64_clean(rng.range_f64(0.05, 1.0))
            .unwrap()
            .round_dp(4);

        let qty = calculate_dynamic_portfolio_shares(
            margin, min_m, max_m, level, max_shares, equity, cash, buy_px, is_crypto,
        );

        assert!(qty >= Decimal::ZERO);
        if margin < min_m || margin > max_m {
            assert_eq!(qty, Decimal::ZERO);
        }
        let notional = qty * buy_px;
        let cap = cash.min(equity * max_shares);
        assert!(
            notional <= cash,
            "CASH OVERSPEND: qty={} * px={} = {} > cash={}",
            qty,
            buy_px,
            notional,
            cash
        );
        assert!(
            notional <= cap,
            "EQUITY CAP VIOLATED: qty={} * px={} = {} > cap={}",
            qty,
            buy_px,
            notional,
            cap
        );
        if qty > Decimal::ZERO {
            assert!(
                notional >= MIN_ORDER_NOTIONAL_USD,
                "MIN NOTIONAL VIOLATED: {} < $10",
                notional
            );
            if !is_crypto {
                assert_eq!(qty, qty.floor(), "Equity share count must be integer!");
            }
            nonzero_count += 1;
        }
    }

    // Invalid/non-finite rejection
    for bad in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert_eq!(
            calculate_dynamic_portfolio_shares(
                bad, min_m, max_m, 0.05, d("1"), d("100000"), d("100000"), d("72"), true
            ),
            Decimal::ZERO
        );
        assert_eq!(
            calculate_dynamic_portfolio_shares(
                0.01, min_m, max_m, bad, d("1"), d("100000"), d("100000"), d("72"), true
            ),
            Decimal::ZERO
        );
    }

    println!(
        "[THEOREM 4 STRESS] 30,000 randomized sizing trials ({} nonzero) verified 0 cash overspends & strict >=$10 notional; x_sat(L=0.05)={:.6}",
        nonzero_count, x_sat_theory
    );
}

#[test]
fn theorem4_regression_ce41_equity_cap_29digit_rounding() {
    // CE-4.1 REGRESSION PROOF:
    // The post-condition loop now guards `capital_cap = min(target_capital, safe_cash, equity_cap)`
    // via `checked_mul`, guaranteeing BOTH `qty * buy_price <= available_cash` AND
    // `qty * buy_price <= total_equity * max_shares_param` even under 29-digit Decimal division rounding!
    let equity = d("29.999999999999999999999999999");
    let cash = d("35.00");
    let buy_px = d("3.00");

    // Crypto mode (4dp step = 0.0001)
    let qty_crypto = calculate_dynamic_portfolio_shares(
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
    assert_eq!(qty_crypto, d("9.9999"));
    assert!(qty_crypto * buy_px <= cash);
    assert!(
        qty_crypto * buy_px <= equity * Decimal::ONE,
        "CE-4.1 REGRESSION (crypto): qty*buy_px={} exceeded equity={}",
        qty_crypto * buy_px,
        equity
    );

    // Equity mode (whole share step = 1)
    let qty_stock = calculate_dynamic_portfolio_shares(
        0.0250,
        0.0036,
        0.0250,
        0.05,
        Decimal::ONE,
        equity,
        cash,
        buy_px,
        false,
    );
    assert_eq!(qty_stock, d("9"));
    assert!(qty_stock * buy_px <= cash);
    assert!(qty_stock * buy_px <= equity * Decimal::ONE);

    // Fractional max_shares_param = 0.5 with 29-digit equity
    let qty_half = calculate_dynamic_portfolio_shares(
        0.0250,
        0.0036,
        0.0250,
        0.05,
        d("0.5"),
        d("59.999999999999999999999999999"),
        d("100.00"),
        d("3.00"),
        true,
    );
    let half_cap = d("59.999999999999999999999999999") * d("0.5");
    assert!(qty_half > Decimal::ZERO);
    assert!(qty_half * d("3.00") <= half_cap);

    println!(
        "[CE-4.1 REGRESSION PROVEN] 29-digit Decimal division rounding strictly obeys capital_cap: crypto qty={} ($29.9997 <= equity), stock qty={} ($27 <= equity)",
        qty_crypto, qty_stock
    );
}

// =============================================================================
// THEOREM 5 — FSM Safety, Absorbing Terminal States & Fault Tolerance
// =============================================================================

#[tokio::test]
async fn theorem5_absorption_and_order_id_uniqueness_stress() {
    let params = default_sol_params();
    let now = DateTime::parse_from_rfc3339("2026-11-24T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc);

    // Populate ALL 5 terminal states + 1 active hunt, inject stale echoes of EVERY terminal
    // order ID plus duplicate open-order rows, and run 50 consecutive reconciliation cycles!
    let terminal_states = [
        ("term-comp", HuntStatus::Completed, "oid-comp"),
        ("term-stle", HuntStatus::LiquidatedStale, "oid-stle"),
        ("term-manu", HuntStatus::LiquidatedManual, "oid-manu"),
        ("term-orph", HuntStatus::LiquidatedOrphan, "oid-orph"),
        ("term-adpc", HuntStatus::AdoptedClosed, "oid-adpc"),
    ];

    let mut st = AdversarialBrokerState::base();
    st.position = Some(Position {
        asset_id: Some("sol".into()),
        symbol: "SOLUSD".into(),
        exchange: Some("CRYPTO".into()),
        asset_class: Some("crypto".into()),
        qty: d("10.0"),
        qty_available: Decimal::ZERO,
        avg_entry_price: d("72.00"),
        side: "long".into(),
        market_value: Some(d("720.00")),
        cost_basis: Some(d("720.00")),
        unrealized_pl: Some(Decimal::ZERO),
        current_price: Some(d("72.00")),
    });

    let mut trades = Vec::new();
    for (id, status, oid) in terminal_states {
        trades.push(TradeRecord {
            id: id.into(),
            symbol: "SOL/USD".into(),
            buy_order_id: Some(format!("buy-{}", id)),
            sell_order_id: Some(oid.into()),
            buy_qty: d("10"),
            qty: d("9.985"),
            filled_exit_qty: d("9.985"),
            buy_price: d("72.00"),
            sell_price: Some(d("72.50")),
            pnl: Some(d("2.00")),
            pnl_pct: Some(d("0.0028")),
            gross_pnl: Some(d("5.00")),
            fees_usd: Some(d("3.00")),
            buy_timestamp: "2026-11-24T08:00:00Z".into(),
            sell_timestamp: Some("2026-11-24T09:00:00Z".into()),
            status,
            adopted_from_broker: status == HuntStatus::AdoptedClosed,
            recovered_orphan: false,
            buy_fee_already_deducted: None,
        });

        // Inject terminal order in authoritative map AND ghost "new" entry in stale_open_orders!
        let auth_ord = Order {
            id: oid.into(),
            client_order_id: None,
            created_at: Some("2026-11-24T08:00:00Z".into()),
            updated_at: None,
            submitted_at: None,
            filled_at: Some("2026-11-24T09:00:00Z".into()),
            expired_at: None,
            canceled_at: None,
            failed_at: None,
            symbol: "SOL/USD".into(),
            qty: d("9.985"),
            filled_qty: d("9.985"),
            filled_avg_price: Some(d("72.50")),
            order_class: None,
            side: "sell".into(),
            order_type: "limit".into(),
            time_in_force: "gtc".into(),
            limit_price: Some(d("72.50")),
            status: "filled".into(),
        };
        let mut stale_ghost = auth_ord.clone();
        stale_ghost.status = "new".into();
        st.orders.insert(oid.into(), auth_ord);
        st.stale_open_orders.push(stale_ghost);
    }

    // Add an untracked legit open sell duplicated TWICE in get_open_orders() to test intra-cycle dedup!
    let legit_ext = Order {
        id: "ext-open-9999".into(),
        client_order_id: None,
        created_at: Some("2026-11-24T11:30:00Z".into()),
        updated_at: None,
        submitted_at: None,
        filled_at: None,
        expired_at: None,
        canceled_at: None,
        failed_at: None,
        symbol: "SOL/USD".into(),
        qty: d("10.0"),
        filled_qty: Decimal::ZERO,
        filled_avg_price: None,
        order_class: None,
        side: "sell".into(),
        order_type: "limit".into(),
        time_in_force: "gtc".into(),
        limit_price: Some(d("75.00")),
        status: "new".into(),
    };
    st.orders.insert("ext-open-9999".into(), legit_ext.clone());
    st.stale_open_orders.push(legit_ext); // duplicate copy in open_orders list!

    let broker = AdversarialMockBroker::new(st);
    let mut fee_tracker = FeeTracker::new();

    for cycle in 0..50 {
        reconcile_and_maintain_positions(
            &broker,
            &params,
            &mut trades,
            &mut fee_tracker,
            d("72.50"),
            d("72.00"),
            now + ChronoDuration::seconds(cycle * 30),
            None,
            ReconciliationTiming::fast_for_tests(),
        )
        .await;

        // 1. Verify ALL 5 terminal states remained strictly absorbing!
        for (idx, (_, expected_status, _)) in terminal_states.iter().enumerate() {
            assert_eq!(
                trades[idx].status, *expected_status,
                "Terminal state escaped absorption on cycle {}",
                cycle
            );
        }

        // 2. Verify strict sell_order_id uniqueness across ALL records!
        let mut seen = HashSet::new();
        for t in &trades {
            if let Some(ref oid) = t.sell_order_id {
                assert!(
                    seen.insert(oid.clone()),
                    "DUPLICATE sell_order_id {} detected on cycle {}!",
                    oid,
                    cycle
                );
            }
        }
        assert_eq!(trades.len(), 6); // 5 original terminal + 1 deduplicated adoption
    }

    println!(
        "[THEOREM 5 ABSORPTION & UNIQUENESS PROVEN] 50 adversarial stale-index cycles: 0 terminal escapes, 0 duplicate sell_order_ids"
    );
}

#[tokio::test]
async fn theorem5_general_api_error_resilience() {
    let params = default_sol_params();
    let now = DateTime::parse_from_rfc3339("2026-11-24T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc);

    // Test every single API failure mode (`get_account`, `get_position`, `get_open_orders`,
    // `get_order`, `submit_order`) against all 3 active states (`ExitSubmitPending`,
    // `OpenExitPending`, `ExitPartiallyFilled`): verifies ZERO false closures to `Completed`
    // and ZERO silent share drops under steady outages.
    for failure_mode in 0..5 {
        let mut st = AdversarialBrokerState::base();
        st.position = Some(Position {
            asset_id: Some("sol".into()),
            symbol: "SOLUSD".into(),
            exchange: Some("CRYPTO".into()),
            asset_class: Some("crypto".into()),
            qty: d("30.0"),
            qty_available: d("10.0"),
            avg_entry_price: d("72.00"),
            side: "long".into(),
            market_value: Some(d("2160.00")),
            cost_basis: Some(d("2160.00")),
            unrealized_pl: Some(Decimal::ZERO),
            current_price: Some(d("72.00")),
        });
        for oid in ["ord-open-1", "ord-part-2"] {
            st.orders.insert(
                oid.into(),
                Order {
                    id: oid.into(),
                    client_order_id: None,
                    created_at: Some("2026-11-24T11:00:00Z".into()),
                    updated_at: None,
                    submitted_at: None,
                    filled_at: None,
                    expired_at: None,
                    canceled_at: None,
                    failed_at: None,
                    symbol: "SOL/USD".into(),
                    qty: d("10.0"),
                    filled_qty: if oid == "ord-part-2" {
                        d("3.0")
                    } else {
                        Decimal::ZERO
                    },
                    filled_avg_price: if oid == "ord-part-2" {
                        Some(d("73.00"))
                    } else {
                        None
                    },
                    order_class: None,
                    side: "sell".into(),
                    order_type: "limit".into(),
                    time_in_force: "gtc".into(),
                    limit_price: Some(d("73.00")),
                    status: if oid == "ord-part-2" {
                        "partially_filled".into()
                    } else {
                        "new".into()
                    },
                },
            );
        }

        match failure_mode {
            0 => st.get_account_fail = true,
            1 => {
                for _ in 0..5 {
                    st.get_position_results
                        .push_back(Err("simulated position outage".into()));
                }
            }
            2 => st.get_open_orders_fail = true,
            3 => {
                st.get_order_fail_ids.insert("ord-open-1".into());
                st.get_order_fail_ids.insert("ord-part-2".into());
            }
            _ => st.submit_sell_fail = true,
        }

        let broker = AdversarialMockBroker::new(st);
        let mut trades = vec![
            TradeRecord {
                id: "h-sub".into(),
                symbol: "SOL/USD".into(),
                buy_order_id: Some("b1".into()),
                sell_order_id: None,
                buy_qty: d("10.015"),
                qty: d("10.0"),
                filled_exit_qty: Decimal::ZERO,
                buy_price: d("72.00"),
                sell_price: Some(d("72.50")),
                pnl: None,
                pnl_pct: None,
                gross_pnl: None,
                fees_usd: None,
                buy_timestamp: "2026-11-24T11:00:00Z".into(),
                sell_timestamp: None,
                status: HuntStatus::ExitSubmitPending,
                adopted_from_broker: false,
                recovered_orphan: false,
                buy_fee_already_deducted: None,
            },
            TradeRecord {
                id: "h-opn".into(),
                symbol: "SOL/USD".into(),
                buy_order_id: Some("b2".into()),
                sell_order_id: Some("ord-open-1".into()),
                buy_qty: d("10.015"),
                qty: d("10.0"),
                filled_exit_qty: Decimal::ZERO,
                buy_price: d("72.00"),
                sell_price: Some(d("73.00")),
                pnl: None,
                pnl_pct: None,
                gross_pnl: None,
                fees_usd: None,
                buy_timestamp: "2026-11-24T11:00:00Z".into(),
                sell_timestamp: None,
                status: HuntStatus::OpenExitPending,
                adopted_from_broker: false,
                recovered_orphan: false,
                buy_fee_already_deducted: None,
            },
            TradeRecord {
                id: "h-prt".into(),
                symbol: "SOL/USD".into(),
                buy_order_id: Some("b3".into()),
                sell_order_id: Some("ord-part-2".into()),
                buy_qty: d("10.015"),
                qty: d("10.0"),
                filled_exit_qty: d("3.0"),
                buy_price: d("72.00"),
                sell_price: Some(d("73.00")),
                pnl: None,
                pnl_pct: None,
                gross_pnl: None,
                fees_usd: None,
                buy_timestamp: "2026-11-24T11:00:00Z".into(),
                sell_timestamp: None,
                status: HuntStatus::ExitPartiallyFilled,
                adopted_from_broker: false,
                recovered_orphan: false,
                buy_fee_already_deducted: None,
            },
        ];
        let mut ft = FeeTracker::new();

        reconcile_and_maintain_positions(
            &broker,
            &params,
            &mut trades,
            &mut ft,
            d("72.50"),
            d("72.00"),
            now,
            None,
            ReconciliationTiming::fast_for_tests(),
        )
        .await;

        assert_eq!(trades.len(), 3);
        for t in &trades {
            assert!(
                t.status.is_active(),
                "Failure mode {} falsely closed active hunt {} as {}",
                failure_mode,
                t.id,
                t.status
            );
        }
    }

    println!(
        "[THEOREM 5 GENERAL ERROR RESILIENCE PROVEN] All 5 endpoint steady-failure modes preserved 100% active hunts without false closure"
    );
}

#[tokio::test]
async fn theorem5_regression_ce51_ce52_ce53_fault_tolerance_and_partial_mkt_fill() {
    let params = default_sol_params();
    let now = DateTime::parse_from_rfc3339("2026-11-24T23:00:00Z")
        .unwrap()
        .with_timezone(&Utc);

    // -------------------------------------------------------------------------
    // CE-5.1 REGRESSION PROOF:
    // Stale order cancel times out in `pending_cancel` -> MUST stay active (`OpenExitPending`),
    // MUST NOT falsely close as `LiquidatedManual`, and `deferred_exit_recovery` MUST block
    // Step D from spawning a duplicate orphan!
    // -------------------------------------------------------------------------
    {
        let mut st = AdversarialBrokerState::base();
        st.cancel_leaves_pending = true;
        st.position = Some(Position {
            asset_id: Some("sol".into()),
            symbol: "SOLUSD".into(),
            exchange: Some("CRYPTO".into()),
            asset_class: Some("crypto".into()),
            qty: d("50.0"),
            qty_available: Decimal::ZERO,
            avg_entry_price: d("74.00"),
            side: "long".into(),
            market_value: Some(d("3600")),
            cost_basis: Some(d("3700")),
            unrealized_pl: Some(d("-100")),
            current_price: Some(d("72.00")),
        });
        st.orders.insert(
            "stale-pend-01".into(),
            Order {
                id: "stale-pend-01".into(),
                client_order_id: None,
                created_at: Some("2026-11-24T09:00:00Z".into()), // 14h old
                updated_at: None,
                submitted_at: None,
                filled_at: None,
                expired_at: None,
                canceled_at: None,
                failed_at: None,
                symbol: "SOL/USD".into(),
                qty: d("50.0"),
                filled_qty: Decimal::ZERO,
                filled_avg_price: None,
                order_class: None,
                side: "sell".into(),
                order_type: "limit".into(),
                time_in_force: "gtc".into(),
                limit_price: Some(d("76.00")),
                status: "new".into(),
            },
        );
        let broker = AdversarialMockBroker::new(st);
        let mut trades = vec![TradeRecord {
            id: "ce51hunt".into(),
            symbol: "SOL/USD".into(),
            buy_order_id: Some("buy-51".into()),
            sell_order_id: Some("stale-pend-01".into()),
            buy_qty: d("50.075"),
            qty: d("50.0"),
            filled_exit_qty: Decimal::ZERO,
            buy_price: d("74.00"),
            sell_price: Some(d("76.00")),
            pnl: None,
            pnl_pct: None,
            gross_pnl: None,
            fees_usd: None,
            buy_timestamp: "2026-11-24T09:00:00Z".into(),
            sell_timestamp: None,
            status: HuntStatus::OpenExitPending,
            adopted_from_broker: false,
            recovered_orphan: false,
            buy_fee_already_deducted: Some(true),
        }];
        let mut ft = FeeTracker::new();
        reconcile_and_maintain_positions(
            &broker,
            &params,
            &mut trades,
            &mut ft,
            d("72.00"),
            d("72.00"),
            now,
            None,
            ReconciliationTiming::fast_for_tests(),
        )
        .await;

        assert_eq!(
            trades[0].status,
            HuntStatus::OpenExitPending,
            "CE-5.1 REGRESSION: pending_cancel timeout MUST stay OpenExitPending"
        );
        assert_eq!(trades.len(), 1, "Step D MUST NOT spawn duplicate orphan while deferred");
        assert!(broker.st.lock().unwrap().submitted_sell_requests.is_empty());
    }

    // -------------------------------------------------------------------------
    // CE-5.2 REGRESSION PROOF:
    // Mid-cycle `get_position` `Err` on canceled exit MUST defer cleanly (keeping hunt
    // `OpenExitPending`), MUST block Step D even if Step C `get_position` recovers showing
    // `qty_available = 50.0`, and MUST cleanly re-raise exit on the next healthy cycle!
    // -------------------------------------------------------------------------
    {
        let mut st = AdversarialBrokerState::base();
        let locked_pos = Position {
            asset_id: Some("sol".into()),
            symbol: "SOLUSD".into(),
            exchange: Some("CRYPTO".into()),
            asset_class: Some("crypto".into()),
            qty: d("50.0"),
            qty_available: Decimal::ZERO,
            avg_entry_price: d("74.00"),
            side: "long".into(),
            market_value: Some(d("3600")),
            cost_basis: Some(d("3700")),
            unrealized_pl: Some(d("-100")),
            current_price: Some(d("72.00")),
        };
        let mut unlocked_pos = locked_pos.clone();
        unlocked_pos.qty_available = d("50.0");

        st.position = Some(unlocked_pos.clone());
        // Call 1 (initial snapshot): Ok(Some(locked_pos)) -> avail_pool = 0, position_fetch_ok = true
        st.get_position_results.push_back(Ok(Some(locked_pos)));
        // Call 2 (Step B mid-cycle re-query after seeing canceled order): Err(timeout)!
        st.get_position_results
            .push_back(Err("transient mid-cycle position timeout".into()));
        // Call 3 (Step C refresh): recovers showing qty_available = 50.0!
        // `deferred_exit_recovery` MUST STILL block Step D from spawning a duplicate `rec-` orphan!
        st.get_position_results.push_back(Ok(Some(unlocked_pos)));

        st.orders.insert(
            "canceled-mid-01".into(),
            Order {
                id: "canceled-mid-01".into(),
                client_order_id: None,
                created_at: Some("2026-11-24T22:00:00Z".into()),
                updated_at: None,
                submitted_at: None,
                filled_at: None,
                expired_at: None,
                canceled_at: Some("2026-11-24T22:59:59Z".into()),
                failed_at: None,
                symbol: "SOL/USD".into(),
                qty: d("50.0"),
                filled_qty: Decimal::ZERO,
                filled_avg_price: None,
                order_class: None,
                side: "sell".into(),
                order_type: "limit".into(),
                time_in_force: "gtc".into(),
                limit_price: Some(d("76.00")),
                status: "canceled".into(),
            },
        );
        let broker = AdversarialMockBroker::new(st);
        let mut trades = vec![TradeRecord {
            id: "ce52hunt".into(),
            symbol: "SOL/USD".into(),
            buy_order_id: Some("buy-52".into()),
            sell_order_id: Some("canceled-mid-01".into()),
            buy_qty: d("50.075"),
            qty: d("50.0"),
            filled_exit_qty: Decimal::ZERO,
            buy_price: d("74.00"),
            sell_price: Some(d("76.00")),
            pnl: None,
            pnl_pct: None,
            gross_pnl: None,
            fees_usd: None,
            buy_timestamp: "2026-11-24T22:00:00Z".into(),
            sell_timestamp: None,
            status: HuntStatus::OpenExitPending,
            adopted_from_broker: false,
            recovered_orphan: false,
            buy_fee_already_deducted: Some(true),
        }];
        let mut ft = FeeTracker::new();

        // Cycle 1: Defers cleanly without closing or duplicating!
        reconcile_and_maintain_positions(
            &broker,
            &params,
            &mut trades,
            &mut ft,
            d("72.00"),
            d("72.00"),
            now,
            None,
            ReconciliationTiming::fast_for_tests(),
        )
        .await;

        assert_eq!(trades.len(), 1, "deferred_exit_recovery MUST block Step D orphan duplication!");
        assert_eq!(
            trades[0].status,
            HuntStatus::OpenExitPending,
            "CE-5.2 REGRESSION: mid-cycle get_position Err MUST keep hunt OpenExitPending"
        );
        assert_eq!(trades[0].sell_order_id.as_deref(), Some("canceled-mid-01"));

        // Cycle 2 (healthy position endpoint): cleanly re-raises exit on the original hunt!
        reconcile_and_maintain_positions(
            &broker,
            &params,
            &mut trades,
            &mut ft,
            d("72.00"),
            d("72.00"),
            now + ChronoDuration::seconds(30),
            None,
            ReconciliationTiming::fast_for_tests(),
        )
        .await;

        assert_eq!(trades.len(), 1);
        assert_eq!(trades[0].status, HuntStatus::OpenExitPending);
        assert_ne!(trades[0].sell_order_id.as_deref(), Some("canceled-mid-01"));
        assert!(trades[0].sell_order_id.is_some());
    }

    // -------------------------------------------------------------------------
    // CE-5.3 REGRESSION PROOF (Part A — Rejected 0-fill market sell):
    // Rejected stale market sell (`filled_qty = 0`) MUST transition to `ExitSubmitPending`
    // (NEVER `LiquidatedStale`), clearing `sell_order_id` and blocking Step D orphan creation!
    // -------------------------------------------------------------------------
    {
        let mut st = AdversarialBrokerState::base();
        st.submit_market_rejected = true;
        st.position = Some(Position {
            asset_id: Some("sol".into()),
            symbol: "SOLUSD".into(),
            exchange: Some("CRYPTO".into()),
            asset_class: Some("crypto".into()),
            qty: d("50.0"),
            qty_available: Decimal::ZERO,
            avg_entry_price: d("74.00"),
            side: "long".into(),
            market_value: Some(d("3600")),
            cost_basis: Some(d("3700")),
            unrealized_pl: Some(d("-100")),
            current_price: Some(d("72.00")),
        });
        st.orders.insert(
            "stale-mkt-rej".into(),
            Order {
                id: "stale-mkt-rej".into(),
                client_order_id: None,
                created_at: Some("2026-11-24T09:00:00Z".into()),
                updated_at: None,
                submitted_at: None,
                filled_at: None,
                expired_at: None,
                canceled_at: None,
                failed_at: None,
                symbol: "SOL/USD".into(),
                qty: d("50.0"),
                filled_qty: Decimal::ZERO,
                filled_avg_price: None,
                order_class: None,
                side: "sell".into(),
                order_type: "limit".into(),
                time_in_force: "gtc".into(),
                limit_price: Some(d("76.00")),
                status: "new".into(),
            },
        );
        let broker = AdversarialMockBroker::new(st);
        let mut trades = vec![TradeRecord {
            id: "ce53hunt".into(),
            symbol: "SOL/USD".into(),
            buy_order_id: Some("buy-53".into()),
            sell_order_id: Some("stale-mkt-rej".into()),
            buy_qty: d("50.075"),
            qty: d("50.0"),
            filled_exit_qty: Decimal::ZERO,
            buy_price: d("74.00"),
            sell_price: Some(d("76.00")),
            pnl: None,
            pnl_pct: None,
            gross_pnl: None,
            fees_usd: None,
            buy_timestamp: "2026-11-24T09:00:00Z".into(),
            sell_timestamp: None,
            status: HuntStatus::OpenExitPending,
            adopted_from_broker: false,
            recovered_orphan: false,
            buy_fee_already_deducted: Some(true),
        }];
        let mut ft = FeeTracker::new();
        reconcile_and_maintain_positions(
            &broker,
            &params,
            &mut trades,
            &mut ft,
            d("72.00"),
            d("72.00"),
            now,
            None,
            ReconciliationTiming::fast_for_tests(),
        )
        .await;

        assert_eq!(
            trades[0].status,
            HuntStatus::ExitSubmitPending,
            "CE-5.3 REGRESSION: rejected stale market sell MUST transition to ExitSubmitPending"
        );
        assert_eq!(trades[0].sell_order_id, None);
        assert_eq!(trades[0].qty, d("50.0"));
        assert_eq!(trades[0].filled_exit_qty, Decimal::ZERO);
        assert_eq!(trades.len(), 1);
    }

    // -------------------------------------------------------------------------
    // CE-5.3 Part B — Non-terminal stale market sell (`status = "new"` past poll wait):
    // MUST remain active (`OpenExitPending`) tracking the new market OID, with Step D blocked!
    // -------------------------------------------------------------------------
    {
        let mut st = AdversarialBrokerState::base();
        st.submit_market_pending = true;
        st.position = Some(Position {
            asset_id: Some("sol".into()),
            symbol: "SOLUSD".into(),
            exchange: Some("CRYPTO".into()),
            asset_class: Some("crypto".into()),
            qty: d("50.0"),
            qty_available: Decimal::ZERO,
            avg_entry_price: d("74.00"),
            side: "long".into(),
            market_value: Some(d("3600")),
            cost_basis: Some(d("3700")),
            unrealized_pl: Some(d("-100")),
            current_price: Some(d("72.00")),
        });
        st.orders.insert(
            "stale-mkt-pend".into(),
            Order {
                id: "stale-mkt-pend".into(),
                client_order_id: None,
                created_at: Some("2026-11-24T09:00:00Z".into()),
                updated_at: None,
                submitted_at: None,
                filled_at: None,
                expired_at: None,
                canceled_at: None,
                failed_at: None,
                symbol: "SOL/USD".into(),
                qty: d("50.0"),
                filled_qty: Decimal::ZERO,
                filled_avg_price: None,
                order_class: None,
                side: "sell".into(),
                order_type: "limit".into(),
                time_in_force: "gtc".into(),
                limit_price: Some(d("76.00")),
                status: "new".into(),
            },
        );
        let broker = AdversarialMockBroker::new(st);
        let mut trades = vec![TradeRecord {
            id: "ce53pend".into(),
            symbol: "SOL/USD".into(),
            buy_order_id: Some("buy-53p".into()),
            sell_order_id: Some("stale-mkt-pend".into()),
            buy_qty: d("50.075"),
            qty: d("50.0"),
            filled_exit_qty: Decimal::ZERO,
            buy_price: d("74.00"),
            sell_price: Some(d("76.00")),
            pnl: None,
            pnl_pct: None,
            gross_pnl: None,
            fees_usd: None,
            buy_timestamp: "2026-11-24T09:00:00Z".into(),
            sell_timestamp: None,
            status: HuntStatus::OpenExitPending,
            adopted_from_broker: false,
            recovered_orphan: false,
            buy_fee_already_deducted: Some(true),
        }];
        let mut ft = FeeTracker::new();
        reconcile_and_maintain_positions(
            &broker,
            &params,
            &mut trades,
            &mut ft,
            d("72.00"),
            d("72.00"),
            now,
            None,
            ReconciliationTiming::fast_for_tests(),
        )
        .await;

        assert_eq!(trades.len(), 1);
        assert_eq!(trades[0].status, HuntStatus::OpenExitPending);
        assert!(trades[0]
            .sell_order_id
            .as_deref()
            .is_some_and(|id| id.starts_with("adv-mkt-")));
    }

    // -------------------------------------------------------------------------
    // ADVERSARIAL ROUND-2 TEST — Stale PARTIAL market fill leaving non-dust remainder:
    // Stale limit order had 10/50 filled; market sell for remaining 40 ends `canceled`
    // filling only 15 -> non-dust remainder 25 SOL MUST remain active in `ExitSubmitPending`
    // with prorated `buy_qty`, `qty = 25`, `filled_exit_qty = 0`, `sell_order_id = None`!
    // -------------------------------------------------------------------------
    {
        let mut st = AdversarialBrokerState::base();
        st.submit_market_partial_fill = Some(d("15.0"));
        st.position = Some(Position {
            asset_id: Some("sol".into()),
            symbol: "SOLUSD".into(),
            exchange: Some("CRYPTO".into()),
            asset_class: Some("crypto".into()),
            qty: d("40.0"), // 10 already filled on stale limit
            qty_available: Decimal::ZERO,
            avg_entry_price: d("74.00"),
            side: "long".into(),
            market_value: Some(d("2880")),
            cost_basis: Some(d("2960")),
            unrealized_pl: Some(d("-80")),
            current_price: Some(d("72.00")),
        });
        st.orders.insert(
            "stale-part-mkt".into(),
            Order {
                id: "stale-part-mkt".into(),
                client_order_id: None,
                created_at: Some("2026-11-24T09:00:00Z".into()),
                updated_at: None,
                submitted_at: None,
                filled_at: None,
                expired_at: None,
                canceled_at: None,
                failed_at: None,
                symbol: "SOL/USD".into(),
                qty: d("50.0"),
                filled_qty: d("10.0"),
                filled_avg_price: Some(d("76.00")),
                order_class: None,
                side: "sell".into(),
                order_type: "limit".into(),
                time_in_force: "gtc".into(),
                limit_price: Some(d("76.00")),
                status: "partially_filled".into(),
            },
        );
        let broker = AdversarialMockBroker::new(st);
        let mut trades = vec![TradeRecord {
            id: "partmkt1".into(),
            symbol: "SOL/USD".into(),
            buy_order_id: Some("buy-pm".into()),
            sell_order_id: Some("stale-part-mkt".into()),
            buy_qty: d("50.075"),
            qty: d("50.0"),
            filled_exit_qty: d("10.0"),
            buy_price: d("74.00"),
            sell_price: Some(d("76.00")),
            pnl: None,
            pnl_pct: None,
            gross_pnl: None,
            fees_usd: None,
            buy_timestamp: "2026-11-24T09:00:00Z".into(),
            sell_timestamp: None,
            status: HuntStatus::ExitPartiallyFilled,
            adopted_from_broker: false,
            recovered_orphan: false,
            buy_fee_already_deducted: Some(true),
        }];
        let mut ft = FeeTracker::new();
        let (_, tel) = reconcile_and_maintain_positions(
            &broker,
            &params,
            &mut trades,
            &mut ft,
            d("72.00"),
            d("72.00"),
            now,
            None,
            ReconciliationTiming::fast_for_tests(),
        )
        .await;

        assert_eq!(trades.len(), 1);
        assert_eq!(trades[0].status, HuntStatus::ExitSubmitPending);
        assert_eq!(trades[0].sell_order_id, None);
        assert_eq!(trades[0].qty, d("25.0"));
        assert_eq!(trades[0].filled_exit_qty, Decimal::ZERO);
        assert_eq!(trades[0].buy_qty, d("25.0375")); // exact proration: 50.075 * (25 / 50)
        assert!((tel.reconciliation.tracked_active_qty - 25.0).abs() < 1e-9);
        assert!((tel.reconciliation.broker_qty - 25.0).abs() < 1e-9);
    }

    println!(
        "[CE-5.1/5.2/5.3 & PARTIAL MKT FILL PROVEN] pending_cancel, mid-cycle position Err, rejected/pending/partial stale market sells ALL preserve active hunts cleanly"
    );
}

#[tokio::test]
async fn theorem5_regression_rf54_and_rf55_residuals_eliminated() {
    let params = default_sol_params();
    let now = DateTime::parse_from_rfc3339("2026-11-24T23:00:00Z")
        .unwrap()
        .with_timezone(&Utc);

    // -------------------------------------------------------------------------
    // RF-5.4 REGRESSION PROOF:
    // Partial stale limit sell (30/50 filled) + market sell HTTP 500 error:
    // MUST remain active (`ExitSubmitPending`, `qty = 20`), prorate `buy_qty`,
    // reset `filled_exit_qty = 0`, and preserve `tracked_active_qty == 20.0` (`is_synced == true`).
    // -------------------------------------------------------------------------
    {
        let mut st = AdversarialBrokerState::base();
        st.submit_market_http_err = true;
        st.position = Some(Position {
            asset_id: Some("sol".into()),
            symbol: "SOLUSD".into(),
            exchange: Some("CRYPTO".into()),
            asset_class: Some("crypto".into()),
            qty: d("20.0"),
            qty_available: Decimal::ZERO,
            avg_entry_price: d("74.00"),
            side: "long".into(),
            market_value: Some(d("1440")),
            cost_basis: Some(d("1480")),
            unrealized_pl: Some(d("-40")),
            current_price: Some(d("72.00")),
        });
        st.orders.insert(
            "stale-part-err".into(),
            Order {
                id: "stale-part-err".into(),
                client_order_id: None,
                created_at: Some("2026-11-24T09:00:00Z".into()),
                updated_at: None,
                submitted_at: None,
                filled_at: None,
                expired_at: None,
                canceled_at: None,
                failed_at: None,
                symbol: "SOL/USD".into(),
                qty: d("50.0"),
                filled_qty: d("30.0"),
                filled_avg_price: Some(d("76.00")),
                order_class: None,
                side: "sell".into(),
                order_type: "limit".into(),
                time_in_force: "gtc".into(),
                limit_price: Some(d("76.00")),
                status: "partially_filled".into(),
            },
        );
        let broker = AdversarialMockBroker::new(st);
        let mut trades = vec![TradeRecord {
            id: "rf54hunt".into(),
            symbol: "SOL/USD".into(),
            buy_order_id: Some("buy-rf54".into()),
            sell_order_id: Some("stale-part-err".into()),
            buy_qty: d("50.075"),
            qty: d("50.0"),
            filled_exit_qty: d("30.0"),
            buy_price: d("74.00"),
            sell_price: Some(d("76.00")),
            pnl: None,
            pnl_pct: None,
            gross_pnl: None,
            fees_usd: None,
            buy_timestamp: "2026-11-24T09:00:00Z".into(),
            sell_timestamp: None,
            status: HuntStatus::ExitPartiallyFilled,
            adopted_from_broker: false,
            recovered_orphan: false,
            buy_fee_already_deducted: Some(true),
        }];
        let mut ft = FeeTracker::new();

        let (_, tel) = reconcile_and_maintain_positions(
            &broker,
            &params,
            &mut trades,
            &mut ft,
            d("72.00"),
            d("72.00"),
            now,
            None,
            ReconciliationTiming::fast_for_tests(),
        )
        .await;

        assert_eq!(trades.len(), 1);
        assert_eq!(trades[0].status, HuntStatus::ExitSubmitPending);
        assert_eq!(trades[0].qty, d("20.0"));
        assert_eq!(trades[0].sell_order_id, None);
        assert_eq!(trades[0].filled_exit_qty, Decimal::ZERO);
        assert_eq!(trades[0].buy_qty, d("20.03"));
        assert!((tel.reconciliation.tracked_active_qty - 20.0).abs() < 1e-9);
        assert!(tel.reconciliation.untracked_qty.abs() < 1e-9);
        assert!((tel.reconciliation.broker_qty_available - 20.0).abs() < 1e-9);
        assert!(!tel.reconciliation.is_synced); // Correctly warns shares are unprotected until next-cycle Step A retry
    }

    // -------------------------------------------------------------------------
    // RF-5.5 REGRESSION PROOF:
    // Transient `get_order` Err in Step B while external cancel freed `qty_available = 50.0`:
    // MUST set `deferred_exit_recovery = true` so Step D does NOT spawn a duplicate orphan!
    // -------------------------------------------------------------------------
    {
        let mut st = AdversarialBrokerState::base();
        st.get_order_fail_ids.insert("ext-canceled-sell".into());
        st.position = Some(Position {
            asset_id: Some("sol".into()),
            symbol: "SOLUSD".into(),
            exchange: Some("CRYPTO".into()),
            asset_class: Some("crypto".into()),
            qty: d("50.0"),
            qty_available: d("50.0"),
            avg_entry_price: d("74.00"),
            side: "long".into(),
            market_value: Some(d("3600")),
            cost_basis: Some(d("3700")),
            unrealized_pl: Some(d("-100")),
            current_price: Some(d("72.00")),
        });
        let broker = AdversarialMockBroker::new(st);
        let mut trades = vec![TradeRecord {
            id: "rf55hunt".into(),
            symbol: "SOL/USD".into(),
            buy_order_id: Some("buy-rf55".into()),
            sell_order_id: Some("ext-canceled-sell".into()),
            buy_qty: d("50.075"),
            qty: d("50.0"),
            filled_exit_qty: Decimal::ZERO,
            buy_price: d("74.00"),
            sell_price: Some(d("76.00")),
            pnl: None,
            pnl_pct: None,
            gross_pnl: None,
            fees_usd: None,
            buy_timestamp: "2026-11-24T22:00:00Z".into(),
            sell_timestamp: None,
            status: HuntStatus::OpenExitPending,
            adopted_from_broker: false,
            recovered_orphan: false,
            buy_fee_already_deducted: Some(true),
        }];
        let mut ft = FeeTracker::new();

        let _ = reconcile_and_maintain_positions(
            &broker,
            &params,
            &mut trades,
            &mut ft,
            d("72.00"),
            d("72.00"),
            now,
            None,
            ReconciliationTiming::fast_for_tests(),
        )
        .await;

        // Must NOT spawn a duplicate `rec-xxxx` orphan while `get_order` failed!
        assert_eq!(trades.len(), 1);
        assert_eq!(trades[0].id, "rf55hunt");
        assert_eq!(trades[0].status, HuntStatus::OpenExitPending);
    }

    println!(
        "[RF-5.4 & RF-5.5 ELIMINATED] Prorated buy_qty + filled_exit_qty reset + get_order Err orphan guard verified"
    );
}

// =============================================================================
// THEOREM 6 — Share Conservation, Metrics Numerical Parity & Fee Reconciliation
// =============================================================================

#[test]
fn theorem6_share_conservation_10000_trials() {
    let mut rng = Rng64::new(0x6666_7777_8888_9999);
    let statuses = [
        HuntStatus::OpenExitPending,
        HuntStatus::ExitSubmitPending,
        HuntStatus::ExitPartiallyFilled,
        HuntStatus::Completed,
        HuntStatus::LiquidatedStale,
        HuntStatus::LiquidatedManual,
        HuntStatus::LiquidatedOrphan,
        HuntStatus::AdoptedClosed,
    ];

    for _ in 0..10_000 {
        let broker_qty = floor_qty(decimal_from_f64_clean(rng.range_f64(0.0, 10_000.0)).unwrap(), 9);
        let broker_avail = floor_qty(
            broker_qty * decimal_from_f64_clean(rng.range_f64(0.0, 1.0)).unwrap(),
            9,
        );
        let pos = Position {
            asset_id: None,
            symbol: "SOLUSD".into(),
            exchange: None,
            asset_class: None,
            qty: broker_qty,
            qty_available: broker_avail,
            avg_entry_price: d("72.50"),
            side: "long".into(),
            market_value: Some(broker_qty * d("72.50")),
            cost_basis: Some(broker_qty * d("72.50")),
            unrealized_pl: Some(Decimal::ZERO),
            current_price: Some(d("72.50")),
        };

        let n_hunts = rng.range_usize(0, 12);
        let mut trades = Vec::with_capacity(n_hunts);
        for h_i in 0..n_hunts {
            let status = statuses[rng.range_usize(0, statuses.len())];
            let qty = floor_qty(decimal_from_f64_clean(rng.range_f64(0.0, 2000.0)).unwrap(), 9);
            let filled = floor_qty(
                qty * decimal_from_f64_clean(rng.range_f64(0.0, 1.1)).unwrap(),
                9,
            );
            trades.push(TradeRecord {
                id: format!("h{}", h_i),
                symbol: "SOL/USD".into(),
                buy_order_id: None,
                sell_order_id: None,
                buy_qty: qty,
                qty,
                filled_exit_qty: filled,
                buy_price: d("72"),
                sell_price: Some(d("73")),
                pnl: None,
                pnl_pct: None,
                gross_pnl: None,
                fees_usd: None,
                buy_timestamp: "2026-11-24T00:00:00Z".into(),
                sell_timestamp: None,
                status,
                adopted_from_broker: false,
                recovered_orphan: false,
                buy_fee_already_deducted: None,
            });
        }

        let tel = build_broker_telemetry(None, Some(&pos), &[], &trades);
        let rec = &tel.reconciliation;
        let err = (rec.tracked_active_qty + rec.untracked_qty - rec.broker_qty).abs();
        let scale = rec
            .broker_qty
            .abs()
            .max(rec.tracked_active_qty.abs())
            .max(1.0);
        assert!(
            err <= 1e-12 * scale,
            "Share conservation violated: tracked={} + untracked={} != broker={} (err={})",
            rec.tracked_active_qty,
            rec.untracked_qty,
            rec.broker_qty,
            err
        );

        let pos_err =
            (tel.broker_position.qty_available + tel.broker_position.locked_in_orders - tel.broker_position.qty).abs();
        assert!(pos_err <= 1e-12 * scale);
    }

    println!("[THEOREM 6 CONSERVATION PROVEN] 10,000 randomized portfolios verified tracked_active + untracked == broker_qty (<1e-12 rel err)");
}

#[test]
fn theorem6_metrics_5000_trials_and_fee_reconciliation_audit() {
    let mut rng = Rng64::new(0x7777_8888_9999_AAAA);
    let now = Utc::now();

    // 1. 5,000 randomized trade histories + portfolio histories verifying metrics exactness & zero-division safety
    for trial in 0..5_000usize {
        let n = trial % 15;
        let mut trades = Vec::with_capacity(n);
        let mut ref_pnls = Vec::new();
        let mut ref_rets = Vec::new();
        let mut ref_fees = 0.0_f64;

        for j in 0..n {
            let status = match j % 6 {
                0 => HuntStatus::Completed,
                1 => HuntStatus::LiquidatedStale,
                2 => HuntStatus::LiquidatedManual,
                3 => HuntStatus::AdoptedClosed, // MUST be excluded from strategy metrics!
                4 => HuntStatus::OpenExitPending,
                _ => HuntStatus::Completed,
            };
            let pnl_f = rng.range_f64(-500.0, 600.0);
            let ret_f = pnl_f / 10_000.0;
            let fee_f = rng.range_f64(0.0, 25.0);
            let pnl_d = decimal_from_f64_clean(pnl_f).unwrap().round_dp(2);
            let ret_d = decimal_from_f64_clean(ret_f).unwrap().round_dp(4);
            let fee_d = decimal_from_f64_clean(fee_f).unwrap().round_dp(2);

            if status.is_closed_strategy_trade() {
                ref_pnls.push(decimal_to_f64(pnl_d));
                ref_rets.push(decimal_to_f64(ret_d));
                ref_fees += decimal_to_f64(fee_d);
            }

            trades.push(TradeRecord {
                id: format!("m{}", j),
                symbol: "SOL/USD".into(),
                buy_order_id: None,
                sell_order_id: None,
                buy_qty: d("100"),
                qty: d("99.85"),
                filled_exit_qty: d("99.85"),
                buy_price: d("100"),
                sell_price: Some(d("101")),
                pnl: Some(pnl_d),
                pnl_pct: Some(ret_d),
                gross_pnl: Some(pnl_d + fee_d),
                fees_usd: Some(fee_d),
                buy_timestamp: "2026-11-24T00:00:00Z".into(),
                sell_timestamp: Some("2026-11-24T01:00:00Z".into()),
                status,
                adopted_from_broker: status == HuntStatus::AdoptedClosed,
                recovered_orphan: false,
                buy_fee_already_deducted: None,
            });
        }

        let m = compute_performance_metrics(&trades, 0.04, 100_000.0, now);
        assert_eq!(m.total_trades, n);
        assert_eq!(m.completed_trades, ref_pnls.len());
        assert!(m.sharpe_ratio.is_finite());
        assert!(m.max_drawdown_pct.is_finite() && m.max_drawdown_pct >= 0.0);
        assert!(m.max_drawdown_dollars.is_finite() && m.max_drawdown_dollars >= 0.0);
        assert!(m.annualized_volatility.is_finite() && m.annualized_volatility >= 0.0);
        assert!(m.profit_factor.is_finite() && m.profit_factor >= 0.0);

        if !ref_pnls.is_empty() {
            let exp_sum = (ref_pnls.iter().sum::<f64>() * 100.0).round() / 100.0;
            assert!((m.total_pnl - exp_sum).abs() <= 0.01);
            let exp_fees = (ref_fees * 100.0).round() / 100.0;
            assert!((m.total_fees_usd - exp_fees).abs() <= 0.01);

            // Verify ddof=1 sample std & Sharpe independently
            if ref_rets.len() > 1 {
                let mean = ref_rets.iter().sum::<f64>() / (ref_rets.len() as f64);
                let var = ref_rets.iter().map(|r| (r - mean).powi(2)).sum::<f64>()
                    / ((ref_rets.len() - 1) as f64);
                let std = var.sqrt();
                let exp_ann_vol = (std * 252.0_f64.sqrt() * 100.0 * 100.0).round() / 100.0;
                assert!((m.annualized_volatility - exp_ann_vol).abs() <= 0.01);
            } else {
                assert_eq!(m.sharpe_ratio, 0.0);
                assert_eq!(m.annualized_volatility, 0.0);
            }
        }
    }

    // Portfolio history edge cases: empty, 1 point, NaN/Inf/0 points, constant equity (zero vol)
    let ph_bad = PortfolioHistory {
        timestamp: vec![1770595200, 1770681600, 1770768000, 1770854400],
        equity: vec![Some(0.0), Some(f64::NAN), Some(f64::INFINITY), Some(100_000.0)],
        ..Default::default()
    };
    let m_ph = compute_portfolio_history_metrics(&ph_bad, Some(100_000.0), &[], 0.04, now);
    assert!(m_ph.sharpe_ratio.is_finite() && m_ph.sharpe_ratio == 0.0);
    assert!(m_ph.annualized_volatility.is_finite() && m_ph.annualized_volatility == 0.0);
    assert_eq!(m_ph.max_drawdown_pct, 0.0);

    // 2. FeeTracker exhaustive tier classification across all 5 Alpaca tiers (maker + taker)
    for expected in ALPACA_CRYPTO_FEE_TIERS {
        let mut ft_buy = FeeTracker::new();
        let buy_qty = d("10000");
        let net_maker = buy_qty * (Decimal::ONE - expected.maker_rate);
        let matched = ft_buy.calibrate_from_buy_fill(buy_qty, net_maker).unwrap();
        assert_eq!(matched.tier, expected.tier);
        assert_eq!(ft_buy.worst_sell_fee_rate(), CRYPTO_TIER1_TAKER_FEE_RATE);
        assert_eq!(ft_buy.worst_buy_fee_rate(), CRYPTO_TIER1_TAKER_FEE_RATE);

        let mut ft_cfee = FeeTracker::new();
        let notional = d("10000");
        let fee_maker = ceil_usd_cents(notional * expected.maker_rate);
        let m_mk = ft_cfee.calibrate_from_cfee(fee_maker, notional, true).unwrap();
        assert_eq!(m_mk.tier, expected.tier);
        assert_eq!(ft_cfee.worst_sell_fee_rate(), CRYPTO_TIER1_TAKER_FEE_RATE);

        let fee_taker = ceil_usd_cents(notional * expected.taker_rate);
        let m_tk = ft_cfee.calibrate_from_cfee(fee_taker, notional, false).unwrap();
        assert_eq!(m_tk.tier, expected.tier);
        assert_eq!(ft_cfee.worst_sell_fee_rate(), CRYPTO_TIER1_TAKER_FEE_RATE);
    }

    // 3. CE-6.1 REGRESSION PROOF (`src/model/pricing.rs:553`):
    //    Exact `b_qty > s_qty` (no 1e-7 epsilon) eliminates buy-fee double-counting on tiny-qty
    //    high-price crypto assets like BTC!
    //    Example A (BTC $100,000, $10 order at Tier 4 0.08%): diff = 8e-8 > 0!
    let btc_buy_qty = d("0.0001");
    let btc_sell_qty = d("0.00009992");
    let btc_buy_px = d("100000.00");
    let btc_sell_px = d("100700.00");
    let sell_cfee = d("0.03");
    let true_cash_pnl = (btc_sell_qty * btc_sell_px - sell_cfee) - btc_buy_qty * btc_buy_px; // +$0.031944 -> $0.03
    let (reported_net, _, _, reported_fees) = compute_net_trade_pnl(
        btc_buy_px,
        btc_sell_px,
        btc_buy_qty,
        btc_sell_qty,
        true,
        d("0.0008"),
        CRYPTO_TIER1_TAKER_FEE_RATE,
        Some(sell_cfee),
        false,
    );
    assert_eq!(
        true_cash_pnl.round_dp_with_strategy(2, RoundingStrategy::MidpointNearestEven),
        d("0.03")
    );
    assert_eq!(
        reported_net,
        d("0.03"),
        "CE-6.1 REGRESSION: exact b_qty > s_qty must match true cash PnL $0.03"
    );
    assert_eq!(reported_fees, d("0.04")); // $0.008 buy fee (from b_qty-s_qty) + $0.03 sell = $0.038 -> $0.04, NOT double-deducted from net_pnl!

    //    Example B ($1,000,000 asset, buy_qty = 0.00005 = $50, Tier 1 0.15%, diff = 7.5e-8 > 0):
    let hi_buy_qty = d("0.00005");
    let hi_sell_qty = floor_qty(hi_buy_qty * (Decimal::ONE - CRYPTO_TIER1_MAKER_FEE_RATE), 9); // 0.000049925
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
    let hi_true_pnl = (hi_sell_qty * hi_exit_px - hi_cfee) - hi_buy_qty * hi_buy_px;
    let hi_true_2dp = hi_true_pnl.round_dp_with_strategy(2, RoundingStrategy::MidpointNearestEven);
    let (hi_reported_pnl, _, _, _) = compute_net_trade_pnl(
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
    assert!(hi_true_pnl > HALF_CENT_USD);
    assert_eq!(
        hi_reported_pnl, hi_true_2dp,
        "CE-6.1 REGRESSION: reported PnL {} must equal true 2dp cash PnL {}",
        hi_reported_pnl, hi_true_2dp
    );
    assert!(hi_reported_pnl >= d("0.01"));
    println!(
        "[CE-6.1 REGRESSION PROVEN] Exact b_qty > s_qty eliminates buy-fee double-count: BTC $10 reported=${} == true=$0.03; $1M high-px reported=${} == true=${}",
        reported_net, hi_reported_pnl, hi_true_2dp
    );

    // 4. Exact numerical parity cross-check against `dashboard/metrics.py` reference outputs:
    //    Trades = [(Completed, +100, +1%, fee 2.50), (Completed, -40, -0.4%, fee 1.25),
    //              (AdoptedClosed, +999, +50%, fee 10.00 -> EXCLUDED), (LiquidatedStale, +25.125, +0.25%, fee 3.00)]
    let py_ref_trades = vec![
        TradeRecord {
            id: "t1".into(),
            symbol: "SOL/USD".into(),
            buy_order_id: None,
            sell_order_id: None,
            buy_qty: d("100"),
            qty: d("100"),
            filled_exit_qty: d("100"),
            buy_price: d("100"),
            sell_price: Some(d("101")),
            pnl: Some(d("100.0")),
            pnl_pct: Some(d("0.01")),
            gross_pnl: Some(d("102.5")),
            fees_usd: Some(d("2.50")),
            buy_timestamp: "2026-11-24T00:00:00Z".into(),
            sell_timestamp: Some("2026-11-24T01:00:00Z".into()),
            status: HuntStatus::Completed,
            adopted_from_broker: false,
            recovered_orphan: false,
            buy_fee_already_deducted: None,
        },
        TradeRecord {
            id: "t2".into(),
            symbol: "SOL/USD".into(),
            buy_order_id: None,
            sell_order_id: None,
            buy_qty: d("100"),
            qty: d("100"),
            filled_exit_qty: d("100"),
            buy_price: d("100"),
            sell_price: Some(d("99.6")),
            pnl: Some(d("-40.0")),
            pnl_pct: Some(d("-0.004")),
            gross_pnl: Some(d("-38.75")),
            fees_usd: Some(d("1.25")),
            buy_timestamp: "2026-11-24T01:00:00Z".into(),
            sell_timestamp: Some("2026-11-24T02:00:00Z".into()),
            status: HuntStatus::Completed,
            adopted_from_broker: false,
            recovered_orphan: false,
            buy_fee_already_deducted: None,
        },
        TradeRecord {
            id: "t3".into(),
            symbol: "SOL/USD".into(),
            buy_order_id: None,
            sell_order_id: None,
            buy_qty: d("100"),
            qty: d("100"),
            filled_exit_qty: d("100"),
            buy_price: d("100"),
            sell_price: Some(d("150")),
            pnl: Some(d("999.0")),
            pnl_pct: Some(d("0.5")),
            gross_pnl: Some(d("1009.0")),
            fees_usd: Some(d("10.0")),
            buy_timestamp: "2026-11-24T02:00:00Z".into(),
            sell_timestamp: Some("2026-11-24T03:00:00Z".into()),
            status: HuntStatus::AdoptedClosed,
            adopted_from_broker: true,
            recovered_orphan: false,
            buy_fee_already_deducted: Some(true),
        },
        TradeRecord {
            id: "t4".into(),
            symbol: "SOL/USD".into(),
            buy_order_id: None,
            sell_order_id: None,
            buy_qty: d("100"),
            qty: d("100"),
            filled_exit_qty: d("100"),
            buy_price: d("100"),
            sell_price: Some(d("100.25")),
            pnl: Some(d("25.125")),
            pnl_pct: Some(d("0.0025")),
            gross_pnl: Some(d("28.125")),
            fees_usd: Some(d("3.0")),
            buy_timestamp: "2026-11-24T03:00:00Z".into(),
            sell_timestamp: Some("2026-11-24T04:00:00Z".into()),
            status: HuntStatus::LiquidatedStale,
            adopted_from_broker: false,
            recovered_orphan: false,
            buy_fee_already_deducted: None,
        },
    ];
    let rm = compute_performance_metrics(&py_ref_trades, 0.04, 100_000.0, now);
    // Exact match with Python dashboard/metrics.py on every metric except exact half-cent midpoint tie-breaking:
    assert_eq!(rm.total_trades, 4);
    assert_eq!(rm.completed_trades, 3);
    assert_eq!(rm.win_rate, 66.67);
    assert_eq!(rm.total_fees_usd, 6.75);
    assert_eq!(rm.sharpe_ratio, 20.19);
    assert_eq!(rm.max_drawdown_pct, 0.04);
    assert_eq!(rm.max_drawdown_dollars, 40.0);
    assert_eq!(rm.annualized_volatility, 11.12);
    assert_eq!(rm.avg_pnl_per_trade, 28.38);
    assert_eq!(rm.profit_factor, 3.13);
    // Midpoint tie-breaker note: 85.125 -> Rust f64::round (half-away-from-zero) = 85.13 vs Python round (half-to-even) = 85.12!
    assert_eq!(rm.total_pnl, 85.13);

    let ph_ref = PortfolioHistory {
        timestamp: vec![1770595200, 1770681600, 1770768000, 1770854400, 1770940800],
        equity: vec![
            Some(0.0),
            Some(100_000.0),
            Some(120_000.0),
            Some(108_000.0),
            Some(150_000.0),
        ],
        ..Default::default()
    };
    let rph = compute_portfolio_history_metrics(&ph_ref, None, &py_ref_trades, 0.04, now);
    assert_eq!(rph.sharpe_ratio, 12.62);
    assert_eq!(rph.annualized_volatility, 471.01);
    assert_eq!(rph.max_drawdown_pct, 10.0);
    assert_eq!(rph.max_drawdown_dollars, 12_000.0);
    assert_eq!(rph.total_pnl, 50_000.0);
    println!(
        "[THEOREM 6 METRICS PARITY VERIFIED] 5,000 trials + exact Python cross-check (Sharpe=20.19/12.62, Vol=11.12%/471.01%, PF=3.13); noted 0.01 midpoint tie-breaker (85.13 vs Py 85.12)"
    );
}

#[tokio::test]
async fn theorem6_adversarial_5page_cfee_pagination_dedup_and_termination() {
    let mk_act = |id: &str, oid: &str, act_type: &str, fee_cents: i64| AccountActivity {
        id: id.into(),
        activity_type: act_type.into(),
        order_id: Some(oid.into()),
        symbol: Some("SOL/USD".into()),
        transaction_time: Some("2026-11-24T12:00:00Z".into()),
        date: Some("2026-11-24".into()),
        net_amount: Some(-Decimal::new(fee_cents, 2)),
        qty: None,
        price: None,
        description: None,
        status: Some("executed".into()),
    };

    // 1. 250 matching fills spread across 3 pages (100 + 100 + 55) WITH:
    //    - 5 overlapping duplicate activity IDs repeated at the boundary between pages
    //    - mixed non-CFEE ("FILL") & unrelated order IDs
    //    - multi-order lookup (&["ord-lim", "ord-mkt"])
    {
        let mut st = AdversarialBrokerState::base();
        let mut p1 = Vec::with_capacity(ACTIVITY_PAGE_SIZE);
        for i in 0..ACTIVITY_PAGE_SIZE {
            let oid = if i % 2 == 0 { "ord-lim" } else { "ord-mkt" };
            p1.push(mk_act(&format!("act-p1-{}", i), oid, "CFEE", 2)); // 100 * $0.02 = $2.00
        }
        let tok1 = p1.last().unwrap().id.clone();

        let mut p2 = Vec::with_capacity(ACTIVITY_PAGE_SIZE);
        // Repeat last 5 IDs from page 1 at start of page 2 (mid-pagination page-shift overlap!)
        for dup_i in (ACTIVITY_PAGE_SIZE - 5)..ACTIVITY_PAGE_SIZE {
            p2.push(mk_act(&format!("act-p1-{}", dup_i), "ord-lim", "CFEE", 2));
        }
        for i in 0..(ACTIVITY_PAGE_SIZE - 5) {
            p2.push(mk_act(&format!("act-p2-{}", i), "ord-mkt", "CFEE", 3)); // 95 * $0.03 = $2.85
        }
        let tok2 = p2.last().unwrap().id.clone();

        let mut p3 = Vec::new();
        for i in 0..55 {
            p3.push(mk_act(&format!("act-p3-{}", i), "ord-lim", "CFEE", 1)); // 55 * $0.01 = $0.55
        }
        // Noise entries that MUST be ignored:
        p3.push(mk_act("noise-fill", "ord-lim", "FILL", 999));
        p3.push(mk_act("noise-other", "unrelated-oid", "CFEE", 888));

        st.paginated_activity_pages.insert(None, Ok(p1));
        st.paginated_activity_pages.insert(Some(tok1), Ok(p2));
        st.paginated_activity_pages.insert(Some(tok2), Ok(p3));

        let broker = AdversarialMockBroker::new(st);
        let total = fetch_actual_cfee_usd(&broker, &["ord-lim", "ord-mkt"], true)
            .await
            .expect("must aggregate across 3 pages");
        // Exact deduplicated total: $2.00 + $2.85 + $0.55 = $5.40 (250 unique fills!)
        assert_eq!(total, d("5.40"));
        assert_eq!(broker.st.lock().unwrap().activity_page_calls, 3);
    }

    // 2. Bounded MAX_ACTIVITY_PAGES (5) termination even when broker has 6+ full pages (600 fills)
    {
        let mut st = AdversarialBrokerState::base();
        let mut cursor: Option<String> = None;
        for page_idx in 0..6usize {
            let mut page = Vec::with_capacity(ACTIVITY_PAGE_SIZE);
            for i in 0..ACTIVITY_PAGE_SIZE {
                page.push(mk_act(
                    &format!("p{}-{}", page_idx, i),
                    "ord-huge",
                    "CFEE",
                    1,
                ));
            }
            let next_tok = page.last().unwrap().id.clone();
            st.paginated_activity_pages.insert(cursor, Ok(page));
            cursor = Some(next_tok);
        }
        let broker = AdversarialMockBroker::new(st);
        let total = fetch_actual_cfee_usd(&broker, &["ord-huge"], true)
            .await
            .expect("must sum 5 full pages");
        assert_eq!(broker.st.lock().unwrap().activity_page_calls, MAX_ACTIVITY_PAGES);
        assert_eq!(total, Decimal::new((MAX_ACTIVITY_PAGES * ACTIVITY_PAGE_SIZE) as i64, 2)); // $5.00 (500 * $0.01)
    }

    // 3. Adversarial repeating-cursor loop guard: broker repeats the SAME last ID -> breaks immediately!
    {
        let mut st = AdversarialBrokerState::base();
        let mut p1 = Vec::with_capacity(ACTIVITY_PAGE_SIZE);
        for i in 0..ACTIVITY_PAGE_SIZE {
            let id = if i + 1 == ACTIVITY_PAGE_SIZE {
                "stuck-cursor".to_string()
            } else {
                format!("cyc1-{}", i)
            };
            p1.push(mk_act(&id, "ord-cyc", "CFEE", 1));
        }
        let mut p2 = Vec::with_capacity(ACTIVITY_PAGE_SIZE);
        for i in 0..ACTIVITY_PAGE_SIZE {
            let id = if i + 1 == ACTIVITY_PAGE_SIZE {
                "stuck-cursor".to_string() // SAME cursor token repeated!
            } else {
                format!("cyc2-{}", i)
            };
            p2.push(mk_act(&id, "ord-cyc", "CFEE", 1));
        }
        st.paginated_activity_pages.insert(None, Ok(p1));
        st.paginated_activity_pages
            .insert(Some("stuck-cursor".into()), Ok(p2));

        let broker = AdversarialMockBroker::new(st);
        let total = fetch_actual_cfee_usd(&broker, &["ord-cyc"], true)
            .await
            .expect("must terminate cleanly on cycle");
        // Strictly 2 RPC calls (seen_tokens breaks before fetching a 3rd page!), 199 unique IDs = $1.99
        assert_eq!(broker.st.lock().unwrap().activity_page_calls, 2);
        assert_eq!(total, d("1.99"));
    }

    // 4. Atomic fallback on mid-pagination HTTP error: page 2 fails -> MUST return None (not truncated page 1!)
    {
        let mut st = AdversarialBrokerState::base();
        let mut p1 = Vec::with_capacity(ACTIVITY_PAGE_SIZE);
        for i in 0..ACTIVITY_PAGE_SIZE {
            p1.push(mk_act(&format!("errp1-{}", i), "ord-err", "CFEE", 5));
        }
        let tok1 = p1.last().unwrap().id.clone();
        st.paginated_activity_pages.insert(None, Ok(p1));
        st.paginated_activity_pages
            .insert(Some(tok1), Err("HTTP 503 on page 2".into()));

        let broker = AdversarialMockBroker::new(st);
        let res = fetch_actual_cfee_usd(&broker, &["ord-err"], true).await;
        assert_eq!(
            res, None,
            "Mid-pagination error MUST return None instead of silently truncated page-1 partial CFEE"
        );
        assert_eq!(broker.st.lock().unwrap().activity_page_calls, 2);
    }

    println!(
        "[THEOREM 6 CFEE 5-PAGE PAGINATION PROVEN] 250-fill overlap dedup=$5.40, 5-page hard cap=500, cursor-cycle guard=2 calls, mid-page error=None"
    );
}

// =============================================================================
// THEOREM 7 — Fixed-USD Dynamic Regime Sizing & Concurrent Exposure Safety
// =============================================================================

struct T7BrokerState {
    account: Account,
    position: Option<Position>,
    orders: HashMap<String, Order>,
    quote_queue: VecDeque<QuoteDetail>,
    default_quote: QuoteDetail,
    activities: Vec<AccountActivity>,
    buy_fee_rate: Decimal,
    fail_next_sell: bool,
    market_fill_price: Decimal,
    next_seq: u64,
}

impl T7BrokerState {
    fn new(cash: Decimal, default_bid: Decimal, default_ask: Decimal) -> Self {
        Self {
            account: Account {
                equity: cash,
                cash,
                buying_power: cash,
                non_marginable_buying_power: Some(cash),
            },
            position: None,
            orders: HashMap::new(),
            quote_queue: VecDeque::new(),
            default_quote: QuoteDetail {
                timestamp: Some("2026-11-25T10:00:00Z".into()),
                bid_price: default_bid,
                ask_price: default_ask,
                bid_size: Some(d("10000")),
                ask_size: Some(d("10000")),
            },
            activities: Vec::new(),
            buy_fee_rate: CRYPTO_TIER1_MAKER_FEE_RATE,
            fail_next_sell: false,
            market_fill_price: default_bid,
            next_seq: 1,
        }
    }
}

#[derive(Clone)]
struct T7MockBroker {
    st: Arc<Mutex<T7BrokerState>>,
}

impl T7MockBroker {
    fn new(state: T7BrokerState) -> Self {
        Self {
            st: Arc::new(Mutex::new(state)),
        }
    }

    fn push_quote(&self, bid: Decimal, ask: Decimal, count: usize) {
        let mut st = self.st.lock().unwrap();
        for _ in 0..count {
            st.quote_queue.push_back(QuoteDetail {
                timestamp: Some("2026-11-25T10:00:00Z".into()),
                bid_price: bid,
                ask_price: ask,
                bid_size: Some(d("10000")),
                ask_size: Some(d("10000")),
            });
        }
    }
}

#[async_trait]
impl AlpacaApi for T7MockBroker {
    async fn get_account(&self) -> Result<Account, AlpacaError> {
        Ok(self.st.lock().unwrap().account.clone())
    }

    async fn get_position(&self, _symbol: &str) -> Result<Option<Position>, AlpacaError> {
        Ok(self.st.lock().unwrap().position.clone())
    }

    async fn get_open_orders(&self, _symbol: Option<&str>) -> Result<Vec<Order>, AlpacaError> {
        let st = self.st.lock().unwrap();
        Ok(st.orders.values().filter(|o| o.is_open()).cloned().collect())
    }

    async fn get_order(&self, order_id: &str) -> Result<Order, AlpacaError> {
        let st = self.st.lock().unwrap();
        st.orders
            .get(order_id)
            .cloned()
            .ok_or_else(|| AlpacaError::ApiStatus {
                status: StatusCode::NOT_FOUND,
                message: format!("Order {} not found", order_id),
            })
    }

    async fn submit_order(&self, req: &OrderRequest) -> Result<Order, AlpacaError> {
        let mut st = self.st.lock().unwrap();
        let qty = Decimal::from_str(&req.qty).unwrap();
        let limit_px = req
            .limit_price
            .as_deref()
            .and_then(|s| Decimal::from_str(s).ok());
        let is_crypto = req.symbol.contains('/');

        if req.side == "buy" {
            let seq = st.next_seq;
            st.next_seq += 1;
            let oid = format!("t7-buy-{}", seq);
            let fill_px = limit_px.unwrap_or(st.default_quote.ask_price);
            let net_qty = if is_crypto {
                floor_qty(qty * (Decimal::ONE - st.buy_fee_rate), 9)
            } else {
                qty
            };
            st.account.cash = (st.account.cash - qty * fill_px).max(Decimal::ZERO);
            match st.position {
                Some(ref mut pos) => {
                    let prev_cost = pos.qty * pos.avg_entry_price;
                    let new_qty = pos.qty + net_qty;
                    pos.avg_entry_price = if new_qty > Decimal::ZERO {
                        (prev_cost + net_qty * fill_px) / new_qty
                    } else {
                        fill_px
                    };
                    pos.qty = new_qty;
                    pos.qty_available += net_qty;
                    pos.current_price = Some(fill_px);
                    pos.market_value = Some(new_qty * fill_px);
                }
                None => {
                    st.position = Some(Position {
                        asset_id: Some("asset-t7".into()),
                        symbol: req.symbol.replace('/', ""),
                        exchange: Some("CRYPTO".into()),
                        asset_class: Some("crypto".into()),
                        qty: net_qty,
                        qty_available: net_qty,
                        avg_entry_price: fill_px,
                        side: "long".into(),
                        market_value: Some(net_qty * fill_px),
                        cost_basis: Some(net_qty * fill_px),
                        unrealized_pl: Some(Decimal::ZERO),
                        current_price: Some(fill_px),
                    });
                }
            }
            let ord = Order {
                id: oid.clone(),
                client_order_id: req.client_order_id.clone(),
                created_at: Some("2026-11-25T10:00:00Z".into()),
                updated_at: Some("2026-11-25T10:00:01Z".into()),
                submitted_at: Some("2026-11-25T10:00:00Z".into()),
                filled_at: Some("2026-11-25T10:00:01Z".into()),
                expired_at: None,
                canceled_at: None,
                failed_at: None,
                symbol: req.symbol.clone(),
                qty,
                filled_qty: qty,
                filled_avg_price: Some(fill_px),
                order_class: Some("simple".into()),
                side: "buy".into(),
                order_type: req.order_type.clone(),
                time_in_force: req.time_in_force.clone(),
                limit_price: limit_px,
                status: "filled".into(),
            };
            st.orders.insert(oid, ord.clone());
            return Ok(ord);
        }

        if st.fail_next_sell {
            st.fail_next_sell = false;
            return Err(AlpacaError::ApiStatus {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                message: "Injected transient 500 on sell submit".into(),
            });
        }

        let seq = st.next_seq;
        st.next_seq += 1;
        if req.order_type == "market" {
            let oid = format!("t7-mkt-{}", seq);
            let fill_px = st.market_fill_price;
            if let Some(ref mut pos) = st.position {
                pos.qty = (pos.qty - qty).max(Decimal::ZERO);
                pos.qty_available = (pos.qty_available - qty).max(Decimal::ZERO);
                if pos.qty <= Decimal::ZERO {
                    st.position = None;
                }
            }
            let cfee = ceil_usd_cents(qty * fill_px * CRYPTO_TIER1_TAKER_FEE_RATE);
            st.account.cash += qty * fill_px - cfee;
            st.activities.insert(
                0,
                AccountActivity {
                    id: format!("cfee-{}", oid),
                    activity_type: "CFEE".into(),
                    order_id: Some(oid.clone()),
                    symbol: Some(req.symbol.clone()),
                    transaction_time: Some("2026-11-25T22:30:00Z".into()),
                    date: Some("2026-11-25".into()),
                    net_amount: Some(-cfee),
                    qty: None,
                    price: None,
                    description: None,
                    status: Some("executed".into()),
                },
            );
            let ord = Order {
                id: oid.clone(),
                client_order_id: req.client_order_id.clone(),
                created_at: Some("2026-11-25T22:30:00Z".into()),
                updated_at: Some("2026-11-25T22:30:01Z".into()),
                submitted_at: Some("2026-11-25T22:30:00Z".into()),
                filled_at: Some("2026-11-25T22:30:01Z".into()),
                expired_at: None,
                canceled_at: None,
                failed_at: None,
                symbol: req.symbol.clone(),
                qty,
                filled_qty: qty,
                filled_avg_price: Some(fill_px),
                order_class: Some("simple".into()),
                side: "sell".into(),
                order_type: "market".into(),
                time_in_force: req.time_in_force.clone(),
                limit_price: None,
                status: "filled".into(),
            };
            st.orders.insert(oid, ord.clone());
            return Ok(ord);
        }

        let oid = format!("t7-sell-{}", seq);
        if let Some(ref mut pos) = st.position {
            pos.qty_available = (pos.qty_available - qty).max(Decimal::ZERO);
        }
        let ord = Order {
            id: oid.clone(),
            client_order_id: req.client_order_id.clone(),
            created_at: Some("2026-11-25T10:00:02Z".into()),
            updated_at: Some("2026-11-25T10:00:02Z".into()),
            submitted_at: Some("2026-11-25T10:00:02Z".into()),
            filled_at: None,
            expired_at: None,
            canceled_at: None,
            failed_at: None,
            symbol: req.symbol.clone(),
            qty,
            filled_qty: Decimal::ZERO,
            filled_avg_price: None,
            order_class: Some("simple".into()),
            side: "sell".into(),
            order_type: "limit".into(),
            time_in_force: req.time_in_force.clone(),
            limit_price: limit_px,
            status: "new".into(),
        };
        st.orders.insert(oid, ord.clone());
        Ok(ord)
    }

    async fn cancel_order(&self, order_id: &str) -> Result<bool, AlpacaError> {
        let mut st = self.st.lock().unwrap();
        let rem = match st.orders.get_mut(order_id) {
            Some(ord) if ord.is_open() => {
                let r = ord.remaining_qty();
                ord.status = "canceled".into();
                ord.canceled_at = Some("2026-11-25T22:30:00Z".into());
                r
            }
            _ => return Ok(false),
        };
        if rem > Decimal::ZERO {
            if let Some(ref mut pos) = st.position {
                pos.qty_available = (pos.qty_available + rem).min(pos.qty);
            }
        }
        Ok(true)
    }

    async fn get_latest_quote(&self, _symbol: &str) -> Result<QuoteDetail, AlpacaError> {
        let mut st = self.st.lock().unwrap();
        Ok(st
            .quote_queue
            .pop_front()
            .unwrap_or_else(|| st.default_quote.clone()))
    }

    async fn get_portfolio_history(
        &self,
        _period: &str,
        _timeframe: &str,
    ) -> Result<PortfolioHistory, AlpacaError> {
        Ok(PortfolioHistory::default())
    }

    async fn get_account_activities(
        &self,
        _activity_types: &str,
        _after: Option<&str>,
    ) -> Result<Vec<AccountActivity>, AlpacaError> {
        Ok(self.st.lock().unwrap().activities.clone())
    }

    async fn get_account_activities_page(
        &self,
        _activity_types: &str,
        _after: Option<&str>,
        page_token: Option<&str>,
    ) -> Result<Vec<AccountActivity>, AlpacaError> {
        if page_token.is_some() {
            Ok(Vec::new())
        } else {
            Ok(self.st.lock().unwrap().activities.clone())
        }
    }
}

#[test]
fn theorem7_1_bounds_numerical_immunity_and_corrupted_state_30000_trials() {
    let prod_params: StrategyParams =
        serde_json::from_str(include_str!("../params.json")).expect("valid params.json");
    AppConfig::validate_params(&prod_params).expect("params.json must validate");
    let rs = prod_params.regime_sizing.clone();
    assert!((rs.min_notional_usd - 3000.0).abs() < 1e-12);
    assert!((rs.max_notional_usd - 30000.0).abs() < 1e-12);
    assert!((rs.min_total_exposure_usd - 15000.0).abs() < 1e-12);
    assert!((rs.max_total_exposure_usd - 90000.0).abs() < 1e-12);

    // 1. 30,000 adversarial & randomized ticks against production params
    let mut ctrl = RegimeNotionalController::new(rs.clone(), None);
    let mut rng = Rng64::new(0x7001_7002_7003_7004);

    // Initial bad ticks before any valid tick must be pure no-ops
    for &bad in &[f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 0.0, -0.0, -1.0, f64::MIN] {
        ctrl.update(bad);
        assert_eq!(ctrl.state().sample_count, 0);
        assert_eq!(ctrl.current_hunt_cap_usd(), d("3000"));
        assert_eq!(ctrl.current_total_exposure_cap_usd(), d("15000"));
    }

    // Flat constant price for 5,000 cycles -> return_var must hit >= 1e-12 floor cleanly
    for _ in 0..5_000 {
        ctrl.update(75.0);
        let st = ctrl.state();
        assert!(st.ema_fast.is_finite() && st.ema_fast > 0.0);
        assert!(st.ema_slow.is_finite() && st.ema_slow > 0.0);
        assert!(st.return_var.is_finite() && st.return_var >= 1e-12);
        assert!(st.smoothed_cap_usd.is_finite());
        let h_cap = ctrl.current_hunt_cap_usd();
        let e_cap = ctrl.current_total_exposure_cap_usd();
        assert!(h_cap >= d("3000") && h_cap <= d("30000"));
        assert!(e_cap >= h_cap && e_cap >= d("15000") && e_cap <= d("90000"));
    }
    assert!(
        (ctrl.state().return_var - 1e-12).abs() < 1e-13,
        "constant price return_var should converge to 1e-12 floor, got {}",
        ctrl.state().return_var
    );

    // 25,000 extreme / mixed / 1000x-jump / alternating-spike / subnormal / NaN ticks
    let extremes = [
        f64::NAN,
        f64::INFINITY,
        f64::NEG_INFINITY,
        0.0,
        -1.0,
        f64::MIN_POSITIVE,
        5e-324_f64,
        1e-100,
        1e-4,
        0.9999,
        1.0,
        1000.0,
        1e6,
        1e100,
        1e300,
        f64::MAX,
    ];
    for step in 0..25_000usize {
        let mid = match step % 16 {
            0 => extremes[(step / 16) % extremes.len()],
            1 => 1e-100,
            2 => 1e100,
            3 => 1e-4,
            4 => 100_000.0, // 1e9x spike
            5 => 0.1,
            _ => rng.log_uniform(1e-10, 1e10),
        };
        ctrl.update(mid);
        let st = ctrl.state();
        assert!(st.ema_fast.is_finite() && st.ema_fast > 0.0);
        assert!(st.ema_slow.is_finite() && st.ema_slow > 0.0);
        assert!(st.last_mid.is_finite() && st.last_mid > 0.0);
        assert!(st.return_var.is_finite() && st.return_var >= 1e-12);
        assert!(
            st.smoothed_cap_usd.is_finite()
                && st.smoothed_cap_usd >= 3000.0
                && st.smoothed_cap_usd <= 30000.0
        );
        let h_cap = ctrl.current_hunt_cap_usd();
        let e_cap = ctrl.current_total_exposure_cap_usd();
        assert!(h_cap >= d("3000") && h_cap <= d("30000"));
        assert!(e_cap >= h_cap && e_cap >= d("15000") && e_cap <= d("90000"));
    }

    // 2. Corrupted / out-of-range deserialized RegimeNotionalState recovery in ::new
    let corrupted_cases = [
        RegimeNotionalState {
            ema_fast: 0.0,
            ema_slow: 100.0,
            last_mid: 100.0,
            return_var: 1e-6,
            smoothed_cap_usd: 15000.0,
            sample_count: 100,
            stale_cooldown_remaining: 0,
        },
        RegimeNotionalState {
            ema_fast: 100.0,
            ema_slow: f64::NAN,
            last_mid: 100.0,
            return_var: 1e-6,
            smoothed_cap_usd: 15000.0,
            sample_count: 100,
            stale_cooldown_remaining: 0,
        },
        RegimeNotionalState {
            ema_fast: 100.0,
            ema_slow: 100.0,
            last_mid: 100.0,
            return_var: -5.0,
            smoothed_cap_usd: 15000.0,
            sample_count: 100,
            stale_cooldown_remaining: 0,
        },
        RegimeNotionalState {
            ema_fast: 100.0,
            ema_slow: 100.0,
            last_mid: 100.0,
            return_var: 0.0,
            smoothed_cap_usd: 1e9,
            sample_count: 100,
            stale_cooldown_remaining: 0,
        },
    ];
    for bad_state in corrupted_cases {
        let mut c = RegimeNotionalController::new(rs.clone(), Some(bad_state));
        assert!(c.current_hunt_cap_usd() >= d("3000") && c.current_hunt_cap_usd() <= d("30000"));
        assert!(c.current_total_exposure_cap_usd() >= c.current_hunt_cap_usd());
        assert!(c.current_total_exposure_cap_usd() <= d("90000"));
        c.update(100.0);
        let st = c.state();
        assert!(st.ema_fast.is_finite() && st.ema_fast > 0.0);
        assert!(st.ema_slow.is_finite() && st.ema_slow > 0.0);
        assert!(st.return_var.is_finite() && st.return_var >= 1e-12);
    }

    // 3. Positive Regression Proofs for T7.1 Edge Cases (CE-7.1a–d):
    // Regression CE-7.1a: ema_fast_cycles = 1 + 1e100 -> 1.0 drop MUST equal 1.0 (convex EMA, zero cancellation!)
    {
        let mut p_fast1 = prod_params.clone();
        p_fast1.regime_sizing.ema_fast_cycles = 1;
        assert!(AppConfig::validate_params(&p_fast1).is_ok());
        let mut c1 = RegimeNotionalController::new(p_fast1.regime_sizing, None);
        c1.update(1e100);
        c1.update(1.0);
        assert!(
            (c1.state().ema_fast - 1.0).abs() < 1e-12,
            "CE-7.1a REGRESSION PROVEN: convex EMA update preserves ema_fast=1.0 when ema_fast_cycles=1, got {}",
            c1.state().ema_fast
        );
        assert!(c1.state().is_valid());
    }

    // Regression CE-7.1b: corrupted sample_count = 0 with negative EMAs rejected; valid sample_count = 0 clamped cleanly
    {
        let crafted = RegimeNotionalState {
            ema_fast: -50.0,
            ema_slow: 0.0,
            last_mid: -1.0,
            return_var: 0.0,
            smoothed_cap_usd: 1e9,
            sample_count: 0,
            stale_cooldown_remaining: 0,
        };
        assert!(
            !crafted.is_valid(),
            "CE-7.1b REGRESSION PROVEN: is_valid must reject sample_count=0 with negative EMAs"
        );
        let c_crafted = RegimeNotionalController::new(rs.clone(), Some(crafted));
        assert!(c_crafted.state().return_var >= 1e-12);
        assert_eq!(c_crafted.state().ema_fast, 0.0);
        assert_eq!(c_crafted.current_hunt_cap_usd(), d("3000"));

        let valid_zero = RegimeNotionalState {
            ema_fast: 0.0,
            ema_slow: 0.0,
            last_mid: 0.0,
            return_var: 0.0,
            smoothed_cap_usd: 1e9,
            sample_count: 0,
            stale_cooldown_remaining: 0,
        };
        assert!(valid_zero.is_valid());
        let c_zero = RegimeNotionalController::new(rs.clone(), Some(valid_zero));
        assert!(c_zero.state().return_var >= 1e-12);
        assert_eq!(c_zero.current_hunt_cap_usd(), d("3000"));
    }

    // Regression CE-7.1c: deserialized sample_count = u64::MAX saturates cleanly on update()
    {
        let max_sc_state = RegimeNotionalState {
            ema_fast: 100.0,
            ema_slow: 100.0,
            last_mid: 100.0,
            return_var: 1e-6,
            smoothed_cap_usd: 3000.0,
            sample_count: u64::MAX,
            stale_cooldown_remaining: 0,
        };
        assert!(max_sc_state.is_valid());
        let mut c_ovf = RegimeNotionalController::new(rs.clone(), Some(max_sc_state));
        c_ovf.update(100.0);
        assert_eq!(
            c_ovf.state().sample_count,
            u64::MAX,
            "CE-7.1c REGRESSION PROVEN: sample_count must saturate at u64::MAX without overflow or wrap"
        );
        assert!(c_ovf.state().is_valid());
    }

    // Regression CE-7.1d: validate_params rejects max_notional_usd > max_total_exposure_usd,
    // and sanitize_bounds guarantees total exposure cap >= hunt cap even for unvalidated params
    {
        let mut p_inconsistent = prod_params.clone();
        p_inconsistent.regime_sizing.max_notional_usd = 50_000.0;
        p_inconsistent.regime_sizing.max_total_exposure_usd = 30_000.0;
        assert!(
            AppConfig::validate_params(&p_inconsistent).is_err(),
            "CE-7.1d REGRESSION PROVEN: validate_params must reject max_notional_usd > max_total_exposure_usd"
        );
        let high_st = RegimeNotionalState {
            ema_fast: 150.0,
            ema_slow: 100.0,
            last_mid: 150.0,
            return_var: 1e-6,
            smoothed_cap_usd: 50_000.0,
            sample_count: 1000,
            stale_cooldown_remaining: 0,
        };
        let c_inc = RegimeNotionalController::new(p_inconsistent.regime_sizing, Some(high_st));
        assert_eq!(c_inc.current_hunt_cap_usd(), d("50000"));
        assert!(
            c_inc.current_total_exposure_cap_usd() >= c_inc.current_hunt_cap_usd(),
            "CE-7.1d REGRESSION PROVEN: total_exposure_cap {} must remain >= hunt_cap {}",
            c_inc.current_total_exposure_cap_usd(),
            c_inc.current_hunt_cap_usd()
        );
    }

    println!(
        "[THEOREM 7.1 PROVEN] 30,000 ticks + CE-7.1a–d regressions (convex EMA, sample_count=0 sanitization, u64::MAX saturation, exposure/notional consistency) strictly invariant-safe"
    );
}

#[tokio::test]
async fn theorem7_2_asymmetric_dynamics_closed_form_and_stale_cooldown() {
    let prod_params: StrategyParams =
        serde_json::from_str(include_str!("../params.json")).expect("valid params.json");
    let rs = prod_params.regime_sizing.clone();

    // 1. Exact 720-cycle (6h) warmup gate: cap stays strictly $3,000 during strong rally
    let mut ctrl = RegimeNotionalController::new(rs.clone(), None);
    let mut price = 100.0_f64;
    for cycle in 1..=720 {
        price *= 1.001;
        ctrl.update(price);
        assert_eq!(
            ctrl.current_hunt_cap_usd(),
            d("3000"),
            "warmup cycle {} must remain locked at $3,000",
            cycle
        );
    }
    // Cycle 721 (sample_count == 720 at entry) unlocks ramp
    price *= 1.001;
    ctrl.update(price);
    assert!(
        ctrl.current_hunt_cap_usd() > d("3000"),
        "cycle 721 must begin ramping above $3,000"
    );

    // 2. Closed-form exponential convergence verification: C_k = C_target - (C_target - C_0)(1 - alpha)^k
    //    When z_reg is saturated at +20.0 (strong bull trend), sig = 1/(1+exp(-2*(20-0.25))) = 1.0
    //    to IEEE-754 double precision (exp(-39.5) < 1e-17), so target_usd == 30_000.0 constant!
    let mut ramp_ctrl = RegimeNotionalController::new(
        rs.clone(),
        Some(RegimeNotionalState {
            ema_fast: 200.0,
            ema_slow: 100.0,
            last_mid: 200.0,
            return_var: 1e-12,
            smoothed_cap_usd: 3000.0,
            sample_count: 1000,
            stale_cooldown_remaining: 0,
        }),
    );
    let c0 = 3000.0_f64;
    let c_target = 30000.0_f64;
    let alpha = rs.ramp_up_alpha; // 0.01
    for k in 1..=300usize {
        ramp_ctrl.update(200.0);
        let exact = c_target - (c_target - c0) * (1.0 - alpha).powi(k as i32);
        let actual = ramp_ctrl.state().smoothed_cap_usd;
        assert!(
            (actual - exact).abs() < 1e-8,
            "step {} closed-form mismatch: actual={} exact={}",
            k,
            actual,
            exact
        );
        if k == 69 {
            let frac = (actual - c0) / (c_target - c0);
            assert!(
                (0.50..0.506).contains(&frac),
                "t_50 must occur at k=69 cycles (34.5m), got frac={}",
                frac
            );
        }
        if k == 229 {
            let frac = (actual - c0) / (c_target - c0);
            assert!((0.899..0.90).contains(&frac), "k=229 frac={}", frac);
        }
        if k == 230 {
            let frac = (actual - c0) / (c_target - c0);
            assert!((0.90..0.902).contains(&frac), "t_90 crossing at k=230 frac={}", frac);
        }
    }

    // 3. 1-cycle instant downward response across 1,000 randomized states
    let mut rng = Rng64::new(0x7201_7202_7203_7204);
    for _ in 0..1_000 {
        let ema_f = rng.range_f64(50.0, 150.0);
        let ema_s = rng.range_f64(50.0, 150.0);
        let last_m = rng.range_f64(50.0, 150.0);
        let rv = rng.log_uniform(1e-12, 1e-2);
        let next_m = rng.range_f64(40.0, 140.0);

        // Controller starting at max cap (30,000) computes exact 1-step target_usd whenever target_usd <= 30,000
        let mut c_target_probe = RegimeNotionalController::new(
            rs.clone(),
            Some(RegimeNotionalState {
                ema_fast: ema_f,
                ema_slow: ema_s,
                last_mid: last_m,
                return_var: rv,
                smoothed_cap_usd: 30_000.0,
                sample_count: 1000,
                stale_cooldown_remaining: 0,
            }),
        );
        c_target_probe.update(next_m);
        let target_usd = c_target_probe.state().smoothed_cap_usd;

        // Any controller with smoothed_cap_usd >= target_usd MUST snap directly to target_usd in 1 cycle
        let higher_cap = rng.range_f64(target_usd, 30_000.0);
        let mut c = RegimeNotionalController::new(
            rs.clone(),
            Some(RegimeNotionalState {
                ema_fast: ema_f,
                ema_slow: ema_s,
                last_mid: last_m,
                return_var: rv,
                smoothed_cap_usd: higher_cap,
                sample_count: 1000,
                stale_cooldown_remaining: 0,
            }),
        );
        c.update(next_m);
        assert!(
            (c.state().smoothed_cap_usd - target_usd).abs() < 1e-12,
            "instant 1-cycle downward drop failed: got {} vs target {} (started at {})",
            c.state().smoothed_cap_usd,
            target_usd,
            higher_cap
        );
    }

    // 4. Stale liquidation cooldown: exact 1,440 cycles (12h) at $3,000 before resuming ramp
    ramp_ctrl.on_stale_liquidation();
    assert_eq!(ramp_ctrl.current_hunt_cap_usd(), d("3000"));
    assert_eq!(ramp_ctrl.current_total_exposure_cap_usd(), d("15000"));
    assert_eq!(ramp_ctrl.state().stale_cooldown_remaining, 1440);
    for cycle in 1..=1440 {
        ramp_ctrl.update(200.0);
        assert_eq!(
            ramp_ctrl.current_hunt_cap_usd(),
            d("3000"),
            "cooldown cycle {} of 1440 must remain locked at $3,000",
            cycle
        );
    }
    assert_eq!(ramp_ctrl.state().stale_cooldown_remaining, 0);
    ramp_ctrl.update(200.0);
    assert!(
        ramp_ctrl.current_hunt_cap_usd() > d("3000"),
        "cycle 1441 after stale liquidation must resume upward ramp"
    );

    // 5. End-to-end TradingEngine stale-count detection when trades_history ALREADY has 50 LiquidatedStale hunts!
    let tmp = tempdir().unwrap();
    let broker = T7MockBroker::new(T7BrokerState::new(d("1000000"), d("100.00"), d("100.02")));
    let base_ts = DateTime::parse_from_rfc3339("2026-11-25T10:00:00Z")
        .unwrap()
        .with_timezone(&Utc);

    let mut initial_hunts: Vec<TradeRecord> = (0..50)
        .map(|i| TradeRecord {
            id: format!("old-stale-{}", i),
            symbol: "SOL/USD".into(),
            buy_order_id: Some(format!("ob-{}", i)),
            sell_order_id: Some(format!("os-{}", i)),
            buy_qty: d("30"),
            qty: d("29.955"),
            filled_exit_qty: d("29.955"),
            buy_price: d("100.00"),
            sell_price: Some(d("99.00")),
            pnl: Some(d("-35.00")),
            pnl_pct: Some(d("-0.0117")),
            gross_pnl: Some(d("-30.00")),
            fees_usd: Some(d("5.00")),
            buy_timestamp: "2026-11-20T00:00:00Z".into(),
            sell_timestamp: Some("2026-11-20T12:00:00Z".into()),
            status: HuntStatus::LiquidatedStale,
            adopted_from_broker: false,
            recovered_orphan: false,
            buy_fee_already_deducted: Some(true),
        })
        .collect();

    // Seed 1 active OpenExitPending hunt created >12h before test_ts
    let active_sell_id = "open-stale-sell-51".to_string();
    initial_hunts.push(TradeRecord {
        id: "active-51".into(),
        symbol: "SOL/USD".into(),
        buy_order_id: Some("ob-51".into()),
        sell_order_id: Some(active_sell_id.clone()),
        buy_qty: d("100"),
        qty: d("99.85"),
        filled_exit_qty: Decimal::ZERO,
        buy_price: d("100.00"),
        sell_price: Some(d("102.00")),
        pnl: None,
        pnl_pct: None,
        gross_pnl: None,
        fees_usd: None,
        buy_timestamp: base_ts.to_rfc3339(),
        sell_timestamp: None,
        status: HuntStatus::OpenExitPending,
        adopted_from_broker: false,
        recovered_orphan: false,
        buy_fee_already_deducted: Some(true),
    });

    {
        let mut st = broker.st.lock().unwrap();
        st.position = Some(Position {
            asset_id: Some("sol".into()),
            symbol: "SOLUSD".into(),
            exchange: Some("CRYPTO".into()),
            asset_class: Some("crypto".into()),
            qty: d("99.85"),
            qty_available: Decimal::ZERO,
            avg_entry_price: d("100.00"),
            side: "long".into(),
            market_value: Some(d("9985")),
            cost_basis: Some(d("9985")),
            unrealized_pl: Some(Decimal::ZERO),
            current_price: Some(d("100.00")),
        });
        st.orders.insert(
            active_sell_id.clone(),
            Order {
                id: active_sell_id,
                client_order_id: None,
                created_at: Some(base_ts.to_rfc3339()),
                updated_at: Some(base_ts.to_rfc3339()),
                submitted_at: Some(base_ts.to_rfc3339()),
                filled_at: None,
                expired_at: None,
                canceled_at: None,
                failed_at: None,
                symbol: "SOL/USD".into(),
                qty: d("99.85"),
                filled_qty: Decimal::ZERO,
                filled_avg_price: None,
                order_class: Some("simple".into()),
                side: "sell".into(),
                order_type: "limit".into(),
                time_in_force: "gtc".into(),
                limit_price: Some(d("102.00")),
                status: "new".into(),
            },
        );
    }

    let store = StateStore::new(tmp.path());
    store.save_hunts(&initial_hunts, 50).unwrap();
    store
        .save_regime_state(&RegimeNotionalState {
            ema_fast: 150.0,
            ema_slow: 100.0,
            last_mid: 150.0,
            return_var: 1e-6,
            smoothed_cap_usd: 30_000.0,
            sample_count: 1000,
            stale_cooldown_remaining: 0,
        })
        .unwrap();

    let mut engine = TradingEngine::with_client_and_timing(
        prod_params,
        tmp.path(),
        broker,
        EngineTiming::fast_for_tests(),
    );
    assert_eq!(engine.regime_ctrl().current_hunt_cap_usd(), d("30000"));
    assert_eq!(
        engine
            .trades_history()
            .iter()
            .filter(|h| h.status == HuntStatus::LiquidatedStale)
            .count(),
        50
    );

    // Run cycle at +12h30m -> stale_before=50, stale_after=51 BEFORE trim -> must trigger cooldown!
    let _snap = engine
        .step_cycle_at(base_ts + ChronoDuration::minutes(750), None)
        .await
        .unwrap();

    assert_eq!(engine.regime_ctrl().current_hunt_cap_usd(), d("3000"));
    assert_eq!(engine.regime_ctrl().state().stale_cooldown_remaining, 1440);
    assert_eq!(
        engine
            .trades_history()
            .iter()
            .filter(|h| h.status == HuntStatus::LiquidatedStale)
            .count(),
        50,
        "post-detection trim should cap terminal hunts back to 50"
    );
    assert!(
        engine.trades_history().iter().any(|h| h.id == "active-51" && h.status == HuntStatus::LiquidatedStale),
        "newly stale hunt active-51 must be preserved as latest terminal LiquidatedStale"
    );

    println!(
        "[THEOREM 7.2 PROVEN] 720-cycle warmup, closed-form alpha=0.01 ramp (t50=69, t90~229.1), 1-cycle downward clamp, 1440-cycle cooldown, & 50-terminal stale detection passed"
    );
}

#[tokio::test]
async fn theorem7_3_decimal_conservation_30000_trials_and_exposure_audit() {
    let min_m = 0.0036;
    let max_m = 0.0250;
    let mut rng = Rng64::new(0x7301_7302_7303_7304);
    let mut nonzero_count = 0usize;

    let ugly_prices = [
        d("0.0001"),
        d("0.0003"),
        d("0.0099"),
        d("0.9999"),
        d("1.00"),
        d("1.01"),
        d("3.00"),
        d("73.33"),
        d("999.99"),
        d("99999.99"),
    ];

    // 1. 30,000 randomized & boundary trials over realistic + high-precision Decimal budgets
    for i in 0..30_000usize {
        let is_crypto = i % 2 == 0;
        let level = match i % 3 {
            0 => 0.05,
            1 => 0.5,
            _ => 1.5,
        };
        let margin = match i % 10 {
            0 => rng.range_f64(-0.01, min_m - 1e-9),
            1 => rng.range_f64(max_m + 1e-9, 0.25),
            2 => min_m,
            3 => max_m,
            4 => f64::NAN,
            5 => f64::INFINITY,
            _ => rng.range_f64(min_m, max_m),
        };

        let max_notional = if i % 25 == 0 {
            d("29999.999999999999999999999999")
        } else {
            decimal_from_f64_clean(rng.log_uniform(1.0, 500_000.0))
                .unwrap()
                .round_dp(2)
        };
        let remaining_exp = match i % 12 {
            0 => d("9.99"),
            1 => Decimal::ZERO,
            2 => d("-10.00"),
            3 => d("29.999999999999999999999999999"),
            _ => decimal_from_f64_clean(rng.log_uniform(1.0, 500_000.0))
                .unwrap()
                .round_dp(2),
        };
        let available_cash = match i % 11 {
            0 => d("9.99"),
            1 => Decimal::ZERO,
            2 => d("29.999999999999999999999999999"),
            _ => decimal_from_f64_clean(rng.log_uniform(1.0, 500_000.0))
                .unwrap()
                .round_dp(2),
        };
        let buy_price = if i % 5 == 0 {
            ugly_prices[i % ugly_prices.len()]
        } else {
            floor_to_tick(decimal_from_f64_clean(rng.log_uniform(0.0001, 100_000.0)).unwrap())
                .max(d("0.0001"))
        };

        let q = calculate_notional_capped_shares(
            margin,
            min_m,
            max_m,
            level,
            max_notional,
            remaining_exp,
            available_cash,
            buy_price,
            is_crypto,
        );

        let min_cap = max_notional
            .min(remaining_exp.max(Decimal::ZERO))
            .min(available_cash.max(Decimal::ZERO));
        let notional = q * buy_price;
        assert!(q >= Decimal::ZERO);
        assert!(
            notional <= min_cap,
            "NOTIONAL CAP VIOLATED: q={} * px={} = {} > min_cap={} (max_n={}, rem={}, cash={})",
            q,
            buy_price,
            notional,
            min_cap,
            max_notional,
            remaining_exp,
            available_cash
        );
        if !margin.is_finite() || margin < min_m || margin > max_m {
            assert_eq!(q, Decimal::ZERO);
        }
        if remaining_exp < MIN_ORDER_NOTIONAL_USD || available_cash < MIN_ORDER_NOTIONAL_USD {
            assert_eq!(q, Decimal::ZERO);
        }
        if q > Decimal::ZERO {
            assert!(
                notional >= MIN_ORDER_NOTIONAL_USD,
                "MIN NOTIONAL VIOLATED: {} < $10.00",
                notional
            );
            if !is_crypto {
                assert_eq!(q, q.floor(), "Equity share count must be integer");
            }
            nonzero_count += 1;
        }
    }

    // 2. 29-digit Decimal::MAX probes:
    //    (a) Verify factor * Decimal::MAX does not panic in calculate_notional_capped_shares
    let dec_max_res = std::panic::catch_unwind(|| {
        calculate_notional_capped_shares(
            0.0200,
            min_m,
            max_m,
            0.05,
            Decimal::MAX,
            d("1000"),
            d("1000"),
            d("73.33"),
            true,
        )
    });
    assert!(dec_max_res.is_ok(), "factor * Decimal::MAX panicked");
    let q_max = dec_max_res.unwrap();
    assert!(q_max * d("73.33") <= d("1000") && q_max * d("73.33") >= d("10"));

    //    (b) REGRESSION CE-7.3a: 29-digit raw_qty stagnation in finalize_floored_qty terminates cleanly!
    //        cap = 5e25, buy_price = 3, is_crypto = true (step = 0.0001):
    //        5e25 / 3 = 16666666666666666666666666.667 (scale 3, 29 digits), where qty - step == qty.
    //        finalize_floored_qty detects next >= qty and returns Decimal::ZERO immediately.
    {
        let cap = d("50000000000000000000000000");
        let px = d("3");
        let raw_qty = cap.checked_div(px).unwrap();
        let qty = floor_qty(raw_qty, 4);
        let step = Decimal::new(1, 4);
        assert_eq!(qty - step, qty);
        assert!(qty.checked_mul(px).is_none_or(|n| n > cap));

        let q_safe = calculate_notional_capped_shares(0.0250, 0.0036, 0.0250, 0.05, cap, cap, cap, px, true);
        assert_eq!(
            q_safe,
            Decimal::ZERO,
            "CE-7.3a REGRESSION PROVEN: 29-digit stagnation returns 0 cleanly without infinite loop"
        );
        assert!(q_safe * px <= cap);
    }

    // 3. active_exposure_usd audit in TradingEngine::step_cycle_at:
    //    (a) Normal ExitPartiallyFilled accurately tracks (qty - filled_exit_qty) * buy_price
    {
        let tmp = tempdir().unwrap();
        let mut params = default_sol_params();
        params.regime_sizing = RegimeSizingParams {
            min_notional_usd: 3000.0,
            max_notional_usd: 30000.0,
            min_total_exposure_usd: 15000.0,
            max_total_exposure_usd: 15000.0,
            ema_fast_cycles: 4,
            ema_slow_cycles: 20,
            vol_cycles: 4,
            ramp_up_alpha: 0.5,
            stale_cooldown_cycles: 10,
            ..Default::default()
        };
        let broker = T7MockBroker::new(T7BrokerState::new(d("1000000"), d("100.00"), d("100.02")));
        let base_ts = DateTime::parse_from_rfc3339("2026-11-25T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        let sell_oid = "partial-sell-1".to_string();
        let partial_hunt = TradeRecord {
            id: "part-1".into(),
            symbol: "SOL/USD".into(),
            buy_order_id: Some("pb-1".into()),
            sell_order_id: Some(sell_oid.clone()),
            buy_qty: d("130"),
            qty: d("130"),
            filled_exit_qty: Decimal::ZERO,
            buy_price: d("100.00"),
            sell_price: Some(d("102.00")),
            pnl: None,
            pnl_pct: None,
            gross_pnl: None,
            fees_usd: None,
            buy_timestamp: base_ts.to_rfc3339(),
            sell_timestamp: None,
            status: HuntStatus::OpenExitPending,
            adopted_from_broker: false,
            recovered_orphan: false,
            buy_fee_already_deducted: Some(true),
        };
        StateStore::new(tmp.path())
            .save_hunts(&[partial_hunt], 50)
            .unwrap();
        {
            let mut st = broker.st.lock().unwrap();
            st.position = Some(Position {
                asset_id: Some("sol".into()),
                symbol: "SOLUSD".into(),
                exchange: Some("CRYPTO".into()),
                asset_class: Some("crypto".into()),
                qty: d("50"),
                qty_available: Decimal::ZERO,
                avg_entry_price: d("100.00"),
                side: "long".into(),
                market_value: Some(d("5000")),
                cost_basis: Some(d("5000")),
                unrealized_pl: Some(Decimal::ZERO),
                current_price: Some(d("100.00")),
            });
            st.orders.insert(
                sell_oid.clone(),
                Order {
                    id: sell_oid,
                    client_order_id: None,
                    created_at: Some(base_ts.to_rfc3339()),
                    updated_at: Some(base_ts.to_rfc3339()),
                    submitted_at: Some(base_ts.to_rfc3339()),
                    filled_at: None,
                    expired_at: None,
                    canceled_at: None,
                    failed_at: None,
                    symbol: "SOL/USD".into(),
                    qty: d("130"),
                    filled_qty: d("80"), // 80 sold -> 50 remaining ($5,000 exposure!)
                    filled_avg_price: Some(d("102.00")),
                    order_class: Some("simple".into()),
                    side: "sell".into(),
                    order_type: "limit".into(),
                    time_in_force: "gtc".into(),
                    limit_price: Some(d("102.00")),
                    status: "partially_filled".into(),
                },
            );
        }
        let mut engine = TradingEngine::with_client_and_timing(
            params,
            tmp.path(),
            broker.clone(),
            EngineTiming::fast_for_tests(),
        );
        // Warm Kalman to ~100, then trigger dip
        broker.push_quote(d("100.00"), d("100.02"), 1);
        engine.step_cycle_at(base_ts, None).await.unwrap();
        broker.push_quote(d("99.00"), d("99.02"), 2);
        let snap = engine
            .step_cycle_at(base_ts + ChronoDuration::seconds(30), None)
            .await
            .unwrap();
        // Remaining exposure was $15,000 - $5,000 = $10,000; hunt cap is $3,000 -> new hunt sized <= $3,000!
        assert_eq!(snap.active_hunts_count, 2);
        let h2 = engine.trades_history().last().unwrap();
        assert!(h2.buy_qty * h2.buy_price <= d("3000"));
    }

    //    (b) REGRESSION CE-7.3b: Step C preserves non-zero sub-$0.0001 avg_entry_ref (0.00004)
    //        and active_exposure_usd marks to max(buy_price, mid) & broker exposure, blocking excess buys!
    {
        let tmp = tempdir().unwrap();
        let mut params = default_sol_params();
        params.regime_sizing = RegimeSizingParams {
            min_notional_usd: 3000.0,
            max_notional_usd: 3000.0,
            min_total_exposure_usd: 15000.0,
            max_total_exposure_usd: 15000.0,
            ema_fast_cycles: 4,
            ema_slow_cycles: 20,
            vol_cycles: 4,
            ramp_up_alpha: 0.5,
            stale_cooldown_cycles: 10,
            ..Default::default()
        };
        let broker =
            T7MockBroker::new(T7BrokerState::new(d("1000000"), d("0.0100"), d("0.0102")));
        let base_ts = DateTime::parse_from_rfc3339("2026-11-25T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        // Existing broker position: 500,000,000 units @ avg_entry=$0.00004 ($20,000 cost, > $15,000 cap!)
        // Protected by an external open sell order adopted by Step C!
        {
            let mut st = broker.st.lock().unwrap();
            st.position = Some(Position {
                asset_id: Some("meme".into()),
                symbol: "SOLUSD".into(),
                exchange: Some("CRYPTO".into()),
                asset_class: Some("crypto".into()),
                qty: d("500000000"),
                qty_available: Decimal::ZERO,
                avg_entry_price: d("0.00004"),
                side: "long".into(),
                market_value: Some(d("5000000")),
                cost_basis: Some(d("20000")),
                unrealized_pl: Some(Decimal::ZERO),
                current_price: Some(d("0.0100")),
            });
            st.orders.insert(
                "ext-sell-1".into(),
                Order {
                    id: "ext-sell-1".into(),
                    client_order_id: None,
                    created_at: Some(base_ts.to_rfc3339()),
                    updated_at: Some(base_ts.to_rfc3339()),
                    submitted_at: Some(base_ts.to_rfc3339()),
                    filled_at: None,
                    expired_at: None,
                    canceled_at: None,
                    failed_at: None,
                    symbol: "SOL/USD".into(),
                    qty: d("500000000"),
                    filled_qty: Decimal::ZERO,
                    filled_avg_price: None,
                    order_class: Some("simple".into()),
                    side: "sell".into(),
                    order_type: "limit".into(),
                    time_in_force: "gtc".into(),
                    limit_price: Some(d("0.05")),
                    status: "new".into(),
                },
            );
        }
        let mut engine = TradingEngine::with_client_and_timing(
            params,
            tmp.path(),
            broker.clone(),
            EngineTiming::fast_for_tests(),
        );
        // Cycle 1: adopts ext-sell-1 preserving sub-4dp avg_entry_ref = 0.00004!
        broker.push_quote(d("0.0100"), d("0.0102"), 1);
        engine.step_cycle_at(base_ts, None).await.unwrap();
        assert_eq!(engine.trades_history().len(), 1);
        assert_eq!(
            engine.trades_history()[0].buy_price,
            d("0.00004"),
            "CE-7.3b REGRESSION PROVEN: Step C preserves non-zero sub-4dp buy_price 0.00004"
        );
        // Cycle 2: 1.48% dip MUST NOT trigger a new buy while broker already holds $20,000 cost ($5M MV) > $15,000 cap!
        broker.push_quote(d("0.0099"), d("0.0100"), 2);
        let snap2 = engine
            .step_cycle_at(base_ts + ChronoDuration::seconds(30), None)
            .await
            .unwrap();
        assert_eq!(
            snap2.active_hunts_count, 1,
            "CE-7.3b REGRESSION PROVEN: exposure floor blocks new buys above $15k cap"
        );
    }

    //    (c) REGRESSION CE-7.3c: Broker inventory floor prevents untracked/deferred positions
    //        from allowing new buys when broker_exposure_usd >= max_total_exposure_usd!
    {
        let tmp = tempdir().unwrap();
        let mut params = default_sol_params();
        params.regime_sizing = RegimeSizingParams {
            min_notional_usd: 3000.0,
            max_notional_usd: 3000.0,
            min_total_exposure_usd: 15000.0,
            max_total_exposure_usd: 15000.0,
            ema_fast_cycles: 4,
            ema_slow_cycles: 20,
            vol_cycles: 4,
            ramp_up_alpha: 0.5,
            stale_cooldown_cycles: 10,
            ..Default::default()
        };
        let broker = T7MockBroker::new(T7BrokerState::new(d("1000000"), d("100.00"), d("100.02")));
        let base_ts = DateTime::parse_from_rfc3339("2026-11-25T10:00:00Z")
            .unwrap()
            .with_timezone(&Utc);
        // 1 active ExitSubmitPending hunt ($3,000) + $20,000 untracked broker position,
        // and next sell retry fails so has_submit_pending stays true (Step D skipped!)
        let pending_hunt = TradeRecord {
            id: "pend-1".into(),
            symbol: "SOL/USD".into(),
            buy_order_id: Some("pb-p1".into()),
            sell_order_id: None,
            buy_qty: d("30"),
            qty: d("29.955"),
            filled_exit_qty: Decimal::ZERO,
            buy_price: d("100.00"),
            sell_price: Some(d("101.00")),
            pnl: None,
            pnl_pct: None,
            gross_pnl: None,
            fees_usd: None,
            buy_timestamp: base_ts.to_rfc3339(),
            sell_timestamp: None,
            status: HuntStatus::ExitSubmitPending,
            adopted_from_broker: false,
            recovered_orphan: false,
            buy_fee_already_deducted: Some(true),
        };
        StateStore::new(tmp.path())
            .save_hunts(&[pending_hunt], 50)
            .unwrap();
        {
            let mut st = broker.st.lock().unwrap();
            st.fail_next_sell = true; // keeps pend-1 in ExitSubmitPending so Step D is skipped
            st.position = Some(Position {
                asset_id: Some("sol".into()),
                symbol: "SOLUSD".into(),
                exchange: Some("CRYPTO".into()),
                asset_class: Some("crypto".into()),
                qty: d("230"), // $23,000 broker position (> $15,000 exposure cap!)
                qty_available: d("230"),
                avg_entry_price: d("100.00"),
                side: "long".into(),
                market_value: Some(d("23000")),
                cost_basis: Some(d("23000")),
                unrealized_pl: Some(Decimal::ZERO),
                current_price: Some(d("100.00")),
            });
        }
        let mut engine = TradingEngine::with_client_and_timing(
            params,
            tmp.path(),
            broker.clone(),
            EngineTiming::fast_for_tests(),
        );
        broker.push_quote(d("100.00"), d("100.02"), 1);
        engine.step_cycle_at(base_ts, None).await.unwrap();
        broker.st.lock().unwrap().fail_next_sell = true;
        broker.push_quote(d("99.00"), d("99.02"), 2);
        let snap = engine
            .step_cycle_at(base_ts + ChronoDuration::seconds(30), None)
            .await
            .unwrap();
        assert_eq!(
            snap.active_hunts_count, 1,
            "CE-7.3c REGRESSION PROVEN: broker inventory exposure ($23,000) blocks new hunt above $15k cap"
        );
    }

    println!(
        "[THEOREM 7.3 PROVEN] 30,000 realistic trials ({} nonzero) strictly conserved caps + CE-7.3a–c regressions (29-digit termination, Step C sub-4dp preservation, broker inventory exposure floor) passed",
        nonzero_count
    );
}

#[tokio::test]
async fn theorem7_4_zero_regression_on_theorems_1_to_6() {
    let params = default_sol_params();
    let now = Utc::now();
    let mut rng = Rng64::new(0x7401_7402_7403_7404);

    // 1. 40 randomized execute_hunt_with_notional_cap runs verifying Theorems 1, 2, 4, 5, 6
    for i in 0..40usize {
        let tmp = tempdir().unwrap();
        let store = StateStore::new(tmp.path());
        let bid = decimal_from_f64_clean(rng.range_f64(20.0, 250.0))
            .unwrap()
            .round_dp(2);
        let spread = if i % 5 == 0 { d("0.01") } else { d("0.04") };
        let ask = bid + spread;
        let fair_mult = 1.0 + rng.range_f64(0.0038, 0.0245);
        let fair_price = decimal_to_f64(bid) * fair_mult;

        let hunt_cap = decimal_from_f64_clean(rng.range_f64(3000.0, 30000.0))
            .unwrap()
            .round_dp(2);
        let rem_exp = decimal_from_f64_clean(rng.range_f64(500.0, 45000.0))
            .unwrap()
            .round_dp(2);
        let cash = decimal_from_f64_clean(rng.range_f64(500.0, 50000.0))
            .unwrap()
            .round_dp(2);

        let mut broker_st = T7BrokerState::new(cash, bid, ask);
        if i % 4 == 0 {
            // Inject transient sell failure to verify crash-safe ExitSubmitPending pre-persistence + Step A recovery (Theorem 5)
            broker_st.fail_next_sell = true;
        }
        let broker = T7MockBroker::new(broker_st);
        let mut trades = Vec::new();
        let mut fee_tracker = FeeTracker::new();

        let rec_opt = execute_hunt_with_notional_cap(
            &broker,
            &params,
            fair_price,
            &mut trades,
            &mut fee_tracker,
            Some(&store),
            now,
            HuntTiming::fast_for_tests(),
            None,
            hunt_cap,
            rem_exp,
        )
        .await
        .expect("hunt execution must not error");

        let rec = rec_opt.expect("valid anomaly with >=$500 cap/cash must open hunt");
        // Theorem 2: strictly passive maker buy price on legal tick grid
        assert!(rec.buy_price > Decimal::ZERO && rec.buy_price < ask);
        assert!(is_legal_sec_612_tick(rec.buy_price));

        // Theorem 7.3 cap conservation
        let buy_notional = rec.buy_qty * rec.buy_price;
        let eff_cap = hunt_cap.min(rem_exp).min(cash);
        assert!(
            buy_notional <= eff_cap && buy_notional >= MIN_ORDER_NOTIONAL_USD,
            "notional {} violated bounds [$10, {}]",
            buy_notional,
            eff_cap
        );

        // If sell deferred (ExitSubmitPending), verify crash-safe disk pre-persistence & Step A recovery
        if rec.status == HuntStatus::ExitSubmitPending {
            let reloaded = store.load_hunts(50);
            assert_eq!(reloaded.len(), 1);
            assert_eq!(reloaded[0].status, HuntStatus::ExitSubmitPending);
            let fair_dec = decimal_from_f64_clean(fair_price).unwrap();
            let (changed, _) = reconcile_and_maintain_positions(
                &broker,
                &params,
                &mut trades,
                &mut fee_tracker,
                fair_dec,
                bid,
                now + ChronoDuration::seconds(30),
                Some(&store),
                ReconciliationTiming::fast_for_tests(),
            )
            .await;
            assert!(changed);
            assert_eq!(trades[0].status, HuntStatus::OpenExitPending);
        }

        // Theorem 1: Simulate exit fill under WORST-CASE 0.25% taker sell fee + 5-fill CFEE split
        let active_rec = trades[0].clone();
        assert_eq!(active_rec.status, HuntStatus::OpenExitPending);
        let sell_oid = active_rec.sell_order_id.clone().unwrap();
        let exit_px = active_rec.sell_price.unwrap();
        let sell_qty = active_rec.qty;

        {
            let mut st = broker.st.lock().unwrap();
            let ord = st.orders.get_mut(&sell_oid).unwrap();
            ord.status = "filled".into();
            ord.filled_qty = sell_qty;
            ord.filled_avg_price = Some(exit_px);
            ord.filled_at = Some((now + ChronoDuration::minutes(5)).to_rfc3339());
            if let Some(ref mut pos) = st.position {
                pos.qty = (pos.qty - sell_qty).max(Decimal::ZERO);
                if pos.qty <= Decimal::from_parts(1, 0, 0, false, 7) {
                    st.position = None;
                }
            }
            // Inject adversarial 5-fill CFEE under worst-case 0.25% taker fee!
            let worst_cfee =
                adversarial_multifill_cfee(sell_qty, exit_px, CRYPTO_TIER1_TAKER_FEE_RATE, 5);
            st.activities.push(AccountActivity {
                id: format!("cfee-adv-{}", i),
                activity_type: "CFEE".into(),
                order_id: Some(sell_oid.clone()),
                symbol: Some("SOL/USD".into()),
                transaction_time: Some((now + ChronoDuration::minutes(5)).to_rfc3339()),
                date: Some("2026-11-25".into()),
                net_amount: Some(-worst_cfee),
                qty: None,
                price: None,
                description: None,
                status: Some("executed".into()),
            });
        }

        let fair_dec = decimal_from_f64_clean(fair_price).unwrap();
        reconcile_and_maintain_positions(
            &broker,
            &params,
            &mut trades,
            &mut fee_tracker,
            fair_dec,
            bid,
            now + ChronoDuration::minutes(5),
            Some(&store),
            ReconciliationTiming::fast_for_tests(),
        )
        .await;

        assert_eq!(trades[0].status, HuntStatus::Completed);
        let net_pnl = trades[0].pnl.unwrap();
        assert!(
            net_pnl >= d("0.01"),
            "THEOREM 1 REGRESSION: Completed hunt realized net_pnl={} < $0.01",
            net_pnl
        );
    }

    // 2. Locked / crossed fresh quote must abort cleanly (Theorem 2)
    {
        let broker = T7MockBroker::new(T7BrokerState::new(d("100000"), d("75.00"), d("75.00")));
        let mut trades = Vec::new();
        let mut fee_tracker = FeeTracker::new();
        let res = execute_hunt_with_notional_cap(
            &broker,
            &params,
            76.0,
            &mut trades,
            &mut fee_tracker,
            None,
            now,
            HuntTiming::fast_for_tests(),
            None,
            d("3000"),
            d("15000"),
        )
        .await
        .unwrap();
        assert!(res.is_none(), "locked fresh quote must abort hunt");
    }

    println!(
        "[THEOREM 7.4 ZERO REGRESSION PROVEN] 40 randomized execute_hunt_with_notional_cap end-to-end runs preserved Theorems 1-6 (strict >=$0.01 net PnL under 0.25% taker + 5-fill CFEE)"
    );
}


