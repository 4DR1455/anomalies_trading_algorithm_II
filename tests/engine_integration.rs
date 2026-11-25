use anomalies_trading_engine::alpaca::client::ACTIVITY_PAGE_SIZE;
use anomalies_trading_engine::alpaca::{
    Account, AccountActivity, AlpacaApi, AlpacaError, Order, OrderRequest, PortfolioHistory,
    Position, QuoteDetail,
};
use anomalies_trading_engine::config::{KalmanParams, RegimeSizingParams, StrategyParams};
use anomalies_trading_engine::engine::hunt::{execute_hunt, HuntTiming};
use anomalies_trading_engine::engine::reconciliation::{
    fetch_actual_cfee_usd, reconcile_and_maintain_positions, ReconciliationTiming,
};
use anomalies_trading_engine::engine::{EngineTiming, TradingEngine};
use anomalies_trading_engine::model::{
    ceil_usd_cents, compute_net_trade_pnl, floor_qty, FeeTracker, CRYPTO_TIER1_MAKER_FEE_RATE,
    CRYPTO_TIER1_TAKER_FEE_RATE,
};
use anomalies_trading_engine::state::{HuntStatus, StateStore, TradeRecord};
use async_trait::async_trait;
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use reqwest::StatusCode;
use rust_decimal::Decimal;
use std::collections::{HashMap, VecDeque};
use std::str::FromStr;
use std::sync::{Arc, Mutex};
use tempfile::tempdir;

fn d(s: &str) -> Decimal {
    Decimal::from_str(s).unwrap()
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
        regime_sizing: RegimeSizingParams::default(),
    }
}

#[derive(Debug, Clone)]
struct MockBrokerState {
    account: Account,
    position: Option<Position>,
    fail_position: bool,
    get_position_script: VecDeque<Result<Option<Position>, String>>,
    fail_submit_sell: bool,
    submit_sell_403_once: Option<Decimal>,
    cancel_leaves_pending: bool,
    market_sell_status_override: Option<String>,
    paginate_activities: bool,
    orders: HashMap<String, Order>,
    extra_stale_open_orders: Vec<Order>,
    quote_queue: VecDeque<QuoteDetail>,
    default_quote: QuoteDetail,
    portfolio_history: PortfolioHistory,
    activities: Vec<AccountActivity>,
    buy_fee_rate: Decimal,
    market_sell_id_override: Option<String>,
    market_sell_fill_price: Decimal,
    market_sell_cfee_rate: Decimal,
    inject_stale_after_market_sell: bool,
    submitted_requests: Vec<OrderRequest>,
    next_order_seq: u64,
}

impl Default for MockBrokerState {
    fn default() -> Self {
        Self {
            account: Account {
                equity: d("100000.00"),
                cash: d("100000.00"),
                buying_power: d("100000.00"),
                non_marginable_buying_power: Some(d("100000.00")),
            },
            position: None,
            fail_position: false,
            get_position_script: VecDeque::new(),
            fail_submit_sell: false,
            submit_sell_403_once: None,
            cancel_leaves_pending: false,
            market_sell_status_override: None,
            paginate_activities: false,
            orders: HashMap::new(),
            extra_stale_open_orders: Vec::new(),
            quote_queue: VecDeque::new(),
            default_quote: QuoteDetail {
                timestamp: Some("2026-11-24T10:00:00Z".to_string()),
                bid_price: d("72.00"),
                ask_price: d("72.02"),
                bid_size: Some(d("100")),
                ask_size: Some(d("100")),
            },
            portfolio_history: PortfolioHistory::default(),
            activities: Vec::new(),
            buy_fee_rate: CRYPTO_TIER1_MAKER_FEE_RATE,
            market_sell_id_override: None,
            market_sell_fill_price: d("73.50"),
            market_sell_cfee_rate: CRYPTO_TIER1_TAKER_FEE_RATE,
            inject_stale_after_market_sell: false,
            submitted_requests: Vec::new(),
            next_order_seq: 1,
        }
    }
}

#[derive(Clone, Default)]
struct MockAlpacaClient {
    state: Arc<Mutex<MockBrokerState>>,
}

impl MockAlpacaClient {
    fn new(state: MockBrokerState) -> Self {
        Self {
            state: Arc::new(Mutex::new(state)),
        }
    }

    /// Simulates an open limit SELL order filling at its limit price (or override) with exact `CFEE`.
    fn fill_sell_order(
        &self,
        order_id: &str,
        fill_price_override: Option<Decimal>,
        sell_fee_rate: Decimal,
        inject_stale_open_entry: bool,
    ) {
        let mut st = self.state.lock().unwrap();
        let (fill_px, fill_qty, stale_snapshot) = {
            let ord = st
                .orders
                .get_mut(order_id)
                .expect("order must exist to fill");
            let fill_px = fill_price_override
                .or(ord.limit_price)
                .expect("fill price required");
            let fill_qty = ord.qty;

            let stale_snapshot = if inject_stale_open_entry {
                let mut stale = ord.clone();
                stale.status = "new".to_string();
                Some(stale)
            } else {
                None
            };

            ord.status = "filled".to_string();
            ord.filled_qty = fill_qty;
            ord.filled_avg_price = Some(fill_px);
            ord.filled_at = Some("2026-11-24T10:05:00Z".to_string());
            (fill_px, fill_qty, stale_snapshot)
        };

        if let Some(stale) = stale_snapshot {
            st.extra_stale_open_orders.push(stale);
        }

        let gross = fill_qty * fill_px;
        let cfee = ceil_usd_cents(gross * sell_fee_rate);

        if let Some(ref mut pos) = st.position {
            pos.qty = (pos.qty - fill_qty).max(Decimal::ZERO);
            if pos.qty <= Decimal::from_parts(1, 0, 0, false, 7) {
                st.position = None;
            }
        }
        st.account.cash += gross - cfee;
        st.activities.insert(
            0,
            AccountActivity {
                id: format!("cfee-{}", order_id),
                activity_type: "CFEE".to_string(),
                order_id: Some(order_id.to_string()),
                symbol: Some("SOL/USD".to_string()),
                transaction_time: Some("2026-11-24T10:05:00Z".to_string()),
                date: Some("2026-11-24".to_string()),
                net_amount: Some(-cfee),
                qty: None,
                price: None,
                description: Some("Crypto fee".to_string()),
                status: Some("executed".to_string()),
            },
        );
    }
}

#[async_trait]
impl AlpacaApi for MockAlpacaClient {
    async fn get_account(&self) -> Result<Account, AlpacaError> {
        Ok(self.state.lock().unwrap().account.clone())
    }

    async fn get_position(&self, _symbol: &str) -> Result<Option<Position>, AlpacaError> {
        let mut st = self.state.lock().unwrap();
        if let Some(scripted) = st.get_position_script.pop_front() {
            return scripted.map_err(AlpacaError::Other);
        }
        if st.fail_position {
            return Err(AlpacaError::Other(
                "Simulated network timeout on GET /v2/positions".to_string(),
            ));
        }
        Ok(st.position.clone())
    }

    async fn get_open_orders(&self, _symbol: Option<&str>) -> Result<Vec<Order>, AlpacaError> {
        let st = self.state.lock().unwrap();
        let mut out: Vec<Order> = st
            .orders
            .values()
            .filter(|o| o.is_open())
            .cloned()
            .collect();
        out.extend(st.extra_stale_open_orders.clone());
        Ok(out)
    }

    async fn get_order(&self, order_id: &str) -> Result<Order, AlpacaError> {
        let st = self.state.lock().unwrap();
        st.orders
            .get(order_id)
            .cloned()
            .ok_or_else(|| AlpacaError::ApiStatus {
                status: StatusCode::NOT_FOUND,
                message: format!("Order {} not found", order_id),
            })
    }

