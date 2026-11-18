"""
Local Persistence Engine for Anomalies Trading Algorithm v2.0
Provides crash-safe, atomic local file storage for:
1. The last 50 market hunts (data/hunts_history.json)
2. Quantitative performance & risk metrics (data/metrics.json)
3. Real-time telemetry snapshots (data/telemetry.json)

Ensures state and metrics are preserved across bot restarts, web dashboard changes,
and process reloads without starting over.
"""

import json
import os
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Dict, List, Optional, Tuple


def atomic_write_json(filepath: Path, data: Any) -> None:
    """Writes data to a temporary file, fsyncs to disk, and atomically replaces target file."""
    filepath = Path(filepath)
    filepath.parent.mkdir(parents=True, exist_ok=True)
    temp_path = filepath.with_suffix(filepath.suffix + ".tmp")

    with open(temp_path, "w", encoding="utf-8") as f:
        json.dump(data, f, indent=2)
        f.flush()
        os.fsync(f.fileno())

    os.replace(temp_path, filepath)


def safe_read_json(filepath: Path, default: Any = None) -> Any:
    """Safely reads JSON from disk, returning default on missing or corrupted files."""
    filepath = Path(filepath)
    if not filepath.exists():
        return default
    try:
        with open(filepath, "r", encoding="utf-8") as f:
            return json.load(f)
    except Exception as exc:
        print(f"[WARN] Could not read {filepath}: {exc}")
        return default


class LocalPersistenceStore:
    def __init__(self, data_dir: Path):
        self.data_dir = Path(data_dir)
        self.data_dir.mkdir(parents=True, exist_ok=True)
        self.hunts_file = self.data_dir / "hunts_history.json"
        self.metrics_file = self.data_dir / "metrics.json"
        self.telemetry_file = self.data_dir / "telemetry.json"

    def load_hunts(self, max_count: int = 50) -> List[Dict[str, Any]]:
        """
        Loads market hunts from local persistent storage.
        Checks hunts_history.json first, falls back to telemetry.json.
        Returns up to the last max_count trades (default 50).
        """
        # 1. Primary store: hunts_history.json
        hunts = safe_read_json(self.hunts_file)
        if isinstance(hunts, list) and len(hunts) > 0:
            return hunts[-max_count:]

        # 2. Fallback: telemetry.json trades array
        telemetry = safe_read_json(self.telemetry_file)
        if isinstance(telemetry, dict):
            trades = telemetry.get("trades")
            if isinstance(trades, list) and len(trades) > 0:
                # Migrate recovered trades into hunts_history.json immediately
                self.save_hunts(trades, max_count=max_count)
                return trades[-max_count:]

        return []

    def save_hunts(self, hunts: List[Dict[str, Any]], max_count: int = 50) -> None:
        """Atomically persists the market hunts list, capped to the last max_count (50)."""
        rolling_hunts = hunts[-max_count:] if hunts else []
        atomic_write_json(self.hunts_file, rolling_hunts)

    def load_metrics(self) -> Dict[str, Any]:
        """Loads persistent performance metrics from disk."""
        metrics = safe_read_json(self.metrics_file)
        if isinstance(metrics, dict) and metrics.get("timestamp"):
            return metrics

        # Fallback default zero metrics if no history exists yet
        return {
            "timestamp": datetime.now(timezone.utc).isoformat(),
            "total_trades": 0,
            "completed_trades": 0,
            "win_rate": 0.0,
            "total_pnl": 0.0,
            "sharpe_ratio": 0.0,
            "max_drawdown_pct": 0.0,
            "max_drawdown_dollars": 0.0,
            "annualized_volatility": 0.0,
            "avg_pnl_per_trade": 0.0,
            "profit_factor": 0.0,
            "cumulative_pnl_series": [0.0],
            "drawdown_series": [0.0],
        }

    def save_metrics(self, metrics: Dict[str, Any]) -> None:
        """Atomically persists performance metrics to disk."""
        metrics_copy = dict(metrics)
        metrics_copy["timestamp"] = datetime.now(timezone.utc).isoformat()
        atomic_write_json(self.metrics_file, metrics_copy)

    def save_telemetry_snapshot(self, snapshot: Dict[str, Any], hunts: List[Dict[str, Any]], metrics: Dict[str, Any]) -> None:
        """Atomically writes telemetry snapshot embedding the persistent last 50 hunts and metrics."""
        snap = dict(snapshot)
        snap["trades"] = hunts[-50:]
        snap["metrics"] = metrics
        atomic_write_json(self.telemetry_file, snap)

    def record_hunt(self, hunt: Dict[str, Any], compute_metrics_fn=None) -> Tuple[List[Dict[str, Any]], Dict[str, Any]]:
        """
        Appends a new market hunt, trims to 50, updates metrics, and persists all stores.
        """
        hunts = self.load_hunts(max_count=50)
        hunts.append(hunt)
        hunts = hunts[-50:]
        self.save_hunts(hunts, max_count=50)

        if compute_metrics_fn:
            metrics = compute_metrics_fn(hunts)
            self.save_metrics(metrics)
        else:
            metrics = self.load_metrics()

        return hunts, metrics

    @staticmethod
    def reconcile_alpaca_orders(orders: List[Dict[str, Any]], symbol: str = "SOL/USD") -> List[Dict[str, Any]]:
        """
        Reconstructs completed round-trip hunts from Alpaca API closed order history
        if local history was cleared or uninitialized.
        """
        buys = []
        sells = []
        for o in orders:
            if o.get("symbol") == symbol and float(o.get("filled_qty", 0)) > 0:
                if o.get("side") == "buy":
                    buys.append(o)
                elif o.get("side") == "sell":
                    sells.append(o)

        hunts = []
        for i in range(min(len(buys), len(sells))):
            b = buys[i]
            s = sells[i]
            b_price = float(b.get("filled_avg_price") or b.get("limit_price") or 0.0)
            s_price = float(s.get("filled_avg_price") or s.get("limit_price") or 0.0)
            qty = float(b.get("filled_qty") or 0.0)
            pnl = round((s_price - b_price) * qty, 2)
            pnl_pct = round((s_price - b_price) / b_price, 4) if b_price > 0 else 0.0

            hunts.append({
                "id": str(b.get("id", ""))[:8],
                "symbol": symbol,
                "buy_order_id": b.get("id"),
                "sell_order_id": s.get("id"),
                "qty": qty,
                "buy_price": b_price,
                "sell_price": s_price,
                "pnl": pnl,
                "pnl_pct": pnl_pct,
                "buy_timestamp": b.get("filled_at") or b.get("created_at"),
                "sell_timestamp": s.get("filled_at") or s.get("created_at"),
                "status": "completed",
            })

        return hunts[-50:]
