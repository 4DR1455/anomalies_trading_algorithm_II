"""
Unit tests for Local Persistence System:
- Atomic file operations
- Rolling 50 market hunts FIFO queue
- Quantitative metrics persistence across restarts
- Telemetry snapshot hydration
- Alpaca API order history reconciliation
"""

import os
import shutil
import tempfile
import unittest
from pathlib import Path

from persistence import LocalPersistenceStore, atomic_write_json, safe_read_json
from dashboard.metrics import compute_performance_metrics


class TestLocalPersistence(unittest.TestCase):
    def setUp(self):
        self.test_dir = Path(tempfile.mkdtemp(prefix="persistence_test_"))
        self.store = LocalPersistenceStore(self.test_dir)

    def tearDown(self):
        shutil.rmtree(self.test_dir, ignore_errors=True)

    def test_atomic_write_and_safe_read(self):
        """Verify atomic writing creates target file cleanly with valid JSON."""
        target = self.test_dir / "sample.json"
        data = {"key": "value", "count": 42}

        atomic_write_json(target, data)
        self.assertTrue(target.exists())
        self.assertFalse((self.test_dir / "sample.json.tmp").exists())

        loaded = safe_read_json(target)
        self.assertEqual(loaded, data)

    def test_rolling_50_hunts_persistency(self):
        """Verify that the store maintains up to 50 hunts in rolling FIFO order."""
        # 1. Add 30 hunts
        hunts_batch_1 = [
            {"id": f"hunt-{i}", "symbol": "SOL/USD", "pnl": float(i), "status": "completed"}
            for i in range(30)
        ]
        self.store.save_hunts(hunts_batch_1, max_count=50)

        loaded = self.store.load_hunts(max_count=50)
        self.assertEqual(len(loaded), 30)
        self.assertEqual(loaded[0]["id"], "hunt-0")
        self.assertEqual(loaded[-1]["id"], "hunt-29")

        # 2. Add 40 more hunts (total 70) -> should be capped at 50
        hunts_batch_2 = hunts_batch_1 + [
            {"id": f"hunt-{i}", "symbol": "SOL/USD", "pnl": float(i), "status": "completed"}
            for i in range(30, 70)
        ]
        self.store.save_hunts(hunts_batch_2, max_count=50)

        loaded_50 = self.store.load_hunts(max_count=50)
        self.assertEqual(len(loaded_50), 50)
        self.assertEqual(loaded_50[0]["id"], "hunt-20")  # Oldest kept
        self.assertEqual(loaded_50[-1]["id"], "hunt-69")  # Newest

    def test_metrics_persistence_across_restarts(self):
        """Verify that performance metrics persist and do not reset on restart."""
        sample_trades = [
            {"id": "t1", "pnl": 10.50, "pnl_pct": 0.015, "status": "completed"},
            {"id": "t2", "pnl": -3.20, "pnl_pct": -0.005, "status": "completed"},
            {"id": "t3", "pnl": 15.00, "pnl_pct": 0.021, "status": "completed"},
        ]
        metrics = compute_performance_metrics(sample_trades)
        self.store.save_metrics(metrics)

        # Simulate fresh process restart by creating a new store instance pointing to same dir
        fresh_store = LocalPersistenceStore(self.test_dir)
        loaded_metrics = fresh_store.load_metrics()

        self.assertEqual(loaded_metrics["total_trades"], 3)
        self.assertEqual(loaded_metrics["completed_trades"], 3)
        self.assertEqual(loaded_metrics["total_pnl"], round(10.50 - 3.20 + 15.00, 2))
        self.assertGreater(loaded_metrics["win_rate"], 0.0)
        self.assertIn("timestamp", loaded_metrics)

    def test_telemetry_snapshot_contains_trades_and_metrics(self):
        """Verify that save_telemetry_snapshot bundles persistent hunts and metrics."""
        trades = [{"id": "t1", "pnl": 5.0, "status": "completed"}]
        metrics = {"total_pnl": 5.0, "completed_trades": 1}
        snapshot = {
            "timestamp": "2026-11-18T20:00:00Z",
            "symbol": "SOL/USD",
            "best_bid": 73.0,
            "best_ask": 73.1,
            "fair_price": 73.15,
        }

        self.store.save_telemetry_snapshot(snapshot, trades, metrics)

        telemetry = safe_read_json(self.store.telemetry_file)
        self.assertIn("trades", telemetry)
        self.assertEqual(len(telemetry["trades"]), 1)
        self.assertIn("metrics", telemetry)
        self.assertEqual(telemetry["metrics"]["total_pnl"], 5.0)

    def test_alpaca_closed_orders_reconciliation(self):
        """Verify pairing of Alpaca closed buy and sell orders into completed hunts with valid timestamps."""
        mock_orders = [
            {"id": "buy-1", "symbol": "SOL/USD", "side": "buy", "filled_qty": "2.5", "filled_avg_price": "70.00", "filled_at": "2026-11-18T10:00:00Z"},
            {"id": "sell-1", "symbol": "SOL/USD", "side": "sell", "filled_qty": "2.5", "filled_avg_price": "70.50", "filled_at": "2026-11-18T10:08:00Z"},
            {"id": "buy-2", "symbol": "SOL/USD", "side": "buy", "filled_qty": "1.0", "filled_avg_price": "71.00", "filled_at": "2026-11-18T11:00:00Z"},
            {"id": "sell-2", "symbol": "SOL/USD", "side": "sell", "filled_qty": "1.0", "filled_avg_price": "70.80", "filled_at": "2026-11-18T11:05:00Z"},
        ]

        hunts = LocalPersistenceStore.reconcile_alpaca_orders(mock_orders, symbol="SOL/USD")
        self.assertEqual(len(hunts), 2)

        # First trade: buy @ 70.00, sell @ 70.50, qty 2.5 -> pnl = +1.25
        self.assertEqual(hunts[0]["qty"], 2.5)
        self.assertEqual(hunts[0]["buy_price"], 70.00)
        self.assertEqual(hunts[0]["sell_price"], 70.50)
        self.assertEqual(hunts[0]["pnl"], 1.25)
        self.assertEqual(hunts[0]["buy_timestamp"], "2026-11-18T10:00:00Z")
        self.assertEqual(hunts[0]["sell_timestamp"], "2026-11-18T10:08:00Z")
        self.assertEqual(hunts[0]["status"], "completed")

        # Second trade: buy @ 71.00, sell @ 70.80, qty 1.0 -> pnl = -0.20
        self.assertEqual(hunts[1]["qty"], 1.0)
        self.assertEqual(hunts[1]["pnl"], -0.20)
        self.assertEqual(hunts[1]["buy_timestamp"], "2026-11-18T11:00:00Z")
        self.assertEqual(hunts[1]["sell_timestamp"], "2026-11-18T11:05:00Z")

    def test_alpaca_reconciliation_filters_ancient_orders(self):
        """Verify ancient orders (e.g. August 2026 at $103) are never cross-paired with November sells."""
        mock_orders = [
            # Ancient order from August 31, 2026
            {"id": "old-buy", "symbol": "SOL/USD", "side": "buy", "filled_qty": "10.0", "filled_avg_price": "103.50", "filled_at": "2026-08-31T10:00:00Z"},
            # Recent November order
            {"id": "nov-buy", "symbol": "SOL/USD", "side": "buy", "filled_qty": "1.0", "filled_avg_price": "74.00", "filled_at": "2026-11-18T10:00:00Z", "client_order_id": "hunt-buy-123"},
            {"id": "nov-sell", "symbol": "SOL/USD", "side": "sell", "filled_qty": "1.0", "filled_avg_price": "74.20", "filled_at": "2026-11-18T10:05:00Z", "client_order_id": "hunt-sell-123"},
        ]

        hunts = LocalPersistenceStore.reconcile_alpaca_orders(mock_orders, symbol="SOL/USD")
        self.assertEqual(len(hunts), 1)
        self.assertEqual(hunts[0]["buy_order_id"], "nov-buy")
        self.assertEqual(hunts[0]["sell_order_id"], "nov-sell")
        self.assertEqual(hunts[0]["buy_price"], 74.00)
        self.assertEqual(hunts[0]["sell_price"], 74.20)
        self.assertEqual(hunts[0]["pnl"], 0.20)
        self.assertEqual(hunts[0]["buy_timestamp"], "2026-11-18T10:00:00Z")
        self.assertEqual(hunts[0]["sell_timestamp"], "2026-11-18T10:05:00Z")

    def test_alpaca_temporal_fifo_pairing(self):
        """Verify strict chronological pairing even if orders are shuffled in the list."""
        mock_orders = [
            # Sell for batch 1 (filled later)
            {"id": "s1", "symbol": "SOL/USD", "side": "sell", "filled_qty": "2.0", "filled_avg_price": "75.50", "filled_at": "2026-11-18T12:30:00Z", "created_at": "2026-11-18T12:01:00Z"},
            # Buy batch 1
            {"id": "b1", "symbol": "SOL/USD", "side": "buy", "filled_qty": "2.0", "filled_avg_price": "75.00", "filled_at": "2026-11-18T12:00:00Z", "created_at": "2026-11-18T12:00:00Z"},
            # Buy batch 2
            {"id": "b2", "symbol": "SOL/USD", "side": "buy", "filled_qty": "3.0", "filled_avg_price": "74.00", "filled_at": "2026-11-18T13:00:00Z", "created_at": "2026-11-18T13:00:00Z"},
            # Sell for batch 2
            {"id": "s2", "symbol": "SOL/USD", "side": "sell", "filled_qty": "3.0", "filled_avg_price": "74.50", "filled_at": "2026-11-18T13:10:00Z", "created_at": "2026-11-18T13:01:00Z"},
        ]

        hunts = LocalPersistenceStore.reconcile_alpaca_orders(mock_orders, symbol="SOL/USD")
        self.assertEqual(len(hunts), 2)
        self.assertEqual(hunts[0]["buy_order_id"], "b1")
        self.assertEqual(hunts[0]["sell_order_id"], "s1")
        self.assertEqual(hunts[0]["pnl"], 1.00)

        self.assertEqual(hunts[1]["buy_order_id"], "b2")
        self.assertEqual(hunts[1]["sell_order_id"], "s2")
        self.assertEqual(hunts[1]["pnl"], 1.50)

    def test_alpaca_rejects_exceeding_duration(self):
        """Verify that sell orders more than 24h after buy are not paired."""
        mock_orders = [
            {"id": "b-old", "symbol": "SOL/USD", "side": "buy", "filled_qty": "1.0", "filled_avg_price": "70.00", "filled_at": "2026-11-10T10:00:00Z"},
            {"id": "s-late", "symbol": "SOL/USD", "side": "sell", "filled_qty": "1.0", "filled_avg_price": "75.00", "filled_at": "2026-11-15T10:00:00Z"},
        ]
        hunts = LocalPersistenceStore.reconcile_alpaca_orders(mock_orders, symbol="SOL/USD")
        self.assertEqual(len(hunts), 0)


if __name__ == "__main__":
    unittest.main()
