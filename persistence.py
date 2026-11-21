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
from datetime import datetime, timezone, timedelta
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


ACTIVE_HUNT_STATUSES = {"open_exit_pending", "exit_submit_pending", "exit_partially_filled"}


def trim_hunts_preserving_active(hunts: List[Dict[str, Any]], max_count: int = 50) -> List[Dict[str, Any]]:
    """
    Trims hunt history to the latest max_count terminal hunts while ALWAYS preserving
    every active/non-terminal hunt so pending exits are never silently evicted.
    Maintains original chronological ordering.
    """
    if not hunts:
        return []

    active_indices = set()
    terminal_indices = []
    for idx, h in enumerate(hunts):
        if h.get("status") in ACTIVE_HUNT_STATUSES:
            active_indices.add(idx)
        else:
            terminal_indices.append(idx)

    kept_terminal = set(terminal_indices[-max_count:]) if max_count > 0 else set()
    kept = active_indices | kept_terminal
    return [h for idx, h in enumerate(hunts) if idx in kept]


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
        Preserves all active/open hunts plus up to the last max_count completed trades (default 50).
        """
        # 1. Primary store: hunts_history.json
        hunts = safe_read_json(self.hunts_file)
        if isinstance(hunts, list) and len(hunts) > 0:
            return trim_hunts_preserving_active(hunts, max_count=max_count)

        # 2. Fallback: telemetry.json trades array
        telemetry = safe_read_json(self.telemetry_file)
        if isinstance(telemetry, dict):
            trades = telemetry.get("trades")
            if isinstance(trades, list) and len(trades) > 0:
                trimmed = trim_hunts_preserving_active(trades, max_count=max_count)
                # Migrate recovered trades into hunts_history.json immediately
                self.save_hunts(trimmed, max_count=max_count)
                return trimmed

        return []

    def save_hunts(self, hunts: List[Dict[str, Any]], max_count: int = 50) -> None:
        """Atomically persists the market hunts list, preserving all active hunts plus the last max_count completed."""
        rolling_hunts = trim_hunts_preserving_active(hunts, max_count=max_count) if hunts else []
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
        """Atomically writes telemetry snapshot embedding all active hunts + last 50 completed hunts and metrics."""
        snap = dict(snapshot)
        snap["trades"] = trim_hunts_preserving_active(hunts, max_count=50)
        snap["metrics"] = metrics
        atomic_write_json(self.telemetry_file, snap)

    def record_hunt(self, hunt: Dict[str, Any], compute_metrics_fn=None) -> Tuple[List[Dict[str, Any]], Dict[str, Any]]:
        """
        Appends a new market hunt, trims preserving active hunts, updates metrics, and persists all stores.
        """
        hunts = self.load_hunts(max_count=50)
        hunts.append(hunt)
        hunts = trim_hunts_preserving_active(hunts, max_count=50)
        self.save_hunts(hunts, max_count=50)

        if compute_metrics_fn:
            metrics = compute_metrics_fn(hunts)
            self.save_metrics(metrics)
        else:
            metrics = self.load_metrics()

        return hunts, metrics

    @staticmethod
    def reconcile_alpaca_orders(
        orders: List[Dict[str, Any]],
        symbol: str = "SOL/USD",
        min_date: Optional[str] = "2026-11-01",
    ) -> List[Dict[str, Any]]:
        """
        Reconstructs completed round-trip hunts from Alpaca API closed order history
        if local history was cleared or uninitialized.
        Filters strictly for active algorithm deployment orders (client_order_id containing 'hunt-'
        or timestamps >= min_date) and performs temporal FIFO order reconciliation (< 24h duration)
        with strict quantity compatibility (allowing 0.15%-0.50% crypto buy-fee deduction)
        so orphaned buys can never shift or cross-pair unrelated sells.
        """
        def parse_iso(ts_str: Optional[str]) -> Optional[datetime]:
            if not ts_str:
                return None
            try:
                clean_str = ts_str.replace("Z", "+00:00")
                dt = datetime.fromisoformat(clean_str)
                if dt.tzinfo is None:
                    dt = dt.replace(tzinfo=timezone.utc)
                return dt
            except Exception:
                return None

        def qty_matches(b_qty: float, s_qty: float) -> bool:
            if b_qty <= 0 or s_qty <= 0:
                return False
            # Accept exact qty OR fee-deducted net crypto qty (up to 0.50% below buy qty)
            lower = b_qty * 0.995 - 1e-4
            upper = b_qty * 1.0005 + 1e-4
            return lower <= s_qty <= upper

        buys = []
        sells = []
        for o in orders:
            if o.get("symbol") != symbol:
                continue
            qty = float(o.get("filled_qty") or 0.0)
            if qty <= 0.0:
                continue

            cid = str(o.get("client_order_id") or "")
            ts = str(o.get("filled_at") or o.get("created_at") or "")

            # Deployment filter: only accept orders from active deployment
            if min_date and ts and ts < min_date:
                continue
            if not ("hunt-" in cid or (min_date and ts and ts >= min_date)):
                continue

            f_time = parse_iso(ts) or datetime.min.replace(tzinfo=timezone.utc)
            c_time = parse_iso(o.get("created_at")) or f_time

            item = (f_time, c_time, o)
            if o.get("side") == "buy":
                buys.append(item)
            elif o.get("side") == "sell":
                sells.append(item)

        # Sort buys chronologically by fill time
        buys.sort(key=lambda x: x[0])
        # Sort sells chronologically by order submission / creation time
        sells.sort(key=lambda x: x[1])

        used_sells = set()
        hunts = []

        for b_f_time, b_c_time, b in buys:
            b_qty = float(b.get("filled_qty") or 0.0)
            matched_idx = None

            # Pass 1: Match sell created shortly after buy fill (within 10m) with compatible quantity
            for s_idx, (s_f_time, s_c_time, s) in enumerate(sells):
                if s_idx in used_sells:
                    continue
                s_qty = float(s.get("filled_qty") or 0.0)
                if s_f_time >= b_f_time and (s_f_time - b_f_time) < timedelta(hours=24):
                    if qty_matches(b_qty, s_qty) and s_c_time >= b_f_time - timedelta(seconds=30) and (s_c_time - b_f_time) < timedelta(minutes=10):
                        matched_idx = s_idx
                        break

            # Pass 2: Strict temporal FIFO fallback (< 24h holding duration) STILL requiring quantity compatibility
            if matched_idx is None:
                for s_idx, (s_f_time, s_c_time, s) in enumerate(sells):
                    if s_idx in used_sells:
                        continue
                    s_qty = float(s.get("filled_qty") or 0.0)
                    if s_f_time >= b_f_time and (s_f_time - b_f_time) < timedelta(hours=24) and qty_matches(b_qty, s_qty):
                        matched_idx = s_idx
                        break

            if matched_idx is not None:
                used_sells.add(matched_idx)
                s = sells[matched_idx][2]
                s_qty = float(s.get("filled_qty") or b_qty)
                b_price = float(b.get("filled_avg_price") or b.get("limit_price") or 0.0)
                s_price = float(s.get("filled_avg_price") or s.get("limit_price") or 0.0)
                cost_basis = b_price * b_qty
                proceeds = s_price * s_qty
                pnl = round(proceeds - cost_basis, 2)
                pnl_pct = round(pnl / cost_basis, 4) if cost_basis > 0 else 0.0

                hunts.append({
                    "id": str(b.get("id", ""))[:8],
                    "symbol": symbol,
                    "buy_order_id": b.get("id"),
                    "sell_order_id": s.get("id"),
                    "buy_qty": b_qty,
                    "qty": s_qty,
                    "buy_price": b_price,
                    "sell_price": s_price,
                    "pnl": pnl,
                    "pnl_pct": pnl_pct,
                    "buy_timestamp": b.get("filled_at") or b.get("created_at"),
                    "sell_timestamp": s.get("filled_at") or s.get("created_at"),
                    "status": "completed",
                })

        return hunts[-50:]
