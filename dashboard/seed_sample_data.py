#!/usr/bin/env python3
"""
Populate sample telemetry data to preview the dashboard.
"""

import json
from datetime import datetime, timezone, timedelta
from pathlib import Path
import sys

root = Path(__file__).resolve().parent
data_dir = root / "data"
data_dir.mkdir(exist_ok=True)

now = datetime.now(timezone.utc)
trades = [
    {
        "id": "hunt-01",
        "symbol": "SPY",
        "buy_order_id": "ord-b1",
        "sell_order_id": "ord-s1",
        "qty": 20,
        "buy_price": 585.10,
        "sell_price": 585.38,
        "pnl": 5.60,
        "pnl_pct": 0.00048,
        "buy_timestamp": (now - timedelta(hours=3)).isoformat(),
        "sell_timestamp": (now - timedelta(hours=2, minutes=58)).isoformat(),
        "status": "completed",
    },
    {
        "id": "hunt-02",
        "symbol": "SPY",
        "buy_order_id": "ord-b2",
        "sell_order_id": "ord-s2",
        "qty": 35,
        "buy_price": 584.85,
        "sell_price": 585.25,
        "pnl": 14.00,
        "pnl_pct": 0.00068,
        "buy_timestamp": (now - timedelta(hours=2)).isoformat(),
        "sell_timestamp": (now - timedelta(hours=1, minutes=55)).isoformat(),
        "status": "completed",
    },
    {
        "id": "hunt-03",
        "symbol": "SPY",
        "buy_order_id": "ord-b3",
        "sell_order_id": "ord-s3",
        "qty": 15,
        "buy_price": 585.40,
        "sell_price": 585.32,
        "pnl": -1.20,
        "pnl_pct": -0.00014,
        "buy_timestamp": (now - timedelta(hours=1)).isoformat(),
        "sell_timestamp": (now - timedelta(minutes=45)).isoformat(),
        "status": "completed",
    },
    {
        "id": "hunt-04",
        "symbol": "SPY",
        "buy_order_id": "ord-b4",
        "sell_order_id": "ord-s4",
        "qty": 40,
        "buy_price": 585.05,
        "sell_price": 585.50,
        "pnl": 18.00,
        "pnl_pct": 0.00077,
        "buy_timestamp": (now - timedelta(minutes=30)).isoformat(),
        "sell_timestamp": (now - timedelta(minutes=24)).isoformat(),
        "status": "completed",
    },
    {
        "id": "hunt-05",
        "symbol": "SPY",
        "buy_order_id": "ord-b5",
        "sell_order_id": "ord-s5",
        "qty": 25,
        "buy_price": 585.12,
        "sell_price": 585.45,
        "pnl": None,
        "pnl_pct": None,
        "buy_timestamp": (now - timedelta(minutes=2)).isoformat(),
        "sell_timestamp": None,
        "status": "open_exit_pending",
    },
]

sample_telemetry = {
    "timestamp": now.isoformat(),
    "symbol": "SPY",
    "best_bid": 585.12,
    "best_ask": 585.15,
    "market_mid": 585.135,
    "fair_price": 585.48,
    "undervaluation_margin": (585.48 - 585.135) / 585.48,
    "min_margin": 0.0008,
    "max_margin": 0.0120,
    "circuit_breaker_active": False,
    "active_hunts_count": 1,
    "completed_trades_count": 4,
    "total_realized_pnl": 36.40,
    "trades": trades,
}

with open(data_dir / "telemetry.json", "w", encoding="utf-8") as f:
    json.dump(sample_telemetry, f, indent=2)

print("Sample telemetry written to data/telemetry.json")
