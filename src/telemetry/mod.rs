use crate::alpaca::{Account, Order, Position};
use crate::metrics::PerformanceMetrics;
use crate::model::pricing::{decimal_to_f64, floor_qty};
use crate::state::TradeRecord;
use rust_decimal::{Decimal, RoundingStrategy};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BrokerAccountTelemetry {
    pub equity: Option<f64>,
    pub cash: Option<f64>,
    pub buying_power: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BrokerPositionTelemetry {
    pub qty: f64,
    pub qty_available: f64,
    pub locked_in_orders: f64,
    pub avg_entry_price: f64,
    pub market_value: f64,
    pub unrealized_pl: f64,
    pub current_price: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrokerOpenOrderSummary {
    pub id: String,
    pub side: String,
    pub qty: f64,
    pub filled_qty: f64,
    pub remaining_qty: f64,
    pub limit_price: Option<f64>,
    pub status: String,
    pub created_at: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ReconciliationTelemetry {
    pub broker_qty: f64,
    pub broker_qty_available: f64,
    pub open_orders_locked_qty: f64,
    pub tracked_active_qty: f64,
    pub untracked_qty: f64,
    pub is_synced: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BrokerTelemetry {
    pub broker_account: BrokerAccountTelemetry,
    pub broker_position: BrokerPositionTelemetry,
    pub broker_open_orders: Vec<BrokerOpenOrderSummary>,
    pub reconciliation: ReconciliationTelemetry,
}

/// Complete real-time telemetry snapshot consumed by `dashboard/app.py` and `dashboard/static/charts.js`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TelemetrySnapshot {
    pub timestamp: String,
    pub symbol: String,
    pub best_bid: f64,
    pub best_ask: f64,
    pub market_mid: f64,
    pub fair_price: f64,
    pub undervaluation_margin: f64,
    pub min_margin: f64,
    pub max_margin: f64,
    pub circuit_breaker_active: bool,
    pub active_hunts_count: usize,
    pub completed_trades_count: usize,
    pub total_realized_pnl: f64,
    #[serde(flatten)]
    pub broker: BrokerTelemetry,
    pub trades: Vec<TradeRecord>,
    pub metrics: PerformanceMetrics,
}

fn round_dp_f64(val: Decimal, dp: u32) -> f64 {
    decimal_to_f64(val.round_dp_with_strategy(dp, RoundingStrategy::MidpointNearestEven))
}

/// Builds real-time Alpaca account, position, open-order, and share-reconciliation telemetry
/// using exact `Decimal` sums before rounding for JSON presentation.
pub fn build_broker_telemetry(
    account: Option<&Account>,
    position: Option<&Position>,
    open_orders: &[Order],
    trades_history: &[TradeRecord],
) -> BrokerTelemetry {
    let broker_account = match account {
        Some(acc) => BrokerAccountTelemetry {
            equity: Some(round_dp_f64(acc.equity, 2)),
            cash: Some(round_dp_f64(acc.cash, 2)),
            buying_power: Some(round_dp_f64(acc.buying_power, 2)),
        },
        None => BrokerAccountTelemetry::default(),
    };

    let broker_qty_d = position
        .map(|p| floor_qty(p.qty, 9))
        .unwrap_or(Decimal::ZERO);
    let broker_avail_d = position
        .map(|p| floor_qty(p.qty_available, 9))
        .unwrap_or(Decimal::ZERO);
    let locked_qty_d = floor_qty((broker_qty_d - broker_avail_d).max(Decimal::ZERO), 9);

    let broker_position = match position {
        Some(p) => BrokerPositionTelemetry {
            qty: decimal_to_f64(broker_qty_d),
            qty_available: decimal_to_f64(broker_avail_d),
            locked_in_orders: decimal_to_f64(locked_qty_d),
            avg_entry_price: round_dp_f64(p.avg_entry_price, 6),
            market_value: round_dp_f64(p.market_value.unwrap_or(Decimal::ZERO), 2),
            unrealized_pl: round_dp_f64(p.unrealized_pl.unwrap_or(Decimal::ZERO), 2),
            current_price: round_dp_f64(p.current_price.unwrap_or(Decimal::ZERO), 4),
        },
        None => BrokerPositionTelemetry::default(),
    };

    let mut open_orders_summary = Vec::with_capacity(open_orders.len());
    let mut open_orders_locked_d = Decimal::ZERO;

    for o in open_orders {
        let o_rem = o.remaining_qty();
        if o.side == "sell" {
            open_orders_locked_d += o_rem;
        }
        open_orders_summary.push(BrokerOpenOrderSummary {
            id: o.id.clone(),
            side: o.side.clone(),
            qty: decimal_to_f64(o.qty),
            filled_qty: decimal_to_f64(o.filled_qty),
            remaining_qty: decimal_to_f64(o_rem),
            limit_price: o.limit_price.map(decimal_to_f64),
            status: o.status.clone(),
            created_at: o.created_at.clone(),
        });
    }

    let mut tracked_active_d = Decimal::ZERO;
    for h in trades_history {
        if h.status.is_active() {
            tracked_active_d += (h.qty - h.filled_exit_qty).max(Decimal::ZERO);
        }
    }

    let tracked_active_floored = floor_qty(tracked_active_d, 9);
    let open_locked_floored = floor_qty(open_orders_locked_d, 9);
    let untracked_d = (broker_qty_d - tracked_active_floored)
        .round_dp_with_strategy(9, RoundingStrategy::MidpointNearestEven);
    let sync_tol = Decimal::from_parts(1, 0, 0, false, 2); // 0.01
    let is_synced = untracked_d.abs() <= sync_tol && broker_avail_d <= sync_tol;

    BrokerTelemetry {
        broker_account,
        broker_position,
        broker_open_orders: open_orders_summary,
        reconciliation: ReconciliationTelemetry {
            broker_qty: decimal_to_f64(broker_qty_d),
            broker_qty_available: decimal_to_f64(broker_avail_d),
            open_orders_locked_qty: decimal_to_f64(open_locked_floored),
            tracked_active_qty: decimal_to_f64(tracked_active_floored),
            untracked_qty: decimal_to_f64(untracked_d),
            is_synced,
        },
    }
}
