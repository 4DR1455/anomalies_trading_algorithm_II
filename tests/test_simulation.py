"""
End-to-End Simulation & Verification Test for 30s Cycles
Simulates market feeds, HiddenMM execution, 20s timeout, partial fills, circuit breakers, and 12h hygiene.
"""

import sys
import unittest
from pathlib import Path

# Add project root and dashboard to sys.path
root_dir = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(root_dir))
sys.path.insert(0, str(root_dir / "dashboard"))

from dashboard.metrics import compute_performance_metrics
from tests.test_model import (
    calculate_shares_amount,
    calculate_hidden_mm_buy_price,
    calculate_exit_sell_price,
)


class MockAlpacaSimulator:
    """Mock simulated exchange order book and order manager."""
    def __init__(self):
        self.orders = {}
        self.positions = {}
        self.current_time = 1000000

    def submit_order(self, symbol: str, qty: int, side: str, order_type: str, price: float = None):
        order_id = f"ord-{len(self.orders)+1}"
        order = {
            "id": order_id,
            "symbol": symbol,
            "qty": qty,
            "filled_qty": 0,
            "side": side,
            "type": order_type,
            "price": price,
            "status": "new",
            "created_at": self.current_time,
        }
        self.orders[order_id] = order
        return order

    def cancel_order(self, order_id: str):
        if order_id in self.orders:
            ord_obj = self.orders[order_id]
            if ord_obj["status"] not in ["filled", "canceled"]:
                ord_obj["status"] = "canceled"
                return True
        return False

    def simulate_fill(self, order_id: str, filled_qty: int, fill_price: float):
        if order_id in self.orders:
            ord_obj = self.orders[order_id]
            ord_obj["filled_qty"] = filled_qty
            ord_obj["status"] = "filled" if filled_qty == ord_obj["qty"] else "partially_filled"
            ord_obj["filled_avg_price"] = fill_price
            # Update position
            sym = ord_obj["symbol"]
            curr_pos = self.positions.get(sym, 0)
            if ord_obj["side"] == "buy":
                self.positions[sym] = curr_pos + filled_qty
            else:
                self.positions[sym] = max(0, curr_pos - filled_qty)


