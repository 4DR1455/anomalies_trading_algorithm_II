#!/usr/bin/env python3
"""
Read-Only Hardened Quantitative Dashboard Server (Waitress + Flask + NumPy)
Binds strictly to 127.0.0.1:8080 behind Nginx reverse proxy.
Displays live Alpaca broker account, exact position reconciliation, and execution history
solely from read-only telemetry JSON snapshots written by live_bot.py.
"""

import html
import json
import os
from datetime import datetime, timezone
from pathlib import Path

from flask import Flask, Response, abort, jsonify, request
from waitress import serve

from metrics import compute_performance_metrics

TELEMETRY_PATH = Path(os.environ.get("TELEMETRY_PATH", "/opt/anomalies-dashboard/data/telemetry.json"))
ACTIVE_HUNT_STATUSES = {"open_exit_pending", "exit_submit_pending", "exit_partially_filled"}

app = Flask(__name__, static_folder="/opt/anomalies-dashboard/static", static_url_path="/static")


@app.before_request
def enforce_readonly_methods():
    if request.method not in ("GET", "HEAD"):
        abort(405)


def format_qty_display(qty: float) -> str:
    """Formats crypto/stock quantities cleanly with up to 6 decimals without scientific notation."""
    if abs(qty) < 1e-9:
        return "0"
    s = f"{qty:.6f}".rstrip("0").rstrip(".")
    return s or "0"


def load_telemetry() -> dict:
    data = {}
    if TELEMETRY_PATH.exists():
        try:
            with open(TELEMETRY_PATH, "r", encoding="utf-8") as f:
                data = json.load(f)
        except Exception as exc:
            print(f"[WARN] Could not read telemetry: {exc}")

    if not isinstance(data, dict):
        data = {}

    # Ensure hunts persist even if telemetry was freshly created
    trades = data.get("trades")
    if not trades or not isinstance(trades, list):
        hunts_path = TELEMETRY_PATH.parent / "hunts_history.json"
        if hunts_path.exists():
            try:
                with open(hunts_path, "r", encoding="utf-8") as f:
                    cached = json.load(f)
                    if isinstance(cached, list) and cached:
                        data["trades"] = cached
            except Exception as exc:
                print(f"[WARN] Could not read hunts_history: {exc}")

    if not data:
        data = {
            "timestamp": datetime.now(timezone.utc).isoformat(),
            "symbol": "SOL/USD",
            "best_bid": 0.0,
            "best_ask": 0.0,
            "market_mid": 0.0,
            "fair_price": 0.0,
            "undervaluation_margin": 0.0,
            "min_margin": 0.00293,
            "max_margin": 0.0250,
            "circuit_breaker_active": False,
            "active_hunts_count": 0,
            "completed_trades_count": 0,
            "total_realized_pnl": 0.0,
            "trades": [],
        }
    return data


def load_persistent_metrics(trades: list) -> dict:
    metrics_path = TELEMETRY_PATH.parent / "metrics.json"
    if metrics_path.exists():
        try:
            with open(metrics_path, "r", encoding="utf-8") as f:
                cached_metrics = json.load(f)
                if isinstance(cached_metrics, dict) and cached_metrics.get("completed_trades") is not None:
                    return cached_metrics
        except Exception as exc:
            print(f"[WARN] Could not read metrics.json: {exc}")

    return compute_performance_metrics(trades)


