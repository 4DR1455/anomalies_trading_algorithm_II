#!/usr/bin/env python3
"""
Dashboard Generator
Generates a modern, standalone HTML report (index.html) displaying:
- Real-time quantitative risk metrics (Sharpe Ratio, Max Drawdown, Volatility) computed via numpy
- Dynamic equity curve & drawdown tracking
- Signal & Kalman Fair Price vs Market Price
- Interactive trade log with status badges
"""

import json
import os
import sys
from datetime import datetime, timezone
from pathlib import Path

# Add parent and dashboard folder to path
sys.path.insert(0, str(Path(__file__).parent.parent))
sys.path.insert(0, str(Path(__file__).parent))

from persistence import LocalPersistenceStore
from metrics import compute_performance_metrics


def load_telemetry(data_dir: Path) -> dict:
    store = LocalPersistenceStore(data_dir)
    telemetry_path = data_dir / "telemetry.json"
    telemetry = {}

    if telemetry_path.exists():
        try:
            with open(telemetry_path, "r", encoding="utf-8") as f:
                telemetry = json.load(f)
        except Exception as e:
            print(f"Warning: Could not read telemetry: {e}")

    # Ensure last 50 hunts are hydrated from persistent hunts_history.json
    trades = telemetry.get("trades", [])
    if not trades:
        persisted_hunts = store.load_hunts(max_count=50)
        if persisted_hunts:
            telemetry["trades"] = persisted_hunts
            trades = persisted_hunts

    # Fallback default empty telemetry
    if not telemetry:
        telemetry = {
            "timestamp": datetime.now(timezone.utc).isoformat(),
            "symbol": "SOL/USD",
            "best_bid": 73.10,
            "best_ask": 73.18,
            "market_mid": 73.14,
            "fair_price": 73.20,
            "undervaluation_margin": 0.0008,
            "min_margin": 0.0015,
            "max_margin": 0.0250,
            "circuit_breaker_active": False,
            "active_hunts_count": 0,
            "completed_trades_count": len(trades),
            "total_realized_pnl": sum(t.get("pnl", 0.0) for t in trades if t.get("pnl") is not None),
            "trades": trades,
        }
    return telemetry