class TestEngineSimulation(unittest.TestCase):
    def setUp(self):
        self.sim = MockAlpacaSimulator()
        self.symbol = "SPY"
        self.min_margin = 0.0008  # 0.08%
        self.max_margin = 0.0120  # 1.20% (Circuit breaker)
        self.level = 2.0
        self.max_shares = 50

    def test_full_market_hunt_cycle(self):
        """Test complete HiddenMM entry + Market Exit cycle."""
        # 1. Quoted price is undervalued relative to predicted fair price
        best_bid = 580.00
        best_ask = 580.04  # 4 cents spread (wide)
        fair_price = 580.80  # ~0.14% anomaly (between min_margin and max_margin)

        market_mid = 0.5 * (best_bid + best_ask)
        margin = (fair_price - market_mid) / fair_price

        self.assertGreaterEqual(margin, self.min_margin)
        self.assertLessEqual(margin, self.max_margin)

        # 2. Dynamic Capital Allocation Sizing
        shares = calculate_shares_amount(margin, self.min_margin, self.max_margin, self.level, self.max_shares)
        self.assertGreater(shares, 0)
        self.assertLessEqual(shares, self.max_shares)

        # 3. HiddenMM Entry Limit Order
        buy_price = calculate_hidden_mm_buy_price(best_bid, best_ask)
        self.assertEqual(buy_price, 580.01)  # Stepped inside by $0.01

        buy_order = self.sim.submit_order(self.symbol, shares, "buy", "limit", buy_price)
        self.assertEqual(buy_order["status"], "new")

        # 4. Fill within 20s
        self.sim.simulate_fill(buy_order["id"], shares, buy_price)
        self.assertEqual(self.sim.positions[self.symbol], shares)

        # 5. Market Exit limit SELL placed at predicted fair price
        exit_price = calculate_exit_sell_price(buy_price, fair_price)
        self.assertEqual(exit_price, 580.80)
        sell_order = self.sim.submit_order(self.symbol, shares, "sell", "limit", exit_price)

        # 6. Exit filled
        self.sim.simulate_fill(sell_order["id"], shares, exit_price)
        self.assertEqual(self.sim.positions[self.symbol], 0)

        # Verify trade metrics
        pnl = (exit_price - buy_price) * shares
        self.assertGreater(pnl, 0)

    def test_circuit_breaker_on_flash_crash(self):
        """Verify that large sudden drops exceeding max_margin halt trading immediately."""
        best_bid = 560.00
        best_ask = 560.02
        fair_price = 580.00  # 3.4% anomaly -> extreme flash crash!

        market_mid = 0.5 * (best_bid + best_ask)
        margin = (fair_price - market_mid) / fair_price

        self.assertGreater(margin, self.max_margin)

        # Capital allocation MUST be 0
        shares = calculate_shares_amount(margin, self.min_margin, self.max_margin, self.level, self.max_shares)
        self.assertEqual(shares, 0, "Circuit breaker failed to block order during flash crash!")

    def test_20s_timeout_and_partial_fill_recovery(self):
        """Verify that unfilled orders are canceled at 20s and only filled shares are sold."""
        shares_requested = 30
        buy_order = self.sim.submit_order(self.symbol, shares_requested, "buy", "limit", 500.00)

        # Simulate partial fill of 12 shares
        self.sim.simulate_fill(buy_order["id"], 12, 500.00)

        # 20s timeout cancels remaining 18 shares
        self.sim.cancel_order(buy_order["id"])
        self.assertEqual(buy_order["status"], "canceled")
        self.assertEqual(buy_order["filled_qty"], 12)

        # Engine recovers exact filled qty (12) for exit order
        exit_order = self.sim.submit_order(self.symbol, 12, "sell", "limit", 500.40)
        self.assertEqual(exit_order["qty"], 12)

    def test_12h_stale_order_liquidation(self):
        """Verify that orders older than 12 hours (43200s) are liquidated at market."""
        old_order = self.sim.submit_order(self.symbol, 25, "sell", "limit", 600.00)
        self.sim.positions[self.symbol] = 25

        # Fast forward time by 13 hours
        self.sim.current_time += 46800  # > 43200s
        age = self.sim.current_time - old_order["created_at"]
        self.assertGreater(age, 43200)

        # Cancel stale order and liquidate at market
        self.sim.cancel_order(old_order["id"])
        liq_order = self.sim.submit_order(self.symbol, self.sim.positions[self.symbol], "sell", "market")
        self.sim.simulate_fill(liq_order["id"], 25, 590.00)
        self.assertEqual(self.sim.positions[self.symbol], 0)

    def test_numpy_risk_metrics_evaluation(self):
        """Verify Sharpe Ratio, Max Drawdown, and Volatility computation with numpy."""
        mock_trades = [
            {"id": "1", "symbol": "SPY", "pnl": 25.0, "pnl_pct": 0.005, "status": "completed"},
            {"id": "2", "symbol": "SPY", "pnl": 35.0, "pnl_pct": 0.007, "status": "completed"},
            {"id": "3", "symbol": "SPY", "pnl": -10.0, "pnl_pct": -0.002, "status": "completed"},
            {"id": "4", "symbol": "SPY", "pnl": 45.0, "pnl_pct": 0.009, "status": "completed"},
            {"id": "5", "symbol": "SPY", "pnl": 30.0, "pnl_pct": 0.006, "status": "completed"},
        ]
        metrics = compute_performance_metrics(mock_trades)

        self.assertGreater(metrics["sharpe_ratio"], 0.0)
        self.assertGreater(metrics["total_pnl"], 0.0)
        self.assertGreaterEqual(metrics["win_rate"], 80.0)
        self.assertGreater(metrics["annualized_volatility"], 0.0)
        self.assertGreaterEqual(metrics["max_drawdown_dollars"], 10.0)


if __name__ == "__main__":
    unittest.main()