def render_dashboard_html(telemetry: dict, metrics: dict) -> str:
    trades = telemetry.get("trades", [])
    if not isinstance(trades, list):
        trades = []

    # Always show ALL active hunts at the top, followed by up to 50 recent terminal hunts
    active_list = [t for t in trades if t.get("status") in ACTIVE_HUNT_STATUSES]
    terminal_list = [t for t in trades if t.get("status") not in ACTIVE_HUNT_STATUSES][-50:]
    ordered_trades = list(reversed(active_list)) + list(reversed(terminal_list))

    trades_rows = []
    for t in ordered_trades:
        status_raw = str(t.get("status", "unknown"))
        badge_class = {
            "completed": "badge-success",
            "open_exit_pending": "badge-warning",
            "exit_partially_filled": "badge-info",
            "exit_submit_pending": "badge-danger",
            "liquidated_stale": "badge-danger",
            "liquidated_manual": "badge-secondary",
            "liquidated_orphan": "badge-danger",
            "adopted_closed": "badge-secondary",
        }.get(status_raw, "badge-info")

        pnl = t.get("pnl")
        pnl_str = f"${float(pnl):+.2f}" if pnl is not None else "Pending"
        pnl_class = "text-success" if pnl is not None and float(pnl) > 0 else ("text-danger" if pnl is not None and float(pnl) < 0 else "text-muted")

        buy_time = html.escape(str(t.get("buy_timestamp") or "")[:19].replace("T", " "))
        sell_raw = t.get("sell_timestamp")
        sell_time = html.escape(str(sell_raw)[:19].replace("T", " ")) if sell_raw else "-"
        tid = html.escape(str(t.get("id") or "")[:8])
        sym = html.escape(str(t.get("symbol") or ""))
        qty = float(t.get("qty") or 0.0)
        filled_exit = float(t.get("filled_exit_qty") or 0.0)
        qty_display = format_qty_display(qty)
        if status_raw == "exit_partially_filled" and filled_exit > 0:
            qty_display = f"{qty_display} ({format_qty_display(filled_exit)} sold)"

        buy_price = float(t.get("buy_price") or 0.0)
        sell_price = float(t.get("sell_price") or 0.0)
        status_esc = html.escape(status_raw)

        trades_rows.append(f"""
        <tr>
            <td><code>{tid}</code></td>
            <td><strong>{sym}</strong></td>
            <td>{html.escape(qty_display)}</td>
            <td>${buy_price:.2f}</td>
            <td>${sell_price:.2f}</td>
            <td class="{pnl_class}"><strong>{html.escape(pnl_str)}</strong></td>
            <td><span class="badge {badge_class}">{status_esc}</span></td>
            <td><small>{buy_time}</small></td>
            <td><small>{sell_time}</small></td>
        </tr>
        """)

    trades_html = "\n".join(trades_rows) if trades_rows else (
        "<tr><td colspan='9' class='text-center py-4 text-muted'>No trades executed yet. Awaiting anomalies.</td></tr>"
    )

    circuit_breaker_html = ""
    if telemetry.get("circuit_breaker_active", False):
        circuit_breaker_html = """
        <div class="alert alert-danger mb-4" role="alert">
            <h4 class="alert-heading">CIRCUIT BREAKER ACTIVATED</h4>
            <p class="mb-0">Market price deviation has exceeded <code>max_margin</code> (Volatility Ceiling). Trading is immediately halted.</p>
        </div>
        """

    # Live Broker Account, Position & Share Reconciliation
    broker_acc = telemetry.get("broker_account") or {}
    broker_pos = telemetry.get("broker_position") or {}
    broker_open = telemetry.get("broker_open_orders") or []
    recon = telemetry.get("reconciliation") or {}

    has_broker_data = bool(recon) or (broker_acc.get("equity") is not None)
    broker_equity = broker_acc.get("equity")
    broker_cash = broker_acc.get("cash")
    broker_qty = float(broker_pos.get("qty") or 0.0)
    broker_avail = float(broker_pos.get("qty_available") or 0.0)
    broker_locked = float(broker_pos.get("locked_in_orders") or 0.0)
    broker_mv = float(broker_pos.get("market_value") or 0.0)
    broker_upl = float(broker_pos.get("unrealized_pl") or 0.0)
    tracked_qty = float(recon.get("tracked_active_qty") or 0.0)
    untracked_qty = float(recon.get("untracked_qty") or 0.0)
    is_synced = bool(recon.get("is_synced", True))

    reconciliation_alert_html = ""
    if has_broker_data and not is_synced:
        reconciliation_alert_html = f"""
        <div class="alert alert-danger mb-4" role="alert">
            <h5 class="alert-heading mb-1">&#9888; BROKER POSITION RECONCILIATION ALERT</h5>
            <p class="mb-0">
                Alpaca live balance: <strong>{format_qty_display(broker_qty)}</strong> &bull;
                Tracked active hunts: <strong>{format_qty_display(tracked_qty)}</strong> &bull;
                Drift: <strong>{untracked_qty:+.6f}</strong> &bull;
                Unprotected (no exit order): <strong>{format_qty_display(broker_avail)}</strong>
            </p>
        </div>
        """

    equity_str = f"${float(broker_equity):,.2f}" if broker_equity is not None else "N/A"
    cash_str = f"${float(broker_cash):,.2f}" if broker_cash is not None else "N/A"
    upl_class = "text-success" if broker_upl >= 0 else "text-danger"
    avail_class = "text-success" if broker_avail <= 0.01 else "text-danger"
    sync_badge_html = (
        '<span class="badge badge-success px-2 py-1">100% SYNCED</span>'
        if (not has_broker_data or is_synced)
        else f'<span class="badge badge-danger px-2 py-1">DRIFT {untracked_qty:+.4f}</span>'
    )

    cum_pnl = metrics.get("cumulative_pnl_series", [0.0])
    drawdown = metrics.get("drawdown_series", [0.0])
    chart_labels = metrics.get("chart_labels")
    if isinstance(chart_labels, list) and len(chart_labels) == len(cum_pnl) and len(chart_labels) > 0:
        x_labels = chart_labels
    else:
        x_labels = list(range(1, len(cum_pnl) + 1)) if len(cum_pnl) > 0 else [1]

    labels_attr = html.escape(json.dumps(x_labels), quote=True)
    pnl_attr = html.escape(json.dumps(cum_pnl), quote=True)
    dd_attr = html.escape(json.dumps(drawdown), quote=True)

    symbol_esc = html.escape(str(telemetry.get("symbol", "SOL/USD")))
    snap_esc = html.escape(str(telemetry.get("timestamp", ""))[:19].replace("T", " "))
    best_bid = float(telemetry.get("best_bid") or 0.0)
    best_ask = float(telemetry.get("best_ask") or 0.0)
    market_mid = float(telemetry.get("market_mid") or 0.0)
    fair_price = float(telemetry.get("fair_price") or 0.0)
    underval_margin = float(telemetry.get("undervaluation_margin") or 0.0)
    min_margin = float(telemetry.get("min_margin") or 0.0)
    max_margin = float(telemetry.get("max_margin") or 0.0)
    active_hunts = int(telemetry.get("active_hunts_count") or len(active_list))

    is_alpaca_history = metrics.get("source") == "alpaca_portfolio_history"
    start_date_esc = html.escape(str(metrics.get("start_date") or "2026-02-10"))
    total_pnl = float(metrics.get("total_pnl", 0.0))
    total_return_pct = metrics.get("total_return_pct")
    total_fees = float(metrics.get("total_fees_usd", 0.0))
    completed_cnt = int(metrics.get("completed_trades", 0))
    pnl_color = "text-success" if total_pnl >= 0 else "text-danger"

    if is_alpaca_history:
        ret_str = f" ({float(total_return_pct):+.2f}%)" if total_return_pct is not None else ""
        pnl_sub = f"Since {start_date_esc}{ret_str}"
        pnl_chart_title = f"Cumulative Portfolio P&amp;L ($) &mdash; Alpaca Official History (since {start_date_esc})"
    else:
        fees_note = f" | Fees: ${total_fees:.2f}" if total_fees > 0 else ""
        pnl_sub = f"{completed_cnt} closed{fees_note}"
        pnl_chart_title = "Cumulative Realized P&amp;L ($)"

    if completed_cnt > 0:
        win_rate_val = f"{metrics.get('win_rate', 0.0)}%"
        win_rate_sub = f"Profit Factor: {metrics.get('profit_factor', 0.0)}"
    else:
        win_rate_val = "N/A"
        win_rate_sub = "Awaiting closed trades"

    return f"""<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <meta http-equiv="refresh" content="30">
    <title>Anomalies Trading Engine v2.0 | Quantitative Dashboard</title>
    <link rel="stylesheet" href="https://cdn.jsdelivr.net/npm/bootstrap@5.3.3/dist/css/bootstrap.min.css">
    <link rel="stylesheet" href="/static/dashboard.css">
    <script src="https://cdn.jsdelivr.net/npm/chart.js"></script>
</head>
<body class="py-4">
<div class="container-fluid px-4">
    <div class="d-flex justify-content-between align-items-center mb-4 pb-3 border-bottom border-secondary">
        <div>
            <h2 class="h3 mb-0 fw-bold text-white">&#9889; Anomalies Trading System v2.0</h2>
            <p class="text-secondary mb-0 small">Adaptive 1D Kalman Filter &bull; Fee-Aware Exit Engine &bull; Live Broker Share Reconciliation</p>
        </div>
        <div class="d-flex align-items-center gap-3">
            <a href="https://github.com/4DR1455/anomalies_trading_algorithm_II" target="_blank" rel="noopener noreferrer" class="btn btn-outline-light btn-sm d-flex align-items-center gap-2 px-3 py-2 shadow-sm" style="border-color: #334155; background: rgba(255,255,255,0.03); text-decoration: none;">
                <svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" fill="currentColor" viewBox="0 0 16 16">
                    <path d="M8 0C3.58 0 0 3.58 0 8c0 3.54 2.29 6.53 5.47 7.59.4.07.55-.17.55-.38 0-.19-.01-.82-.01-1.49-2.01.37-2.53-.49-2.69-.94-.09-.23-.48-.94-.82-1.13-.28-.15-.68-.52-.01-.53.63-.01 1.08.58 1.23.82.72 1.21 1.87.87 2.33.66.07-.52.28-.87.51-1.07-1.78-.2-3.64-.89-3.64-3.95 0-.87.31-1.59.82-2.15-.08-.2-.36-1.02.08-2.12 0 0 .67-.21 2.2.82.64-.18 1.32-.27 2-.27s1.36.09 2 .27c1.53-1.04 2.2-.82 2.2-.82.44 1.1.16 1.92.08 2.12.51.56.82 1.27.82 2.15 0 3.07-1.87 3.75-3.65 3.95.29.25.54.73.54 1.48 0 1.07-.01 1.93-.01 2.2 0 .21.15.46.55.38A8.01 8.01 0 0 0 16 8c0-4.42-3.58-8-8-8"/>
                </svg>
                <span>GitHub</span>
            </a>
            <div class="text-end">
                <span class="badge bg-primary px-3 py-2">Symbol: {symbol_esc}</span>
                {sync_badge_html}
                <div class="small text-secondary mt-1">Snapshot: {snap_esc} UTC</div>
            </div>
        </div>
    </div>

    <div class="alert alert-warning d-flex align-items-center justify-content-center py-2 mb-4 fw-semibold" role="alert">
        &#9888;&nbsp; Still testing new bot
    </div>

    {circuit_breaker_html}
    {reconciliation_alert_html}

    <!-- Live Alpaca Broker Portfolio & Share Reconciliation Row -->
    <div class="row g-3 mb-4">
        <div class="col-md-3 col-sm-6">
            <div class="card p-3">
                <div class="metric-title">Alpaca Equity</div>
                <div class="metric-value text-white">{equity_str}</div>
                <small class="text-secondary">Available Cash: {cash_str}</small>
            </div>
        </div>
        <div class="col-md-3 col-sm-6">
            <div class="card p-3">
                <div class="metric-title">Live Position ({symbol_esc})</div>
                <div class="metric-value text-info">{format_qty_display(broker_qty)}</div>
                <small class="text-secondary">Value: ${broker_mv:,.2f} &bull; <span class="{upl_class}">Unr. P&amp;L: ${broker_upl:+.2f}</span></small>
            </div>
        </div>
        <div class="col-md-3 col-sm-6">
            <div class="card p-3">
                <div class="metric-title">Protected in Exit Orders</div>
                <div class="metric-value text-warning">{format_qty_display(broker_locked)}</div>
                <small class="text-secondary">{len(broker_open)} open broker order(s) &bull; Tracked: {format_qty_display(tracked_qty)}</small>
            </div>
        </div>
        <div class="col-md-3 col-sm-6">
            <div class="card p-3">
                <div class="metric-title">Unprotected (Free Qty)</div>
                <div class="metric-value {avail_class}">{format_qty_display(broker_avail)}</div>
                <small class="text-secondary">Sync Status: {sync_badge_html}</small>
            </div>
        </div>
    </div>

    <!-- Quantitative Risk Metrics Row -->
    <div class="row g-3 mb-4">
        <div class="col-md-2 col-sm-4">
            <div class="card p-3">
                <div class="metric-title">Sharpe Ratio</div>
                <div class="metric-value text-info">{metrics.get("sharpe_ratio", 0.0)}</div>
                <small class="text-secondary">Annualized Risk-Adj</small>
            </div>
        </div>
        <div class="col-md-2 col-sm-4">
            <div class="card p-3">
                <div class="metric-title">Max Drawdown</div>
                <div class="metric-value text-danger">{metrics.get("max_drawdown_pct", 0.0)}%</div>
                <small class="text-secondary">${float(metrics.get("max_drawdown_dollars", 0.0)):,.2f} peak-to-trough</small>
            </div>
        </div>
        <div class="col-md-2 col-sm-4">
            <div class="card p-3">
                <div class="metric-title">Volatility</div>
                <div class="metric-value text-warning">{metrics.get("annualized_volatility", 0.0)}%</div>
                <small class="text-secondary">Annualized Return Std</small>
            </div>
        </div>
        <div class="col-md-2 col-sm-4">
            <div class="card p-3">
                <div class="metric-title">Total Net P&amp;L</div>
                <div class="metric-value {pnl_color}">${total_pnl:+,.2f}</div>
                <small class="text-secondary">{pnl_sub}</small>
            </div>
        </div>
        <div class="col-md-2 col-sm-4">
            <div class="card p-3">
                <div class="metric-title">Win Rate</div>
                <div class="metric-value text-success">{win_rate_val}</div>
                <small class="text-secondary">{win_rate_sub}</small>
            </div>
        </div>
        <div class="col-md-2 col-sm-4">
            <div class="card p-3">
                <div class="metric-title">Kalman Fair Price</div>
                <div class="metric-value text-white">${fair_price:.2f}</div>
                <small class="text-secondary">Mid: ${market_mid:.2f}</small>
            </div>
        </div>
    </div>

    <div class="row g-3 mb-4">
        <div class="col-lg-8">
            <div class="card p-3">
                <h5 class="card-title h6 text-secondary mb-3">{pnl_chart_title}</h5>
                <div class="chart-container">
                    <canvas id="pnlChart"></canvas>
                </div>
            </div>
        </div>
        <div class="col-lg-4">
            <div class="card p-3">
                <h5 class="card-title h6 text-secondary mb-3">Drawdown Profile (%)</h5>
                <div class="chart-container">
                    <canvas id="drawdownChart"></canvas>
                </div>
            </div>
        </div>
    </div>

    <div class="row g-3 mb-4">
        <div class="col-md-4">
            <div class="card p-3">
                <h6 class="text-secondary mb-3">Market Microstructure (SEC Rule 612)</h6>
                <ul class="list-group list-group-flush bg-transparent">
                    <li class="list-group-item bg-transparent text-light d-flex justify-content-between px-0">
                        <span>Best Bid / Best Ask:</span>
                        <strong>${best_bid:.2f} / ${best_ask:.2f}</strong>
                    </li>
                    <li class="list-group-item bg-transparent text-light d-flex justify-content-between px-0">
                        <span>Bid-Ask Spread:</span>
                        <span>${(best_ask - best_bid):.4f}</span>
                    </li>
                    <li class="list-group-item bg-transparent text-light d-flex justify-content-between px-0">
                        <span>Undervaluation Margin:</span>
                        <strong class="text-info">{underval_margin * 100.0:.4f}%</strong>
                    </li>
                    <li class="list-group-item bg-transparent text-light d-flex justify-content-between px-0">
                        <span>Thresholds [min / max]:</span>
                        <span>{min_margin * 100.0:.3f}% / {max_margin * 100.0:.2f}%</span>
                    </li>
                    <li class="list-group-item bg-transparent text-light d-flex justify-content-between px-0">
                        <span>Active Hunts:</span>
                        <span>{active_hunts}</span>
                    </li>
                    <li class="list-group-item bg-transparent text-light d-flex justify-content-between px-0">
                        <span>Broker vs Tracked Drift:</span>
                        <strong class="{'text-success' if is_synced else 'text-danger'}">{untracked_qty:+.6f}</strong>
                    </li>
                </ul>
            </div>
        </div>
        <div class="col-md-8">
            <div class="card p-3">
                <div class="d-flex flex-wrap justify-content-between align-items-center mb-3 gap-2">
                    <h6 class="text-secondary mb-0">Live Hunts &amp; Recent Execution History</h6>
                </div>
                <div class="table-responsive">
                    <table class="table table-dark table-hover table-striped align-middle mb-0">
                        <thead>
                            <tr class="text-secondary small">
                                <th>ID</th>
                                <th>Symbol</th>
                                <th>Qty</th>
                                <th>Buy</th>
                                <th>Exit (Sell)</th>
                                <th>P&amp;L</th>
                                <th>Status</th>
                                <th>Buy Time</th>
                                <th>Exit Time</th>
                            </tr>
                        </thead>
                        <tbody>
                            {trades_html}
                        </tbody>
                    </table>
                </div>
            </div>
        </div>
    </div>
    <!-- Footer -->
    <div class="text-center py-3 border-top border-secondary text-secondary small mt-4">
        <span>Anomalies Trading Algorithm v2.0 &bull; </span>
        <a href="https://github.com/4DR1455/anomalies_trading_algorithm_II" target="_blank" rel="noopener noreferrer" class="text-decoration-none text-info fw-semibold">
            GitHub Repository (4DR1455/anomalies_trading_algorithm_II)
        </a>
    </div>
</div>
<div id="chart-payload" data-labels="{labels_attr}" data-pnl="{pnl_attr}" data-dd="{dd_attr}"></div>
<script src="/static/charts.js"></script>
</body>
</html>
"""


@app.route("/", methods=["GET", "HEAD"])
@app.route("/index.html", methods=["GET", "HEAD"])
def dashboard():
    telemetry = load_telemetry()
    metrics = load_persistent_metrics(telemetry.get("trades", []))
    body = render_dashboard_html(telemetry, metrics)
    return Response(body, status=200, mimetype="text/html")


@app.route("/api/telemetry", methods=["GET", "HEAD"])
def api_telemetry():
    telemetry = load_telemetry()
    metrics = load_persistent_metrics(telemetry.get("trades", []))
    return jsonify({"telemetry": telemetry, "metrics": metrics})


@app.route("/health", methods=["GET", "HEAD"])
def health():
    telemetry = load_telemetry()
    return jsonify({"status": "ok", "timestamp": telemetry.get("timestamp")})


if __name__ == "__main__":
    print("Starting Read-Only Dashboard Backend on 127.0.0.1:8080")
    serve(app, host="127.0.0.1", port=8080, threads=4, ident=None)