def render_html(telemetry: dict, metrics: dict, output_path: Path):
    trades = telemetry.get("trades", [])
    trades_rows = []
    for t in reversed(trades[-50:]):
        status = t.get("status", "unknown")
        badge_class = {
            "completed": "badge-success",
            "open_exit_pending": "badge-warning",
            "liquidated_stale": "badge-danger",
        }.get(status, "badge-info")

        pnl = t.get("pnl")
        pnl_str = f"${pnl:+.2f}" if pnl is not None else "Pending"
        pnl_class = "text-success" if pnl and pnl > 0 else ("text-danger" if pnl and pnl < 0 else "text-muted")

        buy_time = t.get("buy_timestamp", "")[:19].replace("T", " ")
        sell_time = t.get("sell_timestamp", "")[:19].replace("T", " ") if t.get("sell_timestamp") else "-"

        trades_rows.append(f"""
        <tr>
            <td><code>{t.get('id', '')[:8]}</code></td>
            <td><strong>{t.get('symbol', '')}</strong></td>
            <td>{t.get('qty', 0)}</td>
            <td>${t.get('buy_price', 0.0):.2f}</td>
            <td>${t.get('sell_price', 0.0):.2f}</td>
            <td class="{pnl_class}"><strong>{pnl_str}</strong></td>
            <td><span class="badge {badge_class}">{status}</span></td>
            <td><small>{buy_time}</small></td>
            <td><small>{sell_time}</small></td>
        </tr>
        """)

    trades_html = "\n".join(trades_rows) if trades_rows else "<tr><td colspan='9' class='text-center py-4 text-muted'>No trades executed yet. Awaiting anomalies.</td></tr>"

    circuit_breaker_html = ""
    if telemetry.get("circuit_breaker_active", False):
        circuit_breaker_html = """
        <div class="alert alert-danger mb-4" role="alert">
            <h4 class="alert-heading">⚠️ CIRCUIT BREAKER ACTIVATED!</h4>
            <p>Market price deviation has exceeded <code>max_margin</code> (Volatility Ceiling). Trading is immediately halted to prevent catching falling knives.</p>
        </div>
        """

    cum_pnl = metrics.get("cumulative_pnl_series", [0.0])
    drawdown = metrics.get("drawdown_series", [0.0])
    trade_indices = list(range(1, len(cum_pnl) + 1)) if len(cum_pnl) > 0 else [1]

    html_content = f"""<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <meta name="viewport" content="width=device-width, initial-scale=1.0">
    <title>Anomalies Trading Engine v2.0 | Quantitative Dashboard</title>
    <link rel="stylesheet" href="https://cdn.jsdelivr.net/npm/bootstrap@5.3.3/dist/css/bootstrap.min.css">
    <script src="https://cdn.jsdelivr.net/npm/chart.js"></script>
    <style>
        body {{
            background-color: #0b0f19;
            color: #e2e8f0;
            font-family: -apple-system, BlinkMacSystemFont, "Segoe UI", Roboto, "Helvetica Neue", Arial, sans-serif;
        }}
        .card {{
            background-color: #151d2f;
            border: 1px solid #1e293b;
            border-radius: 12px;
            box-shadow: 0 4px 6px -1px rgba(0, 0, 0, 0.3);
        }}
        .metric-title {{
            font-size: 0.85rem;
            color: #94a3b8;
            text-transform: uppercase;
            letter-spacing: 0.05em;
        }}
        .metric-value {{
            font-size: 1.85rem;
            font-weight: 700;
        }}
        .badge-success {{ background-color: #10b981; color: white; }}
        .badge-warning {{ background-color: #f59e0b; color: white; }}
        .badge-danger {{ background-color: #ef4444; color: white; }}
        .badge-info {{ background-color: #3b82f6; color: white; }}
        .table-dark {{
            --bs-table-bg: #151d2f;
            --bs-table-striped-bg: #1a2337;
            --bs-table-border-color: #1e293b;
        }}
        .text-success {{ color: #10b981 !important; }}
        .text-danger {{ color: #ef4444 !important; }}
        .chart-container {{
            position: relative;
            height: 280px;
        }}
    </style>
</head>
<body class="py-4">
<div class="container-fluid px-4">
    <!-- Header -->
    <div class="d-flex justify-content-between align-items-center mb-4 pb-3 border-bottom border-secondary">
        <div>
            <h2 class="h3 mb-0 fw-bold text-white">⚡ Anomalies Trading System v2.0</h2>
            <p class="text-secondary mb-0 small">Adaptive 1D Kalman Filter &bull; SEC Rule 612 Sub-Penny Compliant &bull; Non-Root Docker Architecture</p>
        </div>
        <div class="d-flex align-items-center gap-3">
            <a href="https://github.com/4DR1455/anomalies_trading_algorithm_II" target="_blank" rel="noopener noreferrer" class="btn btn-outline-light btn-sm d-flex align-items-center gap-2 px-3 py-2 shadow-sm" style="border-color: #334155; background: rgba(255,255,255,0.03); text-decoration: none;">
                <svg xmlns="http://www.w3.org/2000/svg" width="16" height="16" fill="currentColor" viewBox="0 0 16 16">
                    <path d="M8 0C3.58 0 0 3.58 0 8c0 3.54 2.29 6.53 5.47 7.59.4.07.55-.17.55-.38 0-.19-.01-.82-.01-1.49-2.01.37-2.53-.49-2.69-.94-.09-.23-.48-.94-.82-1.13-.28-.15-.68-.52-.01-.53.63-.01 1.08.58 1.23.82.72 1.21 1.87.87 2.33.66.07-.52.28-.87.51-1.07-1.78-.2-3.64-.89-3.64-3.95 0-.87.31-1.59.82-2.15-.08-.2-.36-1.02.08-2.12 0 0 .67-.21 2.2.82.64-.18 1.32-.27 2-.27s1.36.09 2 .27c1.53-1.04 2.2-.82 2.2-.82.44 1.1.16 1.92.08 2.12.51.56.82 1.27.82 2.15 0 3.07-1.87 3.75-3.65 3.95.29.25.54.73.54 1.48 0 1.07-.01 1.93-.01 2.2 0 .21.15.46.55.38A8.01 8.01 0 0 0 16 8c0-4.42-3.58-8-8-8"/>
                </svg>
                <span>GitHub</span>
            </a>
            <div class="text-end">
                <span class="badge bg-primary px-3 py-2">Symbol: {telemetry.get('symbol', 'SPY')}</span>
                <div class="small text-secondary mt-1">Snapshot: {telemetry.get('timestamp', '')[:19].replace('T', ' ')} UTC</div>
            </div>
        </div>
    </div>

    {circuit_breaker_html}

    <!-- Key Performance Risk Cards (Calculated with numpy) -->
    <div class="row g-3 mb-4">
        <div class="col-md-2 col-sm-4">
            <div class="card p-3">
                <div class="metric-title">Sharpe Ratio</div>
                <div class="metric-value text-info">{metrics.get('sharpe_ratio', 0.0)}</div>
                <small class="text-secondary">Annualized Risk-Adj</small>
            </div>
        </div>
        <div class="col-md-2 col-sm-4">
            <div class="card p-3">
                <div class="metric-title">Max Drawdown</div>
                <div class="metric-value text-danger">{metrics.get('max_drawdown_pct', 0.0)}%</div>
                <small class="text-secondary">${metrics.get('max_drawdown_dollars', 0.0):.2f} peak-to-trough</small>
            </div>
        </div>
        <div class="col-md-2 col-sm-4">
            <div class="card p-3">
                <div class="metric-title">Volatility</div>
                <div class="metric-value text-warning">{metrics.get('annualized_volatility', 0.0)}%</div>
                <small class="text-secondary">Annualized Return Std</small>
            </div>
        </div>
        <div class="col-md-2 col-sm-4">
            <div class="card p-3">
                <div class="metric-title">Total P&L</div>
                <div class="metric-value {'text-success' if metrics.get('total_pnl', 0.0) >= 0 else 'text-danger'}">
                    ${metrics.get('total_pnl', 0.0):+.2f}
                </div>
                <small class="text-secondary">{metrics.get('completed_trades', 0)} completed trades</small>
            </div>
        </div>
        <div class="col-md-2 col-sm-4">
            <div class="card p-3">
                <div class="metric-title">Win Rate</div>
                <div class="metric-value text-success">{metrics.get('win_rate', 0.0)}%</div>
                <small class="text-secondary">Profit Factor: {metrics.get('profit_factor', 0.0)}</small>
            </div>
        </div>
        <div class="col-md-2 col-sm-4">
            <div class="card p-3">
                <div class="metric-title">Kalman Fair Price</div>
                <div class="metric-value text-white">${telemetry.get('fair_price', 0.0):.2f}</div>
                <small class="text-secondary">Mid: ${telemetry.get('market_mid', 0.0):.2f}</small>
            </div>
        </div>
    </div>

    <!-- Charts Row -->
    <div class="row g-3 mb-4">
        <div class="col-lg-8">
            <div class="card p-3">
                <h5 class="card-title h6 text-secondary mb-3">📈 Cumulative Realized P&L ($)</h5>
                <div class="chart-container">
                    <canvas id="pnlChart"></canvas>
                </div>
            </div>
        </div>
        <div class="col-lg-4">
            <div class="card p-3">
                <h5 class="card-title h6 text-secondary mb-3">🛡️ Drawdown Profile (%)</h5>
                <div class="chart-container">
                    <canvas id="drawdownChart"></canvas>
                </div>
            </div>
        </div>
    </div>

    <!-- Microstructure & Signal Details -->
    <div class="row g-3 mb-4">
        <div class="col-md-4">
            <div class="card p-3">
                <h6 class="text-secondary mb-3">Market Microstructure (SEC Rule 612)</h6>
                <ul class="list-group list-group-flush bg-transparent">
                    <li class="list-group-item bg-transparent text-light d-flex justify-content-between px-0">
                        <span>Best Bid / Best Ask:</span>
                        <strong>${telemetry.get('best_bid', 0.0):.2f} / ${telemetry.get('best_ask', 0.0):.2f}</strong>
                    </li>
                    <li class="list-group-item bg-transparent text-light d-flex justify-content-between px-0">
                        <span>Bid-Ask Spread:</span>
                        <span>${(telemetry.get('best_ask', 0.0) - telemetry.get('best_bid', 0.0)):.2f}</span>
                    </li>
                    <li class="list-group-item bg-transparent text-light d-flex justify-content-between px-0">
                        <span>Undervaluation Margin:</span>
                        <strong class="text-info">{telemetry.get('undervaluation_margin', 0.0)*100.0:.4f}%</strong>
                    </li>
                    <li class="list-group-item bg-transparent text-light d-flex justify-content-between px-0">
                        <span>Thresholds [min / max]:</span>
                        <span>{telemetry.get('min_margin', 0.0)*100.0:.2f}% / {telemetry.get('max_margin', 0.0)*100.0:.2f}%</span>
                    </li>
                </ul>
            </div>
        </div>
        <div class="col-md-8">
            <div class="card p-3">
                <h6 class="text-secondary mb-3">Live Hunts & Recent Execution History</h6>
                <div class="table-responsive">
                    <table class="table table-dark table-hover table-striped align-middle mb-0">
                        <thead>
                            <tr class="text-secondary small">
                                <th>ID</th>
                                <th>Symbol</th>
                                <th>Qty</th>
                                <th>Buy</th>
                                <th>Exit (Sell)</th>
                                <th>P&L</th>
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

<script>
    const pnlCtx = document.getElementById('pnlChart').getContext('2d');
    new Chart(pnlCtx, {{
        type: 'line',
        data: {{
            labels: {json.dumps(trade_indices)},
            datasets: [{{
                label: 'Cumulative P&L ($)',
                data: {json.dumps(cum_pnl)},
                borderColor: '#10b981',
                backgroundColor: 'rgba(16, 185, 129, 0.1)',
                borderWidth: 2,
                fill: true,
                tension: 0.2
            }}]
        }},
        options: {{
            responsive: true,
            maintainAspectRatio: false,
            scales: {{
                x: {{ grid: {{ color: '#1e293b' }}, ticks: {{ color: '#94a3b8' }} }},
                y: {{ grid: {{ color: '#1e293b' }}, ticks: {{ color: '#94a3b8' }} }}
            }},
            plugins: {{ legend: {{ display: false }} }}
        }}
    }});

    const ddCtx = document.getElementById('drawdownChart').getContext('2d');
    new Chart(ddCtx, {{
        type: 'line',
        data: {{
            labels: {json.dumps(trade_indices)},
            datasets: [{{
                label: 'Drawdown (%)',
                data: {json.dumps(drawdown)},
                borderColor: '#ef4444',
                backgroundColor: 'rgba(239, 68, 68, 0.15)',
                borderWidth: 2,
                fill: true,
                tension: 0.1
            }}]
        }},
        options: {{
            responsive: true,
            maintainAspectRatio: false,
            scales: {{
                x: {{ grid: {{ color: '#1e293b' }}, ticks: {{ color: '#94a3b8' }} }},
                y: {{ grid: {{ color: '#1e293b' }}, ticks: {{ color: '#94a3b8' }}, reverse: true }}
            }},
            plugins: {{ legend: {{ display: false }} }}
        }}
    }});
</script>
</body>
</html>
"""
    with open(output_path, "w", encoding="utf-8") as f:
        f.write(html_content)
    print(f"Successfully generated dashboard at: {output_path}")


def main():
    workspace_root = Path(__file__).resolve().parent.parent
    data_dir = workspace_root / "data"
    output_path = workspace_root / "dashboard" / "index.html"

    store = LocalPersistenceStore(data_dir)
    telemetry = load_telemetry(data_dir)
    trades = telemetry.get("trades", [])

    if trades:
        metrics = compute_performance_metrics(trades)
        store.save_metrics(metrics)
    else:
        metrics = store.load_metrics()

    render_html(telemetry, metrics, output_path)


if __name__ == "__main__":
    main()