    async fn submit_order(&self, req: &OrderRequest) -> Result<Order, AlpacaError> {
        let mut st = self.state.lock().unwrap();
        st.submitted_requests.push(req.clone());

        let qty = Decimal::from_str(&req.qty).unwrap();
        let limit_px = req
            .limit_price
            .as_deref()
            .and_then(|s| Decimal::from_str(s).ok());
        let is_crypto = req.symbol.contains('/');

        if req.side == "buy" {
            let seq = st.next_order_seq;
            st.next_order_seq += 1;
            let oid = format!("buy-ord-{:04}", seq);
            let fill_px = limit_px.unwrap_or(st.default_quote.ask_price);
            let net_qty = if is_crypto {
                floor_qty(qty * (Decimal::ONE - st.buy_fee_rate), 9)
            } else {
                qty
            };
            st.account.cash = (st.account.cash - qty * fill_px).max(Decimal::ZERO);

            match st.position {
                Some(ref mut pos) => {
                    let old_cost = pos.qty * pos.avg_entry_price;
                    let new_cost = net_qty * fill_px;
                    pos.qty += net_qty;
                    pos.qty_available += net_qty;
                    if pos.qty > Decimal::ZERO {
                        pos.avg_entry_price = (old_cost + new_cost) / pos.qty;
                    }
                    pos.current_price = Some(fill_px);
                    pos.market_value = Some(pos.qty * fill_px);
                }
                None => {
                    st.position = Some(Position {
                        asset_id: Some("sol-asset".to_string()),
                        symbol: req.symbol.replace('/', ""),
                        exchange: Some("CRYPTO".to_string()),
                        asset_class: Some("crypto".to_string()),
                        qty: net_qty,
                        qty_available: net_qty,
                        avg_entry_price: fill_px,
                        side: "long".to_string(),
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
                created_at: Some("2026-11-24T10:00:00Z".to_string()),
                updated_at: Some("2026-11-24T10:00:01Z".to_string()),
                submitted_at: Some("2026-11-24T10:00:00Z".to_string()),
                filled_at: Some("2026-11-24T10:00:01Z".to_string()),
                expired_at: None,
                canceled_at: None,
                failed_at: None,
                symbol: req.symbol.clone(),
                qty,
                filled_qty: qty,
                filled_avg_price: Some(fill_px),
                order_class: Some("simple".to_string()),
                side: "buy".to_string(),
                order_type: req.order_type.clone(),
                time_in_force: req.time_in_force.clone(),
                limit_price: limit_px,
                status: "filled".to_string(),
            };
            st.orders.insert(oid, ord.clone());
            return Ok(ord);
        }

        // Sell order
        if st.fail_submit_sell {
            return Err(AlpacaError::ApiStatus {
                status: StatusCode::INTERNAL_SERVER_ERROR,
                message: "Simulated exchange 500 during exit sell".to_string(),
            });
        }

        if let Some(avail_cap) = st.submit_sell_403_once.take() {
            if qty > avail_cap {
                return Err(AlpacaError::ApiStatus {
                    status: StatusCode::FORBIDDEN,
                    message: format!(
                        r#"{{"code":40310000,"available":"{}","requested":"{}","message":"insufficient balance"}}"#,
                        avail_cap, qty
                    ),
                });
            }
        }

        let avail = st
            .position
            .as_ref()
            .map(|p| p.qty_available)
            .unwrap_or(Decimal::ZERO);
        if qty > avail {
            return Err(AlpacaError::ApiStatus {
                status: StatusCode::FORBIDDEN,
                message: format!(
                    r#"{{"code":40310000,"available":"{}","requested":"{}","message":"insufficient balance"}}"#,
                    avail, qty
                ),
            });
        }

        let seq = st.next_order_seq;
        st.next_order_seq += 1;

        if req.order_type == "market" {
            let oid = st
                .market_sell_id_override
                .take()
                .unwrap_or_else(|| format!("mkt-sell-{:04}", seq));

            if let Some(override_status) = st.market_sell_status_override.take() {
                let ord = Order {
                    id: oid.clone(),
                    client_order_id: req.client_order_id.clone(),
                    created_at: Some("2026-11-24T12:00:00Z".to_string()),
                    updated_at: Some("2026-11-24T12:00:01Z".to_string()),
                    submitted_at: Some("2026-11-24T12:00:00Z".to_string()),
                    filled_at: None,
                    expired_at: None,
                    canceled_at: None,
                    failed_at: if override_status == "rejected" {
                        Some("2026-11-24T12:00:01Z".to_string())
                    } else {
                        None
                    },
                    symbol: req.symbol.clone(),
                    qty,
                    filled_qty: Decimal::ZERO,
                    filled_avg_price: None,
                    order_class: Some("simple".to_string()),
                    side: "sell".to_string(),
                    order_type: "market".to_string(),
                    time_in_force: req.time_in_force.clone(),
                    limit_price: None,
                    status: override_status,
                };
                st.orders.insert(oid, ord.clone());
                return Ok(ord);
            }

            let fill_px = st.market_sell_fill_price;
            let gross = qty * fill_px;
            let cfee = if is_crypto {
                ceil_usd_cents(gross * st.market_sell_cfee_rate)
            } else {
                Decimal::ZERO
            };

            if let Some(ref mut pos) = st.position {
                pos.qty = (pos.qty - qty).max(Decimal::ZERO);
                pos.qty_available = (pos.qty_available - qty).max(Decimal::ZERO);
                if pos.qty <= Decimal::from_parts(1, 0, 0, false, 7) {
                    st.position = None;
                }
            }
            st.account.cash += gross - cfee;

            if cfee > Decimal::ZERO {
                st.activities.insert(
                    0,
                    AccountActivity {
                        id: format!("cfee-{}", oid),
                        activity_type: "CFEE".to_string(),
                        order_id: Some(oid.clone()),
                        symbol: Some(req.symbol.clone()),
                        transaction_time: Some("2026-11-24T12:00:05Z".to_string()),
                        date: Some("2026-11-24".to_string()),
                        net_amount: Some(-cfee),
                        qty: None,
                        price: None,
                        description: Some("Crypto taker fee".to_string()),
                        status: Some("executed".to_string()),
                    },
                );
            }

            let filled_ord = Order {
                id: oid.clone(),
                client_order_id: req.client_order_id.clone(),
                created_at: Some("2026-11-24T12:00:00Z".to_string()),
                updated_at: Some("2026-11-24T12:00:05Z".to_string()),
                submitted_at: Some("2026-11-24T12:00:00Z".to_string()),
                filled_at: Some("2026-11-24T12:00:05Z".to_string()),
                expired_at: None,
                canceled_at: None,
                failed_at: None,
                symbol: req.symbol.clone(),
                qty,
                filled_qty: qty,
                filled_avg_price: Some(fill_px),
                order_class: Some("simple".to_string()),
                side: "sell".to_string(),
                order_type: "market".to_string(),
                time_in_force: req.time_in_force.clone(),
                limit_price: None,
                status: "filled".to_string(),
            };
            st.orders.insert(oid.clone(), filled_ord.clone());

            if st.inject_stale_after_market_sell {
                let mut stale_ghost = filled_ord.clone();
                stale_ghost.status = "partially_filled".to_string();
                stale_ghost.filled_qty = floor_qty(qty * d("0.90"), 9);
                stale_ghost.filled_at = None;
                st.extra_stale_open_orders.push(stale_ghost);
            }

            return Ok(filled_ord);
        }

        // Limit SELL
        let oid = format!("sell-ord-{:04}", seq);
        if let Some(ref mut pos) = st.position {
            pos.qty_available = (pos.qty_available - qty).max(Decimal::ZERO);
        }

        let ord = Order {
            id: oid.clone(),
            client_order_id: req.client_order_id.clone(),
            created_at: Some("2026-11-24T10:00:02Z".to_string()),
            updated_at: Some("2026-11-24T10:00:02Z".to_string()),
            submitted_at: Some("2026-11-24T10:00:02Z".to_string()),
            filled_at: None,
            expired_at: None,
            canceled_at: None,
            failed_at: None,
            symbol: req.symbol.clone(),
            qty,
            filled_qty: Decimal::ZERO,
            filled_avg_price: None,
            order_class: Some("simple".to_string()),
            side: "sell".to_string(),
            order_type: "limit".to_string(),
            time_in_force: req.time_in_force.clone(),
            limit_price: limit_px,
            status: "new".to_string(),
        };
        st.orders.insert(oid, ord.clone());
        Ok(ord)
    }

    async fn cancel_order(&self, order_id: &str) -> Result<bool, AlpacaError> {
        let mut st = self.state.lock().unwrap();
        let leave_pending = st.cancel_leaves_pending;
        let (was_open, side, rem_qty) = match st.orders.get_mut(order_id) {
            Some(ord) if ord.is_open() => {
                let rem = ord.remaining_qty();
                let side = ord.side.clone();
                if leave_pending {
                    ord.status = "pending_cancel".to_string();
                    (true, side, Decimal::ZERO)
                } else {
                    ord.status = "canceled".to_string();
                    ord.canceled_at = Some("2026-11-24T12:00:00Z".to_string());
                    (true, side, rem)
                }
            }
            Some(_) => return Ok(false),
            None => return Ok(false),
        };
        if was_open && side == "sell" && rem_qty > Decimal::ZERO {
            if let Some(ref mut pos) = st.position {
                pos.qty_available = (pos.qty_available + rem_qty).min(pos.qty);
            }
        }
        Ok(was_open)
    }

    async fn get_latest_quote(&self, _symbol: &str) -> Result<QuoteDetail, AlpacaError> {
        let mut st = self.state.lock().unwrap();
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
        Ok(self.state.lock().unwrap().portfolio_history.clone())
    }

    async fn get_account_activities(
        &self,
        _activity_types: &str,
        _after: Option<&str>,
    ) -> Result<Vec<AccountActivity>, AlpacaError> {
        Ok(self.state.lock().unwrap().activities.clone())
    }

    async fn get_account_activities_page(
        &self,
        _activity_types: &str,
        _after: Option<&str>,
        page_token: Option<&str>,
    ) -> Result<Vec<AccountActivity>, AlpacaError> {
        let st = self.state.lock().unwrap();
        if !st.paginate_activities {
            return if page_token.is_some() {
                Ok(Vec::new())
            } else {
                Ok(st.activities.clone())
            };
        }

        let start_idx = match page_token {
            None => 0,
            Some(tok) => match st.activities.iter().position(|a| a.id == tok) {
                Some(idx) => idx + 1,
                None => return Ok(Vec::new()),
            },
        };
        let end_idx = (start_idx + ACTIVITY_PAGE_SIZE).min(st.activities.len());
        Ok(st.activities[start_idx..end_idx].to_vec())
    }
}

/// Test 1: Exact replay of `deb40eea` — 0.22% taker buy on a tight margin right above min_margin.
/// Proves that `execute_hunt` observes the exact post-fee net balance (99.78% of buy_qty),
/// calibrates `FeeTracker` to Tier 2, raises the exit limit price strictly above round-trip break-even
/// (above fair_price), and realizes STRICTLY POSITIVE net PnL even under worst-case 0.25% taker sell CFEE.
#[tokio::test]
async fn test_replay_deb40eea_taker_buy_guaranteed_positive_net_pnl() {
    let tmp = tempdir().unwrap();
    let store = StateStore::new(tmp.path());
    let params = default_sol_params();

    // Fresh quote: bid 71.99 / ask 72.01 -> passive buy_price = 72.00
    // Fair price = 72.27 (+0.3736% above 72.00, > min_margin 0.36%, but BELOW 0.22% + 0.25% = 0.47% fees!)
    let fair_price_f64 = 72.27;
    let mock = MockAlpacaClient::new(MockBrokerState {
        account: Account {
            equity: d("100000.00"),
            cash: d("100000.00"),
            buying_power: d("100000.00"),
            non_marginable_buying_power: Some(d("100000.00")),
        },
        default_quote: QuoteDetail {
            timestamp: Some("2026-11-24T10:00:00Z".to_string()),
            bid_price: d("71.99"),
            ask_price: d("72.01"),
            bid_size: Some(d("500")),
            ask_size: Some(d("500")),
        },
        buy_fee_rate: d("0.0022"), // Tier 2 taker buy fee (exact deb40eea scenario)
        ..Default::default()
    });

    let mut trades_history = Vec::new();
    let mut fee_tracker = FeeTracker::new();
    let now = DateTime::parse_from_rfc3339("2026-11-24T10:00:00Z")
        .unwrap()
        .with_timezone(&Utc);

    let created = execute_hunt(
        &mock,
        &params,
        fair_price_f64,
        &mut trades_history,
        &mut fee_tracker,
        Some(&store),
        now,
        HuntTiming::fast_for_tests(),
        None,
    )
    .await
    .unwrap()
    .expect("hunt must execute and place exit");

    // 1. FeeTracker calibrated to Tier 2 from realized 0.22% buy-fill delta, while preserving >= 0.25% exit floor
    assert_eq!(fee_tracker.current_tier().tier, 2);
    assert_eq!(fee_tracker.maker_fee_rate(), d("0.0012"));
    assert_eq!(fee_tracker.taker_fee_rate(), d("0.0022"));
    assert_eq!(fee_tracker.worst_sell_fee_rate(), d("0.0025"));

    // 2. Exit limit price MUST be raised strictly above fair_price (72.27) to clear round-trip fees
    let exit_px = created.sell_price.expect("exit sell price must be set");
    assert!(
        exit_px > d("72.27"),
        "exit_px {} must be strictly raised above fair_price 72.27 to prevent deb40eea loss",
        exit_px
    );
    assert_eq!(created.status, HuntStatus::OpenExitPending);
    let sell_oid = created.sell_order_id.clone().expect("sell order submitted");
    assert!(
        created.qty < created.buy_qty,
        "sell qty {} must reflect net post-0.22%-fee balance < buy_qty {}",
        created.qty,
        created.buy_qty
    );

    // 3. Simulate exit filling under WORST-CASE 0.25% Tier 1 taker sell CFEE!
    mock.fill_sell_order(&sell_oid, Some(exit_px), CRYPTO_TIER1_TAKER_FEE_RATE, false);

    let (changed, telemetry) = reconcile_and_maintain_positions(
        &mock,
        &params,
        &mut trades_history,
        &mut fee_tracker,
        d("72.27"),
        d("72.35"),
        now + ChronoDuration::minutes(5),
        Some(&store),
        ReconciliationTiming::fast_for_tests(),
    )
    .await;

    assert!(changed);
    assert_eq!(trades_history.len(), 1);
    let closed = &trades_history[0];
    assert_eq!(closed.status, HuntStatus::Completed);
    let net_pnl = closed.pnl.expect("pnl must be recorded");
    assert!(
        net_pnl > Decimal::ZERO,
        "deb40eea invariant violated: net_pnl={} must be strictly > 0 even at 0.25% taker sell CFEE (buy={}, sell={}, buy_qty={}, sell_qty={})",
        net_pnl,
        closed.buy_price,
        exit_px,
        closed.buy_qty,
        closed.qty
    );
    assert!(telemetry.reconciliation.is_synced);
    assert_eq!(telemetry.reconciliation.broker_qty, 0.0);
}

/// Test 2: Exact replay of `adp-75d8` — stale 12h liquidation fills market sell `75d82683`,
/// then Alpaca's stale `get_open_orders()` index STILL returns `75d82683` as a `partially_filled`
/// market order while another unrelated position remains open.
/// Proves Step C rejects adoption across all 6 gates -> 0 phantom duplicates, 0 share drift.
#[tokio::test]
async fn test_replay_adp_75d8_stale_liquidation_rejects_phantom_adoption() {
    let tmp = tempdir().unwrap();
    let store = StateStore::new(tmp.path());
    let params = default_sol_params();
    let now = DateTime::parse_from_rfc3339("2026-11-24T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let thirteen_hours_ago = (now - ChronoDuration::hours(13)).to_rfc3339();
    let one_hour_ago = (now - ChronoDuration::hours(1)).to_rfc3339();

    let mut orders = HashMap::new();
    // Stale order (>12h old) to be canceled and liquidated at market
    orders.insert(
        "stale-limit-01".to_string(),
        Order {
            id: "stale-limit-01".to_string(),
            client_order_id: Some("hunt-sell-stale".to_string()),
            created_at: Some(thirteen_hours_ago.clone()),
            updated_at: Some(thirteen_hours_ago.clone()),
            submitted_at: Some(thirteen_hours_ago.clone()),
            filled_at: None,
            expired_at: None,
            canceled_at: None,
            failed_at: None,
            symbol: "SOL/USD".to_string(),
            qty: d("50.0"),
            filled_qty: Decimal::ZERO,
            filled_avg_price: None,
            order_class: Some("simple".to_string()),
            side: "sell".to_string(),
            order_type: "limit".to_string(),
            time_in_force: "gtc".to_string(),
            limit_price: Some(d("76.00")),
            status: "new".to_string(),
        },
    );
    // Healthy second position (20 SOL) kept open so broker position.qty > CRYPTO_DUST_QTY stays true
    orders.insert(
        "legit-limit-02".to_string(),
        Order {
            id: "legit-limit-02".to_string(),
            client_order_id: Some("hunt-sell-legit".to_string()),
            created_at: Some(one_hour_ago.clone()),
            updated_at: Some(one_hour_ago.clone()),
            submitted_at: Some(one_hour_ago.clone()),
            filled_at: None,
            expired_at: None,
            canceled_at: None,
            failed_at: None,
            symbol: "SOL/USD".to_string(),
            qty: d("20.0"),
            filled_qty: Decimal::ZERO,
            filled_avg_price: None,
            order_class: Some("simple".to_string()),
            side: "sell".to_string(),
            order_type: "limit".to_string(),
            time_in_force: "gtc".to_string(),
            limit_price: Some(d("75.50")),
            status: "new".to_string(),
        },
    );

    let mock = MockAlpacaClient::new(MockBrokerState {
        position: Some(Position {
            asset_id: Some("sol".to_string()),
            symbol: "SOLUSD".to_string(),
            exchange: Some("CRYPTO".to_string()),
            asset_class: Some("crypto".to_string()),
            qty: d("70.0"),
            qty_available: Decimal::ZERO, // All 70 locked across the 2 open limit sells
            avg_entry_price: d("74.00"),
            side: "long".to_string(),
            market_value: Some(d("5145.00")),
            cost_basis: Some(d("5180.00")),
            unrealized_pl: Some(d("-35.00")),
            current_price: Some(d("73.50")),
        }),
        orders,
        market_sell_id_override: Some("75d82683-dead-beef-0000-000000000001".to_string()),
        market_sell_fill_price: d("73.50"),
        market_sell_cfee_rate: d("0.0022"),
        inject_stale_after_market_sell: true, // Replays exact Alpaca stale index bug!
        ..Default::default()
    });

    let mut trades_history = vec![
        TradeRecord {
            id: "stale001".to_string(),
            symbol: "SOL/USD".to_string(),
            buy_order_id: Some("buy-stale-01".to_string()),
            sell_order_id: Some("stale-limit-01".to_string()),
            buy_qty: d("50.075"),
            qty: d("50.0"),
            filled_exit_qty: Decimal::ZERO,
            buy_price: d("74.00"),
            sell_price: Some(d("76.00")),
            pnl: None,
            pnl_pct: None,
            gross_pnl: None,
            fees_usd: None,
            buy_timestamp: thirteen_hours_ago,
            sell_timestamp: None,
            status: HuntStatus::OpenExitPending,
            adopted_from_broker: false,
            recovered_orphan: false,
            buy_fee_already_deducted: None,
        },
        TradeRecord {
            id: "legit002".to_string(),
            symbol: "SOL/USD".to_string(),
            buy_order_id: Some("buy-legit-02".to_string()),
            sell_order_id: Some("legit-limit-02".to_string()),
            buy_qty: d("20.03"),
            qty: d("20.0"),
            filled_exit_qty: Decimal::ZERO,
            buy_price: d("74.00"),
            sell_price: Some(d("75.50")),
            pnl: None,
            pnl_pct: None,
            gross_pnl: None,
            fees_usd: None,
            buy_timestamp: one_hour_ago,
            sell_timestamp: None,
            status: HuntStatus::OpenExitPending,
            adopted_from_broker: false,
            recovered_orphan: false,
            buy_fee_already_deducted: None,
        },
    ];

    let mut fee_tracker = FeeTracker::new();
    let (changed, telemetry) = reconcile_and_maintain_positions(
        &mock,
        &params,
        &mut trades_history,
        &mut fee_tracker,
        d("74.50"),
        d("73.50"),
        now,
        Some(&store),
        ReconciliationTiming::fast_for_tests(),
    )
    .await;

    assert!(changed);
    // ZERO phantom `adp-75d8` duplicate created!
    assert_eq!(trades_history.len(), 2);
    assert!(!trades_history.iter().any(|h| h.id.starts_with("adp-")));
    assert_eq!(trades_history[0].status, HuntStatus::LiquidatedStale);
    assert_eq!(
        trades_history[0].sell_order_id.as_deref(),
        Some("75d82683-dead-beef-0000-000000000001")
    );
    assert!(trades_history[0].fees_usd.unwrap() > Decimal::ZERO);
    assert_eq!(trades_history[1].status, HuntStatus::OpenExitPending);

    // Reconciliation shows 0 drift and 100% sync on remaining 20 SOL!
    assert_eq!(telemetry.reconciliation.broker_qty, 20.0);
    assert_eq!(telemetry.reconciliation.tracked_active_qty, 20.0);
    assert_eq!(telemetry.reconciliation.untracked_qty, 0.0);
    assert!(telemetry.reconciliation.is_synced);
    assert_eq!(telemetry.broker_open_orders.len(), 1);
    assert_eq!(telemetry.broker_open_orders[0].id, "legit-limit-02");
}

/// Test 3: Stale-index race on a normal `Completed` limit exit AND an `AdoptedClosed` exit.
/// Proves Step C rejects re-adoption BOTH in the completion cycle (`cycle_closed_order_ids`)
/// AND in subsequent cycles (`all_known_sell_ids` across terminal hunts + `get_order` point check).
#[tokio::test]
async fn test_stale_index_race_on_completed_and_adopted_exit_rejects_readoption() {
    let params = default_sol_params();
    let now = DateTime::parse_from_rfc3339("2026-11-24T11:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let ts = (now - ChronoDuration::minutes(30)).to_rfc3339();

    let mut orders = HashMap::new();
    for (oid, qty, px) in [
        ("sell-normal-01", d("40.0"), d("74.60")),
        ("sell-adopted-02", d("10.0"), d("74.70")),
        ("sell-active-03", d("30.0"), d("75.00")),
    ] {
        orders.insert(
            oid.to_string(),
            Order {
                id: oid.to_string(),
                client_order_id: Some(format!("cid-{}", oid)),
                created_at: Some(ts.clone()),
                updated_at: Some(ts.clone()),
                submitted_at: Some(ts.clone()),
                filled_at: None,
                expired_at: None,
                canceled_at: None,
                failed_at: None,
                symbol: "SOL/USD".to_string(),
                qty,
                filled_qty: Decimal::ZERO,
                filled_avg_price: None,
                order_class: Some("simple".to_string()),
                side: "sell".to_string(),
                order_type: "limit".to_string(),
                time_in_force: "gtc".to_string(),
                limit_price: Some(px),
                status: "new".to_string(),
            },
        );
    }

    let mock = MockAlpacaClient::new(MockBrokerState {
        position: Some(Position {
            asset_id: Some("sol".to_string()),
            symbol: "SOLUSD".to_string(),
            exchange: Some("CRYPTO".to_string()),
            asset_class: Some("crypto".to_string()),
            qty: d("80.0"),
            qty_available: Decimal::ZERO,
            avg_entry_price: d("74.00"),
            side: "long".to_string(),
            market_value: Some(d("5968.00")),
            cost_basis: Some(d("5920.00")),
            unrealized_pl: Some(d("48.00")),
            current_price: Some(d("74.60")),
        }),
        orders,
        ..Default::default()
    });

    // Both sell-normal-01 and sell-adopted-02 fill, BUT Alpaca's stale get_open_orders STILL returns them!
    mock.fill_sell_order("sell-normal-01", None, CRYPTO_TIER1_MAKER_FEE_RATE, true);
    mock.fill_sell_order("sell-adopted-02", None, CRYPTO_TIER1_MAKER_FEE_RATE, true);

    let mut trades_history = vec![
        TradeRecord {
            id: "norm0001".to_string(),
            symbol: "SOL/USD".to_string(),
            buy_order_id: Some("buy-norm-01".to_string()),
            sell_order_id: Some("sell-normal-01".to_string()),
            buy_qty: d("40.0601"),
            qty: d("40.0"),
            filled_exit_qty: Decimal::ZERO,
            buy_price: d("74.00"),
            sell_price: Some(d("74.60")),
            pnl: None,
            pnl_pct: None,
            gross_pnl: None,
            fees_usd: None,
            buy_timestamp: ts.clone(),
            sell_timestamp: None,
            status: HuntStatus::OpenExitPending,
            adopted_from_broker: false,
            recovered_orphan: false,
            buy_fee_already_deducted: None,
        },
        TradeRecord {
            id: "adp-sell".to_string(),
            symbol: "SOL/USD".to_string(),
            buy_order_id: None,
            sell_order_id: Some("sell-adopted-02".to_string()),
            buy_qty: d("10.0"),
            qty: d("10.0"),
            filled_exit_qty: Decimal::ZERO,
            buy_price: d("74.00"),
            sell_price: Some(d("74.70")),
            pnl: None,
            pnl_pct: None,
            gross_pnl: None,
            fees_usd: None,
            buy_timestamp: ts.clone(),
            sell_timestamp: None,
            status: HuntStatus::OpenExitPending,
            adopted_from_broker: true,
            recovered_orphan: false,
            buy_fee_already_deducted: Some(true),
        },
        TradeRecord {
            id: "actv0003".to_string(),
            symbol: "SOL/USD".to_string(),
            buy_order_id: Some("buy-actv-03".to_string()),
            sell_order_id: Some("sell-active-03".to_string()),
            buy_qty: d("30.0451"),
            qty: d("30.0"),
            filled_exit_qty: Decimal::ZERO,
            buy_price: d("74.00"),
            sell_price: Some(d("75.00")),
            pnl: None,
            pnl_pct: None,
            gross_pnl: None,
            fees_usd: None,
            buy_timestamp: ts,
            sell_timestamp: None,
            status: HuntStatus::OpenExitPending,
            adopted_from_broker: false,
            recovered_orphan: false,
            buy_fee_already_deducted: None,
        },
    ];

    let mut fee_tracker = FeeTracker::new();

    // Cycle 1: fills finalized, stale index still has both filled orders
    let (changed1, tel1) = reconcile_and_maintain_positions(
        &mock,
        &params,
        &mut trades_history,
        &mut fee_tracker,
        d("74.60"),
        d("74.58"),
        now,
        None,
        ReconciliationTiming::fast_for_tests(),
    )
    .await;

    assert!(changed1);
    assert_eq!(trades_history.len(), 3, "must NOT re-adopt completed orders");
    assert_eq!(trades_history[0].status, HuntStatus::Completed);
    assert_eq!(trades_history[1].status, HuntStatus::AdoptedClosed);
    // Adopted order had buy_fee_already_deducted=Some(true): only sell CFEE ($1.13) should be charged!
    assert_eq!(
        trades_history[1].fees_usd,
        Some(ceil_usd_cents(d("10.0") * d("74.70") * CRYPTO_TIER1_MAKER_FEE_RATE))
    );
    assert_eq!(trades_history[2].status, HuntStatus::OpenExitPending);
    assert!(tel1.reconciliation.is_synced);
    assert_eq!(tel1.reconciliation.untracked_qty, 0.0);

    // Cycle 2 (fresh empty cycle_closed_order_ids): stale index STILL lists sell-normal-01!
    // Must STILL be rejected by `all_known_sell_ids` + `get_order` point check!
    let (changed2, tel2) = reconcile_and_maintain_positions(
        &mock,
        &params,
        &mut trades_history,
        &mut fee_tracker,
        d("74.60"),
        d("74.58"),
        now + ChronoDuration::seconds(30),
        None,
        ReconciliationTiming::fast_for_tests(),
    )
    .await;

    assert!(!changed2);
    assert_eq!(trades_history.len(), 3);
    assert!(tel2.reconciliation.is_synced);
}

/// Test 4: Network failure (`get_position()` = `Err`) during a canceled partial exit.
/// Proves the engine DEFERS cleanly instead of falsely closing the hunt as `Completed`,
/// and automatically re-raises a guaranteed-profitable exit sell once the network recovers.
#[tokio::test]
async fn test_network_failure_during_canceled_partial_exit_defers_then_recovers() {
    let params = default_sol_params();
    let now = DateTime::parse_from_rfc3339("2026-11-24T11:30:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let ts = (now - ChronoDuration::minutes(20)).to_rfc3339();

    let mut orders = HashMap::new();
    orders.insert(
        "sell-cancel-01".to_string(),
        Order {
            id: "sell-cancel-01".to_string(),
            client_order_id: Some("cid-cancel-01".to_string()),
            created_at: Some(ts.clone()),
            updated_at: Some(now.to_rfc3339()),
            submitted_at: Some(ts.clone()),
            filled_at: None,
            expired_at: None,
            canceled_at: Some(now.to_rfc3339()),
            failed_at: None,
            symbol: "SOL/USD".to_string(),
            qty: d("100.0"),
            filled_qty: d("25.0"), // 25 filled, 75 remaining!
            filled_avg_price: Some(d("74.80")),
            order_class: Some("simple".to_string()),
            side: "sell".to_string(),
            order_type: "limit".to_string(),
            time_in_force: "gtc".to_string(),
            limit_price: Some(d("74.80")),
            status: "canceled".to_string(),
        },
    );

    let mock = MockAlpacaClient::new(MockBrokerState {
        position: Some(Position {
            asset_id: Some("sol".to_string()),
            symbol: "SOLUSD".to_string(),
            exchange: Some("CRYPTO".to_string()),
            asset_class: Some("crypto".to_string()),
            qty: d("75.0"),
            qty_available: d("75.0"),
            avg_entry_price: d("74.00"),
            side: "long".to_string(),
            market_value: Some(d("5550.00")),
            cost_basis: Some(d("5550.00")),
            unrealized_pl: Some(Decimal::ZERO),
            current_price: Some(d("74.00")),
        }),
        fail_position: true, // Network failure on get_position!
        orders,
        ..Default::default()
    });

    let mut trades_history = vec![TradeRecord {
        id: "cncl0001".to_string(),
        symbol: "SOL/USD".to_string(),
        buy_order_id: Some("buy-cncl-01".to_string()),
        sell_order_id: Some("sell-cancel-01".to_string()),
        buy_qty: d("100.1502"),
        qty: d("100.0"),
        filled_exit_qty: Decimal::ZERO,
        buy_price: d("74.00"),
        sell_price: Some(d("74.10")), // Intentionally tight legacy target; recovery must raise it!
        pnl: None,
        pnl_pct: None,
        gross_pnl: None,
        fees_usd: None,
        buy_timestamp: ts,
        sell_timestamp: None,
        status: HuntStatus::OpenExitPending,
        adopted_from_broker: false,
        recovered_orphan: false,
        buy_fee_already_deducted: None,
    }];

    let mut fee_tracker = FeeTracker::new();

    // Cycle 1: position endpoint errors -> MUST defer, NEVER close as Completed/LiquidatedManual!
    let (changed1, _) = reconcile_and_maintain_positions(
        &mock,
        &params,
        &mut trades_history,
        &mut fee_tracker,
        d("74.05"),
        d("74.00"),
        now,
        None,
        ReconciliationTiming::fast_for_tests(),
    )
    .await;

    assert!(!changed1);
    assert_eq!(trades_history[0].status, HuntStatus::OpenExitPending);
    assert_eq!(
        trades_history[0].sell_order_id.as_deref(),
        Some("sell-cancel-01")
    );
    assert!(trades_history[0].pnl.is_none());

    // Cycle 2: network recovers -> engine re-raises protective sell for remaining 75.0 SOL above break-even
    mock.state.lock().unwrap().fail_position = false;
    let (changed2, tel2) = reconcile_and_maintain_positions(
        &mock,
        &params,
        &mut trades_history,
        &mut fee_tracker,
        d("74.05"),
        d("74.00"),
        now + ChronoDuration::seconds(30),
        None,
        ReconciliationTiming::fast_for_tests(),
    )
    .await;

    assert!(changed2);
    assert_eq!(trades_history[0].status, HuntStatus::OpenExitPending);
    assert_ne!(
        trades_history[0].sell_order_id.as_deref(),
        Some("sell-cancel-01")
    );
    assert_eq!(trades_history[0].qty, d("75"));
    let reraised_px = trades_history[0].sell_price.unwrap();
    assert!(
        reraised_px > d("74.30"),
        "re-raised exit price {} must clear worst-case fees above buy 74.00",
        reraised_px
    );
    assert!(tel2.reconciliation.is_synced);
}

/// Test 5: Orphan auto-protection when `fair_price < avg_entry`.
/// Proves Step D reconstructs conservative gross buy_qty = orphan_avail / (1 - worst_buy_fee_rate),
/// prices strictly above round-trip break-even (ignoring depressed fair_price), AND persists
/// `ExitSubmitPending` to disk BEFORE calling `submit_order`.
#[tokio::test]
async fn test_orphan_auto_protection_when_fair_price_below_avg_entry() {
    let tmp = tempdir().unwrap();
    let store = StateStore::new(tmp.path());
    let params = default_sol_params();
    let now = DateTime::parse_from_rfc3339("2026-11-24T12:00:00Z")
        .unwrap()
        .with_timezone(&Utc);

    // Part A: Simulate exchange 500 on exit submit -> verify ExitSubmitPending was ALREADY persisted on disk!
    let mock_fail = MockAlpacaClient::new(MockBrokerState {
        position: Some(Position {
            asset_id: Some("sol".to_string()),
            symbol: "SOLUSD".to_string(),
            exchange: Some("CRYPTO".to_string()),
            asset_class: Some("crypto".to_string()),
            qty: d("100.0"),
            qty_available: d("100.0"),
            avg_entry_price: d("74.00"),
            side: "long".to_string(),
            market_value: Some(d("7245.00")),
            cost_basis: Some(d("7400.00")),
            unrealized_pl: Some(d("-155.00")),
            current_price: Some(d("72.45")),
        }),
        fail_submit_sell: true,
        ..Default::default()
    });

    let mut trades_history = Vec::new();
    let mut fee_tracker = FeeTracker::new();

    let (changed_fail, _) = reconcile_and_maintain_positions(
        &mock_fail,
        &params,
        &mut trades_history,
        &mut fee_tracker,
        d("72.50"), // fair_price < avg_entry (74.00)!
        d("72.45"),
        now,
        Some(&store),
        ReconciliationTiming::fast_for_tests(),
    )
    .await;

    assert!(changed_fail);
    assert_eq!(trades_history.len(), 1);
    assert_eq!(trades_history[0].status, HuntStatus::ExitSubmitPending);
    assert!(trades_history[0].recovered_orphan);

    // Crash-safety verification: persisted to disk BEFORE submit failed!
    let reloaded = store.load_hunts(50);
    assert_eq!(reloaded.len(), 1);
    assert_eq!(reloaded[0].status, HuntStatus::ExitSubmitPending);
    assert!(reloaded[0].buy_qty > d("100.25"));

    // Part B: Next cycle exchange recovers -> Step A submits protective exit strictly above round-trip break-even
    mock_fail.state.lock().unwrap().fail_submit_sell = false;
    let (changed_ok, tel) = reconcile_and_maintain_positions(
        &mock_fail,
        &params,
        &mut trades_history,
        &mut fee_tracker,
        d("72.50"),
        d("72.45"),
        now + ChronoDuration::seconds(30),
        Some(&store),
        ReconciliationTiming::fast_for_tests(),
    )
    .await;

    assert!(changed_ok);
    assert_eq!(trades_history.len(), 1);
    let rec = &trades_history[0];
    assert_eq!(rec.status, HuntStatus::OpenExitPending);
    assert!(rec.sell_order_id.is_some());
    let exit_px = rec.sell_price.unwrap();
    assert!(
        exit_px > d("74.37"),
        "orphan exit_px {} must clear round-trip break-even above avg_entry=74.00 even when fair=72.50",
        exit_px
    );

    // Verify strict profitability even under worst-case 0.25% buy fee + 0.25% taker sell CFEE
    let (net_pnl, _, _, _) = compute_net_trade_pnl(
        d("74.00"),
        exit_px,
        rec.buy_qty,
        rec.qty,
        true,
        CRYPTO_TIER1_TAKER_FEE_RATE,
        CRYPTO_TIER1_TAKER_FEE_RATE,
        None,
        false,
    );
    assert!(
        net_pnl > Decimal::ZERO,
        "orphan exit_px={} produced non-positive worst-case net_pnl={}",
        exit_px,
        net_pnl
    );
    assert!(tel.reconciliation.is_synced);
}

/// Test 6: Full `TradingEngine` lifecycle — startup CFEE calibration, legacy JSON compatibility,
/// anomaly hunt, atomic persistence of `hunts_history.json`, `metrics.json`, `telemetry.json`,
/// and `kalman_state.json`, plus reload parity with `dashboard/app.py` and `dashboard/static/charts.js`.
#[tokio::test]
async fn test_full_cycle_persistence_and_dashboard_schema_compatibility() {
    let tmp = tempdir().unwrap();
    let params = default_sol_params();

    // Seed existing production-schema hunts_history.json (including adopted_closed & legacy completed)
    let legacy_hunts_json = r#"[
        {
            "id": "hist0001",
            "symbol": "SOL/USD",
            "buy_order_id": "b-hist-1",
            "sell_order_id": "s-hist-1",
            "buy_qty": 100.0,
            "qty": 99.85,
            "filled_exit_qty": 99.85,
            "buy_price": 72.00,
            "sell_price": 72.60,
            "pnl": 38.23,
            "pnl_pct": 0.0053,
            "gross_pnl": 60.00,
            "fees_usd": 21.68,
            "buy_timestamp": "2026-11-23T08:00:00Z",
            "sell_timestamp": "2026-11-23T09:00:00Z",
            "status": "completed"
        },
        {
            "id": "adp-9999",
            "symbol": "SOL/USD",
            "buy_order_id": null,
            "sell_order_id": "s-adp-9",
            "buy_qty": 25.0,
            "qty": 25.0,
            "filled_exit_qty": 25.0,
            "buy_price": 73.00,
            "sell_price": 73.40,
            "pnl": 7.80,
            "pnl_pct": 0.0043,
            "gross_pnl": 10.00,
            "fees_usd": 2.20,
            "buy_timestamp": "2026-11-23T10:00:00Z",
            "sell_timestamp": "2026-11-23T11:00:00Z",
            "status": "adopted_closed",
            "adopted_from_broker": true,
            "buy_fee_already_deducted": true
        }
    ]"#;
    std::fs::write(tmp.path().join("hunts_history.json"), legacy_hunts_json).unwrap();

    // Seed startup CFEE activity representing Tier 2 maker (0.12% on $10,000 = $12.00)
    let mut orders = HashMap::new();
    orders.insert(
        "s-cfee-tier2".to_string(),
        Order {
            id: "s-cfee-tier2".to_string(),
            client_order_id: None,
            created_at: Some("2026-11-23T12:00:00Z".to_string()),
            updated_at: Some("2026-11-23T12:00:05Z".to_string()),
            submitted_at: Some("2026-11-23T12:00:00Z".to_string()),
            filled_at: Some("2026-11-23T12:00:05Z".to_string()),
            expired_at: None,
            canceled_at: None,
            failed_at: None,
            symbol: "SOL/USD".to_string(),
            qty: d("100.0"),
            filled_qty: d("100.0"),
            filled_avg_price: Some(d("100.00")),
            order_class: Some("simple".to_string()),
            side: "sell".to_string(),
            order_type: "limit".to_string(),
            time_in_force: "gtc".to_string(),
            limit_price: Some(d("100.00")),
            status: "filled".to_string(),
        },
    );

    let mut quotes = VecDeque::new();
    // 3 stable quotes around $75.01 to anchor Kalman fair price, then a 0.65% dip (cycle quote + fresh hunt quote)
    for _ in 0..3 {
        quotes.push_back(QuoteDetail {
            timestamp: Some("2026-11-24T09:00:00Z".to_string()),
            bid_price: d("75.00"),
            ask_price: d("75.02"),
            bid_size: Some(d("100")),
            ask_size: Some(d("100")),
        });
    }
    for _ in 0..2 {
        quotes.push_back(QuoteDetail {
            timestamp: Some("2026-11-24T09:02:00Z".to_string()),
            bid_price: d("74.50"),
            ask_price: d("74.52"),
            bid_size: Some(d("100")),
            ask_size: Some(d("100")),
        });
    }

    let mock = MockAlpacaClient::new(MockBrokerState {
        account: Account {
            equity: d("105000.00"),
            cash: d("105000.00"),
            buying_power: d("105000.00"),
            non_marginable_buying_power: Some(d("105000.00")),
        },
        orders,
        quote_queue: quotes,
        default_quote: QuoteDetail {
            timestamp: Some("2026-11-24T09:03:00Z".to_string()),
            bid_price: d("74.50"),
            ask_price: d("74.52"),
            bid_size: Some(d("100")),
            ask_size: Some(d("100")),
        },
        portfolio_history: PortfolioHistory {
            timestamp: vec![1770681600, 1770768000, 1770854400],
            equity: vec![Some(100_000.0), Some(103_000.0), Some(104_500.0)],
            ..Default::default()
        },
        activities: vec![AccountActivity {
            id: "act-cfee-1".to_string(),
            activity_type: "CFEE".to_string(),
            order_id: Some("s-cfee-tier2".to_string()),
            symbol: Some("SOL/USD".to_string()),
            transaction_time: Some("2026-11-23T12:00:05Z".to_string()),
            date: Some("2026-11-23".to_string()),
            net_amount: Some(d("-12.00")), // 0.12% Tier 2 maker on $10,000
            qty: None,
            price: None,
            description: Some("Crypto maker fee".to_string()),
            status: Some("executed".to_string()),
        }],
        buy_fee_rate: d("0.0012"),
        ..Default::default()
    });

    let mut engine = TradingEngine::with_client_and_timing(
        params.clone(),
        tmp.path(),
        mock.clone(),
        EngineTiming::fast_for_tests(),
    );
    engine.initialize().await;

    // Verify startup CFEE calibration picked up Tier 2 while preserving 0.25% worst-case exit floor
    assert_eq!(engine.fee_tracker().current_tier().tier, 2);
    assert_eq!(engine.fee_tracker().worst_sell_fee_rate(), d("0.0025"));

    let base_time = DateTime::parse_from_rfc3339("2026-11-24T09:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let mut last_snap = None;
    for i in 0..4 {
        let snap = engine
            .step_cycle_at(base_time + ChronoDuration::seconds(30 * i), None)
            .await
            .unwrap();
        last_snap = Some(snap);
    }

    let snap = last_snap.unwrap();
    assert_eq!(snap.active_hunts_count, 1, "4th cycle dip must open 1 active hunt");
    // Only `hist0001` counts as closed strategy trade (`adp-9999` is `adopted_closed`, excluded from stats)
    assert_eq!(snap.completed_trades_count, 1);
    assert!(snap.broker.reconciliation.is_synced);

    // Validate all 4 JSON files exist and match dashboard/app.py + charts.js schema
    let hunts_raw: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(tmp.path().join("hunts_history.json")).unwrap(),
    )
    .unwrap();
    let metrics_raw: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(tmp.path().join("metrics.json")).unwrap(),
    )
    .unwrap();
    let telemetry_raw: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(tmp.path().join("telemetry.json")).unwrap(),
    )
    .unwrap();
    let kalman_raw: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(tmp.path().join("kalman_state.json")).unwrap(),
    )
    .unwrap();

    assert_eq!(hunts_raw.as_array().unwrap().len(), 3);
    assert_eq!(hunts_raw[1]["status"], "adopted_closed");
    assert_eq!(hunts_raw[2]["status"], "open_exit_pending");

    for key in [
        "source",
        "start_date",
        "start_equity",
        "current_equity",
        "total_return_pct",
        "total_trades",
        "completed_trades",
        "win_rate",
        "total_pnl",
        "total_fees_usd",
        "sharpe_ratio",
        "max_drawdown_pct",
        "max_drawdown_dollars",
        "annualized_volatility",
        "avg_pnl_per_trade",
        "profit_factor",
        "chart_labels",
        "equity_series",
        "cumulative_pnl_series",
        "drawdown_series",
        "timestamp",
    ] {
        assert!(metrics_raw.get(key).is_some(), "metrics.json missing key '{}'", key);
    }
    assert_eq!(metrics_raw["source"], "alpaca_portfolio_history");
    assert_eq!(metrics_raw["completed_trades"], 1);

    for key in [
        "timestamp",
        "symbol",
        "best_bid",
        "best_ask",
        "market_mid",
        "fair_price",
        "undervaluation_margin",
        "min_margin",
        "max_margin",
        "circuit_breaker_active",
        "active_hunts_count",
        "completed_trades_count",
        "total_realized_pnl",
        "broker_account",
        "broker_position",
        "broker_open_orders",
        "reconciliation",
        "trades",
        "metrics",
    ] {
        assert!(
            telemetry_raw.get(key).is_some(),
            "telemetry.json missing key '{}'",
            key
        );
    }
    assert_eq!(telemetry_raw["reconciliation"]["is_synced"], true);
    assert!(kalman_raw["sample_count"].as_u64().unwrap() >= 4);

    // Reload into a brand new engine instance to prove restart persistence continuity
    let reloaded_engine = TradingEngine::with_client_and_timing(
        params,
        tmp.path(),
        mock,
        EngineTiming::fast_for_tests(),
    );
    assert_eq!(reloaded_engine.trades_history().len(), 3);
    assert!(reloaded_engine.filter().is_initialized());
    assert_eq!(reloaded_engine.filter().state().sample_count, 4);
}

/// Test 7 (Extra Invariant): Fresh-quote runaway guard in `execute_hunt`.
/// Proves that if the quote jumps right before `submit_order` so executable margin < min_margin,
/// `execute_hunt` aborts cleanly with ZERO orders submitted.
#[tokio::test]
async fn test_execute_hunt_aborts_when_fresh_quote_runs_away() {
    let params = default_sol_params();
    let mock = MockAlpacaClient::new(MockBrokerState {
        // Fresh quote ran up to 74.90 / 74.92 while fair_price is 75.00 -> margin 0.12% < 0.36%
        default_quote: QuoteDetail {
            timestamp: Some("2026-11-24T10:00:00Z".to_string()),
            bid_price: d("74.90"),
            ask_price: d("74.92"),
            bid_size: Some(d("100")),
            ask_size: Some(d("100")),
        },
        ..Default::default()
    });

    let mut trades_history = Vec::new();
    let mut fee_tracker = FeeTracker::new();
    let res = execute_hunt(
        &mock,
        &params,
        75.00,
        &mut trades_history,
        &mut fee_tracker,
        None,
        Utc::now(),
        HuntTiming::fast_for_tests(),
        None,
    )
    .await
    .unwrap();

    assert!(res.is_none());
    assert!(trades_history.is_empty());
    assert!(mock.state.lock().unwrap().submitted_requests.is_empty());
}

/// Test 8 (CE-1.2 Regression):
/// Part A: Transient `get_position` error falls back to `worst_buy_fee_rate` (0.25%), AND
///         when HTTP 403 further shrinks qty within 2% (99.50 SOL), `submit_exit_sell_with_retry`
///         recomputes `exit_price` UPWARD, updates `hunt.sell_price`, and realizes strictly positive
///         net P&L >= $0.01 even under 0.25% taker sell CFEE.
/// Part B: Drastic >2% shrink (`available = 90.00` vs `buy_qty = 100.00`) aborts retry and leaves
///         the hunt cleanly in `HuntStatus::ExitSubmitPending` without placing a bad exit order.
#[tokio::test]
async fn test_ce12_retry_recomputes_exit_price_and_guards_2pct_shrink() {
    let tmp = tempdir().unwrap();
    let store = StateStore::new(tmp.path());
    let mut params = default_sol_params();
    params.max_shares = 100.0;
    let now = DateTime::parse_from_rfc3339("2026-11-24T10:00:00Z")
        .unwrap()
        .with_timezone(&Utc);

    // Part A: get_position fails during settle -> worst_buy_fee fallback (99.75), then 403 shrinks to 99.50!
    {
        let mut st = MockBrokerState {
            default_quote: QuoteDetail {
                timestamp: Some("2026-11-24T10:00:00Z".into()),
                bid_price: d("71.99"),
                ask_price: d("72.01"),
                bid_size: Some(d("1000")),
                ask_size: Some(d("1000")),
            },
            buy_fee_rate: CRYPTO_TIER1_TAKER_FEE_RATE,
            submit_sell_403_once: Some(d("99.50")),
            ..Default::default()
        };
        st.get_position_script.push_back(Ok(None)); // pre-buy
        st.get_position_script
            .push_back(Err("transient timeout 1".into()));
        st.get_position_script
            .push_back(Err("transient timeout 2".into()));

        let mock = MockAlpacaClient::new(st);
        let mut trades_history = Vec::new();
        let mut fee_tracker = FeeTracker::new();

        let created = execute_hunt(
            &mock,
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
        .expect("hunt must succeed with repriced retry");

        assert_eq!(created.status, HuntStatus::OpenExitPending);
        assert_eq!(created.qty, d("99.50"));
        let sell_oid = created.sell_order_id.clone().unwrap();
        let repriced_px = created.sell_price.unwrap();
        // Verify broker order limit_price matches the repriced higher exit_px
        let broker_ord = mock.state.lock().unwrap().orders[&sell_oid].clone();
        assert_eq!(broker_ord.limit_price, Some(repriced_px));
        assert!(
            repriced_px > d("72.55"),
            "shrinking qty from 99.75 -> 99.50 must dynamically raise exit_px (got {})",
            repriced_px
        );

        // Fill at repriced limit under worst-case 0.25% taker sell CFEE -> strictly profitable Completed!
        mock.fill_sell_order(&sell_oid, Some(repriced_px), CRYPTO_TIER1_TAKER_FEE_RATE, false);
        reconcile_and_maintain_positions(
            &mock,
            &params,
            &mut trades_history,
            &mut fee_tracker,
            d("72.27"),
            repriced_px,
            now + ChronoDuration::minutes(5),
            Some(&store),
            ReconciliationTiming::fast_for_tests(),
        )
        .await;

        assert_eq!(trades_history[0].status, HuntStatus::Completed);
        assert!(
            trades_history[0].pnl.unwrap() >= d("0.01"),
            "repriced exit must realize strictly positive net PnL >= $0.01, got {:?}",
            trades_history[0].pnl
        );
    }

    // Part B: >2% drastic shrink on 403 (`available = 90.00` vs `buy_qty ~ 100`) aborts retry safely!
    {
        let mut st = MockBrokerState {
            default_quote: QuoteDetail {
                timestamp: Some("2026-11-24T10:00:00Z".into()),
                bid_price: d("71.99"),
                ask_price: d("72.01"),
                bid_size: Some(d("1000")),
                ask_size: Some(d("1000")),
            },
            buy_fee_rate: CRYPTO_TIER1_TAKER_FEE_RATE,
            submit_sell_403_once: Some(d("90.00")), // 10% below buy_qty -> triggers 2% guard!
            ..Default::default()
        };
        st.get_position_script.push_back(Ok(None));
        st.get_position_script.push_back(Err("timeout 1".into()));
        st.get_position_script.push_back(Err("timeout 2".into()));

        let mock = MockAlpacaClient::new(st);
        let mut trades_history = Vec::new();
        let mut fee_tracker = FeeTracker::new();

        let created = execute_hunt(
            &mock,
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
        .expect("hunt record must persist as ExitSubmitPending");

        assert_eq!(created.status, HuntStatus::ExitSubmitPending);
        assert!(created.sell_order_id.is_none());
        // Only the buy and the single rejected sell attempt should have been sent (no 90 SOL sell!)
        let sell_reqs: Vec<_> = mock
            .state
            .lock()
            .unwrap()
            .submitted_requests
            .iter()
            .filter(|r| r.side == "sell")
            .cloned()
            .collect();
        assert_eq!(sell_reqs.len(), 1);
    }
}

/// Test 9 (CE-1.3a & CE-1.3b Regression):
/// - Part A: Canceled partial exit leaving un-sellable `< $1.00` dust with negative net P&L
///   is classified as `LiquidatedManual` (NEVER `Completed`).
/// - Part B: Stale `0.001 BTC` (`$90`) exit canceled with 0 fills is NOT treated as dust and NOT
///   passed into `finalize_filled_exit`; it is liquidated at MARKET and marked `LiquidatedStale`.
#[tokio::test]
async fn test_ce13_dust_non_loss_gate_and_btc_notional_dust() {
    let params = default_sol_params();
    let now = DateTime::parse_from_rfc3339("2026-11-24T23:00:00Z")
        .unwrap()
        .with_timezone(&Utc);

    // Part A: CE-1.3a dust remainder with negative PnL -> LiquidatedManual, NOT Completed
    {
        let mut orders = HashMap::new();
        orders.insert(
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
        let mock = MockAlpacaClient::new(MockBrokerState {
            orders,
            activities: vec![AccountActivity {
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
            }],
            ..Default::default()
        });
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
            buy_fee_already_deducted: Some(true),
        }];
        let mut ft = FeeTracker::new();
        reconcile_and_maintain_positions(
            &mock,
            &params,
            &mut trades,
            &mut ft,
            d("72.52"),
            d("72.50"),
            now,
            None,
            ReconciliationTiming::fast_for_tests(),
        )
        .await;

        assert_eq!(trades[0].status, HuntStatus::LiquidatedManual);
        assert!(trades[0].pnl.unwrap() <= Decimal::ZERO);
    }

    // Part B: CE-1.3b 0.001 BTC ($85-$90) stale order canceled with 0 fills -> LiquidatedStale at market!
    {
        let mut btc_params = default_sol_params();
        btc_params.symbol = "BTC/USD".into();
        let mut orders = HashMap::new();
        orders.insert(
            "stale-btc-001".into(),
            Order {
                id: "stale-btc-001".into(),
                client_order_id: None,
                created_at: Some("2026-11-24T09:00:00Z".into()),
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
        let mock = MockAlpacaClient::new(MockBrokerState {
            position: Some(Position {
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
            }),
            orders,
            market_sell_fill_price: d("85000.00"),
            ..Default::default()
        });
        let mut trades = vec![TradeRecord {
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
        let mut ft = FeeTracker::new();
        reconcile_and_maintain_positions(
            &mock,
            &btc_params,
            &mut trades,
            &mut ft,
            d("85000.00"),
            d("85000.00"),
            now,
            None,
            ReconciliationTiming::fast_for_tests(),
        )
        .await;

        assert_eq!(trades[0].status, HuntStatus::LiquidatedStale);
        assert_ne!(trades[0].sell_order_id.as_deref(), Some("stale-btc-001"));
        assert!(trades[0].pnl.unwrap() < Decimal::ZERO);
    }
}

/// Test 10 (CE-1.5 Regression): Step D orphan rounds `stored_buy_px` BEFORE computing `exit_px`
/// and prices against `avg_entry.max(stored_buy_px)`, guaranteeing strictly positive `net_pnl >= $0.01`.
#[tokio::test]
async fn test_ce15_step_d_orphan_rounds_buy_price_before_exit_pricing() {
    let mut params = default_sol_params();
    params.symbol = "DOGE/USD".into();
    let now = DateTime::parse_from_rfc3339("2026-11-24T10:00:00Z")
        .unwrap()
        .with_timezone(&Utc);

    let mock = MockAlpacaClient::new(MockBrokerState {
        position: Some(Position {
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
        }),
        ..Default::default()
    });

    let mut trades = Vec::new();
    let mut ft = FeeTracker::new();
    reconcile_and_maintain_positions(
        &mock,
        &params,
        &mut trades,
        &mut ft,
        d("0.0500"),
        d("0.0500"),
        now,
        None,
        ReconciliationTiming::fast_for_tests(),
    )
    .await;

    assert_eq!(trades.len(), 1);
    assert_eq!(trades[0].status, HuntStatus::OpenExitPending);
    assert_eq!(trades[0].buy_price, d("0.1001"));
    let exit_px = trades[0].sell_price.unwrap();
    assert!(
        exit_px >= d("0.1007"),
        "exit_px {} must clear rounded-up buy_price=0.1001 + round-trip fees",
        exit_px
    );
    let sell_oid = trades[0].sell_order_id.clone().unwrap();

    mock.fill_sell_order(&sell_oid, Some(exit_px), CRYPTO_TIER1_TAKER_FEE_RATE, false);
    reconcile_and_maintain_positions(
        &mock,
        &params,
        &mut trades,
        &mut ft,
        d("0.1010"),
        exit_px,
        now + ChronoDuration::minutes(10),
        None,
        ReconciliationTiming::fast_for_tests(),
    )
    .await;

    assert_eq!(trades[0].status, HuntStatus::Completed);
    assert!(
        trades[0].pnl.unwrap() >= d("0.01"),
        "orphan Completed trade must realize net_pnl >= $0.01, got {:?}",
        trades[0].pnl
    );
}

/// Test 11 (CE-5.1, CE-5.2, CE-5.3 Regression):
/// - CE-5.1: Stale cancel stuck in `pending_cancel` defers safely (remains `OpenExitPending`).
/// - CE-5.2: Mid-cycle `get_position` `Err` on canceled exit defers safely (NEVER falls back to stale `avail_pool = 0`).
/// - CE-5.3: Rejected OR still-pending stale market sell NEVER fabricates `LiquidatedStale`.
#[tokio::test]
async fn test_ce51_ce52_ce53_reconciliation_fault_tolerance() {
    let params = default_sol_params();
    let now = DateTime::parse_from_rfc3339("2026-11-24T23:00:00Z")
        .unwrap()
        .with_timezone(&Utc);

    let mk_pos = || Position {
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
    let mk_order = |id: &str, created: &str, status: &str| Order {
        id: id.into(),
        client_order_id: None,
        created_at: Some(created.into()),
        updated_at: None,
        submitted_at: None,
        filled_at: None,
        expired_at: None,
        canceled_at: (status == "canceled").then(|| "2026-11-24T22:59:59Z".into()),
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
        status: status.into(),
    };
    let mk_hunt = |id: &str, oid: &str, ts: &str| TradeRecord {
        id: id.into(),
        symbol: "SOL/USD".into(),
        buy_order_id: Some("buy-1".into()),
        sell_order_id: Some(oid.into()),
        buy_qty: d("50.075"),
        qty: d("50.0"),
        filled_exit_qty: Decimal::ZERO,
        buy_price: d("74.00"),
        sell_price: Some(d("76.00")),
        pnl: None,
        pnl_pct: None,
        gross_pnl: None,
        fees_usd: None,
        buy_timestamp: ts.into(),
        sell_timestamp: None,
        status: HuntStatus::OpenExitPending,
        adopted_from_broker: false,
        recovered_orphan: false,
        buy_fee_already_deducted: Some(true),
    };

    // CE-5.1: Stale cancel times out in `pending_cancel` -> MUST defer as OpenExitPending!
    {
        let mut orders = HashMap::new();
        orders.insert(
            "stale-pend-01".into(),
            mk_order("stale-pend-01", "2026-11-24T09:00:00Z", "new"),
        );
        let mock = MockAlpacaClient::new(MockBrokerState {
            position: Some(mk_pos()),
            orders,
            cancel_leaves_pending: true,
            ..Default::default()
        });
        let mut trades = vec![mk_hunt("ce51", "stale-pend-01", "2026-11-24T09:00:00Z")];
        let mut ft = FeeTracker::new();
        reconcile_and_maintain_positions(
            &mock,
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
        assert_eq!(trades[0].sell_order_id.as_deref(), Some("stale-pend-01"));
    }

    // CE-5.2: Mid-cycle get_position Err on canceled exit -> MUST defer as OpenExitPending!
    {
        let mut orders = HashMap::new();
        orders.insert(
            "canceled-mid-01".into(),
            mk_order("canceled-mid-01", "2026-11-24T22:00:00Z", "canceled"),
        );
        let locked = mk_pos();
        let mut st = MockBrokerState {
            position: Some(locked.clone()),
            orders,
            ..Default::default()
        };
        st.get_position_script.push_back(Ok(Some(locked.clone())));
        st.get_position_script
            .push_back(Err("mid-cycle position timeout".into()));
        st.get_position_script.push_back(Ok(Some(locked)));

        let mock = MockAlpacaClient::new(st);
        let mut trades = vec![mk_hunt("ce52", "canceled-mid-01", "2026-11-24T22:00:00Z")];
        let mut ft = FeeTracker::new();
        reconcile_and_maintain_positions(
            &mock,
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
        assert!(trades[0].pnl.is_none());
    }

    // CE-5.3a: Rejected stale market sell (filled_qty = 0) -> MUST transition to ExitSubmitPending!
    {
        let mut orders = HashMap::new();
        orders.insert(
            "stale-mkt-rej".into(),
            mk_order("stale-mkt-rej", "2026-11-24T09:00:00Z", "new"),
        );
        let mock = MockAlpacaClient::new(MockBrokerState {
            position: Some(mk_pos()),
            orders,
            market_sell_status_override: Some("rejected".into()),
            ..Default::default()
        });
        let mut trades = vec![mk_hunt("ce53a", "stale-mkt-rej", "2026-11-24T09:00:00Z")];
        let mut ft = FeeTracker::new();
        reconcile_and_maintain_positions(
            &mock,
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
        assert!(trades[0].sell_order_id.is_none());
        assert_eq!(trades[0].qty, d("50.0"));
        assert!(trades[0].pnl.is_none());
    }

    // CE-5.3b: Non-terminal stale market sell (`accepted`, filled_qty = 0) -> defers cleanly!
    {
        let mut orders = HashMap::new();
        orders.insert(
            "stale-mkt-pend".into(),
            mk_order("stale-mkt-pend", "2026-11-24T09:00:00Z", "new"),
        );
        let mock = MockAlpacaClient::new(MockBrokerState {
            position: Some(mk_pos()),
            orders,
            market_sell_id_override: Some("mkt-pend-999".into()),
            market_sell_status_override: Some("accepted".into()),
            ..Default::default()
        });
        let mut trades = vec![mk_hunt("ce53b", "stale-mkt-pend", "2026-11-24T09:00:00Z")];
        let mut ft = FeeTracker::new();
        reconcile_and_maintain_positions(
            &mock,
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
        assert_eq!(trades[0].sell_order_id.as_deref(), Some("mkt-pend-999"));
        assert!(trades[0].pnl.is_none());
    }
}

/// Test 12 (Bonus #11 Regression): Multi-page `page_token` CFEE pagination sums >100 partial fills
/// accurately across up to 5 pages (500 activities) without duplicates or truncation.
#[tokio::test]
async fn test_bonus11_cfee_pagination_over_100_partial_fills() {
    let mut activities = Vec::with_capacity(260);
    // 250 CFEE activities for target order ($0.01 each = $2.50 total) + 10 unrelated activities
    for i in 0..250usize {
        activities.push(AccountActivity {
            id: format!("cfee-fill-{:04}", i),
            activity_type: "CFEE".into(),
            order_id: Some("sell-mega-split".into()),
            symbol: Some("SOL/USD".into()),
            transaction_time: Some("2026-11-24T10:05:00Z".into()),
            date: Some("2026-11-24".into()),
            net_amount: Some(d("-0.01")),
            qty: None,
            price: None,
            description: None,
            status: Some("executed".into()),
        });
        if i % 25 == 0 {
            activities.push(AccountActivity {
                id: format!("cfee-other-{:04}", i),
                activity_type: "CFEE".into(),
                order_id: Some("other-order".into()),
                symbol: Some("SOL/USD".into()),
                transaction_time: Some("2026-11-24T10:04:00Z".into()),
                date: Some("2026-11-24".into()),
                net_amount: Some(d("-0.50")),
                qty: None,
                price: None,
                description: None,
                status: Some("executed".into()),
            });
        }
    }

    let mock = MockAlpacaClient::new(MockBrokerState {
        activities,
        paginate_activities: true,
        ..Default::default()
    });

    let total_cfee = fetch_actual_cfee_usd(&mock, &["sell-mega-split"], true)
        .await
        .expect("must sum across all 3 pages");
    assert_eq!(total_cfee, d("2.50"));
}

/// Test 13: End-to-end fixed-USD dynamic regime sizing (`$3,000 <-> $30,000`), concurrent exposure cap,
/// instant drop to `$3,000` on `LiquidatedStale`, and crash-safe `regime_state.json` persistence.
#[tokio::test]
async fn test_regime_sizing_bear_bull_exposure_cap_and_stale_drop() {
    let tmp = tempdir().unwrap();
    let mut params = default_sol_params();
    params.regime_sizing = RegimeSizingParams {
        min_notional_usd: 3000.0,
        max_notional_usd: 30000.0,
        min_total_exposure_usd: 15000.0,
        max_total_exposure_usd: 45000.0,
        ema_fast_cycles: 4,
        ema_slow_cycles: 20,
        vol_cycles: 4,
        sigmoid_k: 2.0,
        sigmoid_center: 0.25,
        ramp_up_alpha: 0.5,
        stale_cooldown_cycles: 10,
    };

    let mock = MockAlpacaClient::new(MockBrokerState {
        account: Account {
            equity: d("1000000.00"),
            cash: d("1000000.00"),
            buying_power: d("1000000.00"),
            non_marginable_buying_power: Some(d("1000000.00")),
        },
        market_sell_fill_price: d("110.00"),
        ..Default::default()
    });

    let push_quote = |bid: Decimal, count: usize| {
        let mut st = mock.state.lock().unwrap();
        for _ in 0..count {
            st.quote_queue.push_back(QuoteDetail {
                timestamp: Some("2026-11-24T10:00:00Z".into()),
                bid_price: bid,
                ask_price: bid + d("0.02"),
                bid_size: Some(d("10000")),
                ask_size: Some(d("10000")),
            });
        }
    };

    let mut engine = TradingEngine::with_client_and_timing(
        params.clone(),
        tmp.path(),
        mock.clone(),
        EngineTiming::fast_for_tests(),
    );
    engine.initialize().await;

    let base_time = DateTime::parse_from_rfc3339("2026-11-24T10:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let mut step = 0i64;
    let mut next_ts = || {
        step += 1;
        base_time + ChronoDuration::seconds(30 * step)
    };

    // 1. Warmup / Bear regime -> strictly capped at $3,000 despite $1,000,000 cash
    push_quote(d("100.00"), 3);
    for _ in 0..3 {
        engine.step_cycle_at(next_ts(), None).await.unwrap();
    }
    assert_eq!(engine.regime_ctrl().current_hunt_cap_usd(), d("3000"));

    push_quote(d("99.00"), 2);
    let bear_snap = engine.step_cycle_at(next_ts(), None).await.unwrap();
    assert_eq!(bear_snap.active_hunts_count, 1);
    let h1 = engine.trades_history()[0].clone();
    let h1_notional = h1.buy_qty * h1.buy_price;
    assert!(
        h1_notional <= d("3000") && h1_notional >= d("2950"),
        "bear/warmup hunt notional {} must respect $3,000 floor cap",
        h1_notional
    );

    // Close Hunt #1 cleanly so Phase 2 starts with zero active exposure
    mock.fill_sell_order(
        h1.sell_order_id.as_deref().unwrap(),
        None,
        CRYPTO_TIER1_MAKER_FEE_RATE,
        false,
    );
    push_quote(d("100.00"), 1);
    let closed_snap = engine.step_cycle_at(next_ts(), None).await.unwrap();
    assert_eq!(closed_snap.active_hunts_count, 0);

    // 2. Sustained bull uptrend -> ramps hunt cap toward $30,000 and total exposure cap toward $45,000
    for i in 1..=30 {
        let bid = d("100.00") + Decimal::new(i * 50, 2);
        push_quote(bid, 1);
        engine.step_cycle_at(next_ts(), None).await.unwrap();
    }
    let bull_cap = engine.regime_ctrl().current_hunt_cap_usd();
    let bull_exp_cap = engine.regime_ctrl().current_total_exposure_cap_usd();
    assert!(
        bull_cap >= d("29500") && bull_cap <= d("30000"),
        "bull cap {} should ramp close to $30,000",
        bull_cap
    );
    assert!(
        bull_exp_cap >= d("44500") && bull_exp_cap <= d("45000"),
        "bull total exposure cap {} should ramp close to $45,000",
        bull_exp_cap
    );

    // Trigger 1st bull anomaly (~1% dip below Kalman fair price) -> opens ~$30k hunt
    let dip_bid_1 = floor_qty(
        Decimal::from_f64_retain(engine.filter().state().x_hat * 0.99).unwrap(),
        2,
    );
    push_quote(dip_bid_1, 2);
    let bull_snap_1 = engine.step_cycle_at(next_ts(), None).await.unwrap();
    assert_eq!(bull_snap_1.active_hunts_count, 1);
    let h2 = engine.trades_history().last().unwrap().clone();
    let h2_notional = h2.buy_qty * h2.buy_price;
    assert!(
        h2_notional > d("25000") && h2_notional <= d("30000"),
        "bull hunt notional {} must scale up to ~$30,000 cap",
        h2_notional
    );

    // Trigger 2nd bull anomaly -> strictly capped by remaining concurrent exposure (~$15,000)
    let dip_bid_2 = floor_qty(
        Decimal::from_f64_retain(engine.filter().state().x_hat * 0.99).unwrap(),
        2,
    );
    push_quote(dip_bid_2, 2);
    let bull_snap_2 = engine.step_cycle_at(next_ts(), None).await.unwrap();
    assert_eq!(bull_snap_2.active_hunts_count, 2);
    let h3 = engine.trades_history().last().unwrap().clone();
    let h3_notional = h3.buy_qty * h3.buy_price;
    assert!(
        h3_notional <= d("16000") && h3_notional > d("1000"),
        "second concurrent bull hunt {} must be capped by remaining exposure budget",
        h3_notional
    );
    let active_exposure: Decimal = engine
        .trades_history()
        .iter()
        .filter(|h| h.status.is_active())
        .map(|h| (h.qty - h.filled_exit_qty).max(Decimal::ZERO) * h.buy_price)
        .sum();
    assert!(
        active_exposure <= d("45000"),
        "active exposure {} exceeded max_total_exposure_usd $45,000",
        active_exposure
    );

    // 3rd dip when concurrent exposure cap is saturated -> ZERO additional hunt opened
    let dip_bid_3 = floor_qty(
        Decimal::from_f64_retain(engine.filter().state().x_hat * 0.99).unwrap(),
        2,
    );
    push_quote(dip_bid_3, 2);
    let capped_snap = engine.step_cycle_at(next_ts(), None).await.unwrap();
    assert_eq!(
        capped_snap.active_hunts_count, 2,
        "exposure cap saturation must block a 3rd concurrent hunt"
    );

    // 3. Advance +13h -> stale liquidation triggers LiquidatedStale & instant clamp to $3,000
    push_quote(d("110.00"), 1);
    let stale_snap = engine
        .step_cycle_at(base_time + ChronoDuration::hours(13), None)
        .await
        .unwrap();
    assert_eq!(stale_snap.active_hunts_count, 0);
    assert!(
        engine
            .trades_history()
            .iter()
            .any(|h| h.status == HuntStatus::LiquidatedStale),
        "expected stale liquidation"
    );
    assert_eq!(engine.regime_ctrl().current_hunt_cap_usd(), d("3000"));
    assert_eq!(
        engine.regime_ctrl().current_total_exposure_cap_usd(),
        d("15000")
    );
    assert_eq!(engine.regime_ctrl().state().stale_cooldown_remaining, 10);

    // Verify regime_state.json persists across engine restart
    assert!(tmp.path().join("regime_state.json").exists());
    let reloaded = TradingEngine::with_client_and_timing(
        params,
        tmp.path(),
        mock,
        EngineTiming::fast_for_tests(),
    );
    assert_eq!(reloaded.regime_ctrl().current_hunt_cap_usd(), d("3000"));
    assert_eq!(reloaded.regime_ctrl().state().stale_cooldown_remaining, 10);
}

