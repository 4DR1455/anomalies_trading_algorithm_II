"""
Unit & Integration Tests for live_bot.py Bug Fixes:
- Fee-aware & balance-verified crypto exit sizing (reproducing the 332.7229 SOL HTTP 403 bug)
- Automatic HTTP 403 / 422 recovery on exit sell submission (never dropping hunts)
- Terminal status polling after cancel_order (fixing the 17.5ms late-fill race condition)
- 12-hour stale order liquidation (reproducing Hunt 64dc0008 / da4b6a70)
- Partial exit fill tracking & fee-inclusive net P&L
- Orphaned position auto-detection & protection + untracked open order adoption
- Canceled/expired exit order automatic re-raising
- Metrics inclusion of liquidated_stale and $100k base equity
"""

from datetime import datetime, timedelta, timezone
import json
import shutil
import tempfile
import unittest
from pathlib import Path

from live_bot import (
    AlpacaAPIError,
    build_broker_telemetry,
    compute_net_trade_pnl,
    compute_safe_exit_sell_qty,
    floor_qty,
    format_order_qty,
    reconcile_and_maintain_positions,
    submit_exit_sell_with_retry,
    wait_for_terminal_order,
)
from persistence import LocalPersistenceStore
from dashboard.metrics import compute_performance_metrics


class MockBrokerClient:
    """Stateful mock Alpaca client simulating crypto SOL buy fees, HTTP 403 balance checks, and open orders."""

    def __init__(self, equity=100000.0, cash=100000.0, pos_qty=0.0, pos_avail=0.0, avg_entry=73.0, cur_price=72.5):
        self.equity = equity
        self.cash = cash
        self.pos_qty = pos_qty
        self.pos_avail = pos_avail
        self.avg_entry = avg_entry
        self.cur_price = cur_price
        self.orders = {}
        self.order_seq = 0
        self.poll_sequences = {}  # order_id -> list of status dicts to pop sequentially
        self.force_422_next_submit = False

    def get_account(self):
        return {
            "equity": str(self.equity),
            "cash": str(self.cash),
            "buying_power": str(self.cash * 4),
        }

    def get_positions(self, symbol=None):
        if self.pos_qty <= 1e-10:
            return None if symbol else []
        pos = {
            "symbol": "SOLUSD",
            "qty": f"{self.pos_qty:.9f}",
            "qty_available": f"{self.pos_avail:.9f}",
            "avg_entry_price": str(self.avg_entry),
            "current_price": str(self.cur_price),
            "market_value": f"{self.pos_qty * self.cur_price:.2f}",
            "unrealized_pl": f"{(self.cur_price - self.avg_entry) * self.pos_qty:.2f}",
        }
        return pos if symbol else [pos]

    def get_open_orders(self, symbol=None):
        return [
            o for o in self.orders.values()
            if o.get("status") in ("new", "accepted", "pending_new", "partially_filled")
        ]

    def get_order(self, order_id):
        if order_id in self.poll_sequences and self.poll_sequences[order_id]:
            nxt = self.poll_sequences[order_id].pop(0)
            self.orders[order_id] = nxt
            return nxt
        return self.orders[order_id]

    def cancel_order(self, order_id):
        if order_id in self.orders:
            o = self.orders[order_id]
            if o.get("status") not in ("filled", "canceled", "rejected", "expired"):
                rem = float(o["qty"]) - float(o.get("filled_qty") or 0.0)
                if o.get("side") == "sell":
                    self.pos_avail = round(self.pos_avail + rem, 9)
                o["status"] = "canceled"
                o["canceled_at"] = datetime.now(timezone.utc).isoformat()
        return {}

    def submit_order(self, symbol, qty, side, order_type, limit_price=None, tif="gtc"):
        if self.force_422_next_submit:
            self.force_422_next_submit = False
            raise AlpacaAPIError(422, "https://paper-api.alpaca.markets/v2/orders", '{"code":42210000,"message":"unprocessable"}')

        qty_f = float(qty)
        if side == "sell":
            if qty_f > self.pos_avail + 1e-12:
                err_payload = json.dumps({
                    "available": f"{self.pos_avail:.9f}",
                    "balance": f"{self.pos_qty:.9f}",
                    "code": 40310000,
                    "message": f"insufficient balance for SOL (requested: {qty}, available: {self.pos_avail:.9f})",
                    "symbol": "USD",
                })
                raise AlpacaAPIError(403, "https://paper-api.alpaca.markets/v2/orders", err_payload)

            self.order_seq += 1
            oid = f"sell-ord-{self.order_seq}"
            if order_type == "market":
                self.pos_avail = round(self.pos_avail - qty_f, 9)
                self.pos_qty = round(self.pos_qty - qty_f, 9)
                ord_obj = {
                    "id": oid,
                    "symbol": symbol,
                    "side": "sell",
                    "type": "market",
                    "qty": str(qty),
                    "filled_qty": str(qty),
                    "filled_avg_price": str(self.cur_price),
                    "filled_at": datetime.now(timezone.utc).isoformat(),
                    "created_at": datetime.now(timezone.utc).isoformat(),
                    "status": "filled",
                }
            else:
                self.pos_avail = round(self.pos_avail - qty_f, 9)
                ord_obj = {
                    "id": oid,
                    "symbol": symbol,
                    "side": "sell",
                    "type": "limit",
                    "qty": str(qty),
                    "filled_qty": "0",
                    "limit_price": str(limit_price),
                    "created_at": datetime.now(timezone.utc).isoformat(),
                    "status": "new",
                }
            self.orders[oid] = ord_obj
            return ord_obj

        raise NotImplementedError("Mock buy not needed in this test")


class TestLiveBotFixes(unittest.TestCase):
    def setUp(self):
        self.tmp_dir = Path(tempfile.mkdtemp(prefix="live_bot_test_"))
        self.store = LocalPersistenceStore(self.tmp_dir)
        self.params = {
            "symbol": "SOL/USD",
            "stale_order_max_age_seconds": 43200,
        }

    def tearDown(self):
        shutil.rmtree(self.tmp_dir, ignore_errors=True)

    def test_crypto_fee_aware_exit_sizing_reproduces_332_sol_incident(self):
        """
        Reproduces Incident #2 (Nov 19 16:51Z):
        Bought 332.7229 SOL from 0.082164923 free cushion; Alpaca deducted -0.499084350 SOL maker fee,
        yielding avail_after = 332.305980573. Safe exit qty MUST equal exact delta 332.22381565 (<= avail_after)
        so Alpaca NEVER rejects with HTTP 403!
        """
        avail_before = 0.082164923
        avail_after = 332.305980573
        client = MockBrokerClient(pos_qty=882.486814541, pos_avail=avail_after)

        safe_qty, live_avail = compute_safe_exit_sell_qty(
            filled_qty=332.7229,
            symbol="SOL/USD",
            client=client,
            avail_before=avail_before,
            settle_sleep=0.0,
        )
        self.assertAlmostEqual(safe_qty, 332.22381565, places=8)
        self.assertLessEqual(safe_qty, live_avail)

        # Submitting this safe quantity MUST succeed on first attempt with 0 rejections!
        sell_ord, submitted = submit_exit_sell_with_retry(client, "SOL/USD", safe_qty, 73.67, tif="gtc")
        self.assertIsNotNone(sell_ord)
        self.assertEqual(sell_ord["status"], "new")
        self.assertAlmostEqual(submitted, 332.22381565, places=8)

    def test_submit_exit_sell_recovers_from_http_403_insufficient_balance(self):
        """Even if a stale gross qty (332.7229) is passed, HTTP 403 retry adjusts to available 332.305980573 and succeeds."""
        client = MockBrokerClient(pos_qty=882.486814541, pos_avail=332.305980573)
        sell_ord, submitted = submit_exit_sell_with_retry(client, "SOL/USD", 332.7229, 73.67, tif="gtc")
        self.assertIsNotNone(sell_ord)
        self.assertAlmostEqual(submitted, 332.305980573, places=9)
        self.assertAlmostEqual(client.pos_avail, 0.0, places=9)

    def test_submit_exit_sell_handles_http_422_without_none_order_id(self):
        """HTTP 422 must never create an open_exit_pending hunt with sell_order_id=None."""
        client = MockBrokerClient(pos_qty=10.0, pos_avail=10.0)
        client.force_422_next_submit = True
        sell_ord, submitted = submit_exit_sell_with_retry(client, "SOL/USD", 10.0, 75.0, tif="gtc", max_retries=0)
        self.assertIsNone(sell_ord)
        self.assertEqual(submitted, 0.0)

    def test_wait_for_terminal_order_captures_late_partial_fill(self):
        """Reproduces the 17.5ms cancel race: keeps polling until terminal status and captures final filled_qty."""
        client = MockBrokerClient()
        oid = "buy-race-1"
        client.poll_sequences[oid] = [
            {"id": oid, "status": "pending_cancel", "filled_qty": "50.0", "filled_avg_price": "74.38"},
            {"id": oid, "status": "canceled", "filled_qty": "69.6288", "filled_avg_price": "74.38"},
        ]
        term = wait_for_terminal_order(client, oid, max_wait_secs=2.0, poll_interval=0.0)
        self.assertEqual(term["status"], "canceled")
        self.assertEqual(term["filled_qty"], "69.6288")

    def test_reconcile_retries_exit_submit_pending_hunt(self):
        """A hunt persisted before a transient exit failure is automatically protected on the next cycle."""
        client = MockBrokerClient(pos_qty=100.0, pos_avail=100.0, avg_entry=70.0, cur_price=70.1)
        trades = [{
            "id": "pend0001",
            "symbol": "SOL/USD",
            "buy_order_id": "b1",
            "sell_order_id": None,
            "buy_qty": 100.15,
            "qty": 100.0,
            "filled_exit_qty": 0.0,
            "buy_price": 70.0,
            "sell_price": 70.35,
            "pnl": None,
            "pnl_pct": None,
            "buy_timestamp": "2026-11-21T08:00:00Z",
            "sell_timestamp": None,
            "status": "exit_submit_pending",
        }]
        changed, tele = reconcile_and_maintain_positions(
            client, "SOL/USD", trades, self.params, fair_price=70.35, bid=70.1, store=self.store
        )
        self.assertTrue(changed)
        self.assertEqual(trades[0]["status"], "open_exit_pending")
        self.assertIsNotNone(trades[0]["sell_order_id"])
        self.assertTrue(tele["reconciliation"]["is_synced"])
        self.assertAlmostEqual(tele["reconciliation"]["broker_qty_available"], 0.0, places=6)

    def test_reconcile_tracks_partially_filled_and_completes_with_fee_inclusive_pnl(self):
        """Tracks partially_filled exit orders accurately and computes fee-inclusive net P&L on completion."""
        client = MockBrokerClient(pos_qty=100.0, pos_avail=0.0, avg_entry=70.0, cur_price=70.5)
        now = datetime(2026, 11, 21, 10, 0, 0, tzinfo=timezone.utc)
        client.orders["s-part"] = {
            "id": "s-part",
            "symbol": "SOL/USD",
            "side": "sell",
            "type": "limit",
            "qty": "99.85",
            "filled_qty": "40.0",
            "limit_price": "70.50",
            "created_at": (now - timedelta(minutes=30)).isoformat(),
            "status": "partially_filled",
        }
        trades = [{
            "id": "h-part",
            "symbol": "SOL/USD",
            "buy_order_id": "b-part",
            "sell_order_id": "s-part",
            "buy_qty": 100.0,
            "qty": 99.85,
            "filled_exit_qty": 0.0,
            "buy_price": 70.00,
            "sell_price": 70.50,
            "pnl": None,
            "pnl_pct": None,
            "buy_timestamp": (now - timedelta(minutes=31)).isoformat(),
            "sell_timestamp": None,
            "status": "open_exit_pending",
        }]
        changed, tele = reconcile_and_maintain_positions(client, "SOL/USD", trades, self.params, fair_price=70.4, bid=70.3, now_dt=now)
        self.assertTrue(changed)
        self.assertEqual(trades[0]["status"], "exit_partially_filled")
        self.assertAlmostEqual(trades[0]["filled_exit_qty"], 40.0)
        # Tracked remaining qty should be 99.85 - 40.0 = 59.85
        self.assertAlmostEqual(tele["reconciliation"]["tracked_active_qty"], 59.85, places=4)

        # Now order fills completely
        client.orders["s-part"]["status"] = "filled"
        client.orders["s-part"]["filled_qty"] = "99.85"
        client.orders["s-part"]["filled_avg_price"] = "70.50"
        client.orders["s-part"]["filled_at"] = now.isoformat()
        client.pos_qty = 0.0

        changed2, tele2 = reconcile_and_maintain_positions(client, "SOL/USD", trades, self.params, fair_price=70.5, bid=70.5, now_dt=now)
        self.assertTrue(changed2)
        self.assertEqual(trades[0]["status"], "completed")
        # Bought 100 @ 70.00 ($7000 cost); sold 99.85 @ 70.50 ($7039.425 gross) - $10.56 sell fee = $28.865 -> $28.86
        self.assertAlmostEqual(trades[0]["pnl"], 28.86, places=2)
        self.assertAlmostEqual(trades[0]["gross_pnl"], 50.00, places=2)
        self.assertAlmostEqual(trades[0]["fees_usd"], 21.06, places=2)
        self.assertTrue(tele2["reconciliation"]["is_synced"])

    def test_reconcile_liquidates_stale_order_after_12h(self):
        """Reproduces Hunt 64dc0008: open exit sell older than 12h is canceled and liquidated at market."""
        now = datetime(2026, 11, 20, 9, 0, 0, tzinfo=timezone.utc)
        created_17h_ago = (now - timedelta(hours=17, minutes=30)).isoformat()
        client = MockBrokerClient(pos_qty=111.2966, pos_avail=0.0, avg_entry=74.48, cur_price=72.49)
        client.orders["da4b6a70"] = {
            "id": "da4b6a70",
            "symbol": "SOL/USD",
            "side": "sell",
            "type": "limit",
            "qty": "111.2966",
            "filled_qty": "0",
            "limit_price": "74.73",
            "created_at": created_17h_ago,
            "status": "new",
        }
        trades = [{
            "id": "64dc0008",
            "symbol": "SOL/USD",
            "buy_order_id": "ba1e3bc2",
            "sell_order_id": "da4b6a70",
            "buy_qty": 111.4638,
            "qty": 111.2966,
            "filled_exit_qty": 0.0,
            "buy_price": 74.48,
            "sell_price": 74.73,
            "pnl": None,
            "pnl_pct": None,
            "buy_timestamp": created_17h_ago,
            "sell_timestamp": None,
            "status": "open_exit_pending",
        }]
        changed, tele = reconcile_and_maintain_positions(
            client, "SOL/USD", trades, self.params, fair_price=72.50, bid=72.49, now_dt=now
        )
        self.assertTrue(changed)
        self.assertEqual(trades[0]["status"], "liquidated_stale")
        self.assertLess(trades[0]["pnl"], 0.0)
        self.assertAlmostEqual(client.pos_qty, 0.0, places=6)
        self.assertTrue(tele["reconciliation"]["is_synced"])

        # Metrics MUST include liquidated_stale losses!
        metrics = compute_performance_metrics(trades)
        self.assertEqual(metrics["completed_trades"], 1)
        self.assertLess(metrics["total_pnl"], 0.0)

    def test_reconcile_auto_detects_and_protects_orphaned_330_sol(self):
        """Reproduces the 330.839269062 SOL unhedged orphan: bot detects, records, and places protective SELL."""
        client = MockBrokerClient(pos_qty=330.839269062, pos_avail=330.839269062, avg_entry=73.43, cur_price=72.49)
        trades = []
        changed, tele = reconcile_and_maintain_positions(
            client, "SOL/USD", trades, self.params, fair_price=72.60, bid=72.49, store=self.store
        )
        self.assertTrue(changed)
        self.assertEqual(len(trades), 1)
        self.assertTrue(trades[0]["recovered_orphan"])
        self.assertEqual(trades[0]["status"], "open_exit_pending")
        self.assertAlmostEqual(trades[0]["qty"], 330.839269062, places=9)
        self.assertIsNotNone(trades[0]["sell_order_id"])
        # After protection, broker_qty_available must be 0 and synced must be True!
        self.assertAlmostEqual(client.pos_avail, 0.0, places=9)
        self.assertTrue(tele["reconciliation"]["is_synced"])

    def test_reconcile_adopts_untracked_open_broker_sell_order(self):
        """If an open SELL exists on Alpaca but is missing from local JSON, bot adopts it automatically."""
        client = MockBrokerClient(pos_qty=50.0, pos_avail=0.0, avg_entry=71.0, cur_price=70.5)
        client.orders["ext-sell-1"] = {
            "id": "ext-sell-1",
            "symbol": "SOL/USD",
            "side": "sell",
            "type": "limit",
            "qty": "50.0",
            "filled_qty": "0",
            "limit_price": "71.50",
            "created_at": datetime.now(timezone.utc).isoformat(),
            "status": "new",
        }
        trades = []
        changed, tele = reconcile_and_maintain_positions(client, "SOL/USD", trades, self.params, fair_price=70.8, bid=70.5)
        self.assertTrue(changed)
        self.assertEqual(len(trades), 1)
        self.assertEqual(trades[0]["sell_order_id"], "ext-sell-1")
        self.assertTrue(tele["reconciliation"]["is_synced"])
        self.assertAlmostEqual(tele["reconciliation"]["untracked_qty"], 0.0, places=6)

    def test_reconcile_reraises_canceled_exit_order(self):
        """If an open exit SELL is canceled externally while shares remain on Alpaca, bot automatically re-raises it."""
        client = MockBrokerClient(pos_qty=25.0, pos_avail=25.0, avg_entry=71.0, cur_price=70.8)
        client.orders["s-canc"] = {
            "id": "s-canc",
            "symbol": "SOL/USD",
            "side": "sell",
            "type": "limit",
            "qty": "25.0",
            "filled_qty": "0",
            "limit_price": "71.40",
            "status": "canceled",
            "canceled_at": datetime.now(timezone.utc).isoformat(),
        }
        trades = [{
            "id": "h-canc",
            "symbol": "SOL/USD",
            "buy_order_id": "b-canc",
            "sell_order_id": "s-canc",
            "buy_qty": 25.0375,
            "qty": 25.0,
            "filled_exit_qty": 0.0,
            "buy_price": 71.0,
            "sell_price": 71.40,
            "pnl": None,
            "pnl_pct": None,
            "buy_timestamp": datetime.now(timezone.utc).isoformat(),
            "sell_timestamp": None,
            "status": "open_exit_pending",
        }]
        changed, tele = reconcile_and_maintain_positions(client, "SOL/USD", trades, self.params, fair_price=71.2, bid=70.8)
        self.assertTrue(changed)
        self.assertEqual(trades[0]["status"], "open_exit_pending")
        self.assertNotEqual(trades[0]["sell_order_id"], "s-canc")
        self.assertTrue(tele["reconciliation"]["is_synced"])

    def test_format_order_qty_avoids_scientific_notation_and_floors(self):
        self.assertEqual(format_order_qty(332.3059805739, is_crypto=True), "332.305980573")
        self.assertEqual(format_order_qty(0.00001, is_crypto=True), "0.00001")
        self.assertEqual(floor_qty(1.9999999999, 9), 1.999999999)

    def test_compute_portfolio_history_metrics_matches_alpaca_curve(self):
        from dashboard.metrics import compute_portfolio_history_metrics

        ph = {
            "timestamp": [
                1770595200,  # 2026-02-09 (pre-funding 0)
                1770681600,  # 2026-02-10 ($100,000)
                1770768000,  # 2026-02-11 ($120,000)
                1770854400,  # 2026-02-12 ($108,000 -> -10% DD / -$12,000)
                1770940800,  # 2026-02-13 ($150,000)
            ],
            "equity": [0.0, 100000.0, 120000.0, 108000.0, 150000.0],
        }
        m = compute_portfolio_history_metrics(ph, live_equity=None, trades=[])
        self.assertEqual(m["source"], "alpaca_portfolio_history")
        self.assertEqual(m["start_date"], "2026-02-10")
        self.assertEqual(m["total_pnl"], 50000.0)
        self.assertEqual(m["total_return_pct"], 50.0)
        self.assertEqual(m["max_drawdown_pct"], 10.0)
        self.assertEqual(m["max_drawdown_dollars"], 12000.0)
        self.assertEqual(m["cumulative_pnl_series"], [0.0, 20000.0, 8000.0, 50000.0])
        self.assertEqual(m["chart_labels"], ["2026-02-10", "2026-02-11", "2026-02-12", "2026-02-13"])
        self.assertGreater(m["sharpe_ratio"], 0.0)


if __name__ == "__main__":
    unittest.main()
