#!/usr/bin/env python3
"""
Anomalies Trading Algorithm v2.0 - Live Bot Runner
Connects directly to Alpaca API (Paper Trading) using params.json and .env.
Executes the Adaptive 1D Kalman Filter, HiddenMM entry, 20s cancel, and exit limit sells.
Writes real-time snapshots to data/telemetry.json.
"""

import json
import os
import signal
import sys
import time
import urllib.request
import urllib.error
from datetime import datetime, timezone
from pathlib import Path
import uuid

# Ensure root paths
ROOT_DIR = Path(__file__).resolve().parent
DATA_DIR = ROOT_DIR / "data"
DATA_DIR.mkdir(exist_ok=True)

sys.path.insert(0, str(ROOT_DIR))
sys.path.insert(0, str(ROOT_DIR / "dashboard"))

from persistence import LocalPersistenceStore
from metrics import compute_performance_metrics

RUNNING = True


def handle_sigint(sig, frame):
    global RUNNING
    print("\n[INFO] Shutdown signal received. Exiting cleanly...")
    RUNNING = False


signal.signal(signal.SIGINT, handle_sigint)
signal.signal(signal.SIGTERM, handle_sigint)


def load_config():
    with open(ROOT_DIR / "params.json", "r", encoding="utf-8") as f:
        params = json.load(f)

    # Read .env
    key = os.environ.get("APCA_API_KEY_ID")
    secret = os.environ.get("APCA_API_SECRET_KEY")
    endpoint = os.environ.get("APCA_API_BASE_URL", "https://paper-api.alpaca.markets/v2")

    env_path = ROOT_DIR / ".env"
    if (not key or not secret) and env_path.exists():
        with open(env_path, "r", encoding="utf-8") as f:
            for line in f:
                line = line.strip()
                if not line or line.startswith("#"):
                    continue
                if ":" in line:
                    k, v = line.split(":", 1)
                    k, v = k.strip().lower(), v.strip()
                    if k in ["key", "apca_api_key_id"]:
                        key = key or v
                    elif k in ["secret", "apca_api_secret_key"]:
                        secret = secret or v
                    elif k in ["end point", "endpoint", "apca_api_base_url"]:
                        endpoint = endpoint or v

    if not key or not secret:
        raise ValueError("Missing APCA-API-KEY-ID or APCA-API-SECRET-KEY in .env")

    return params, key, secret, endpoint.rstrip("/")


class AlpacaLiveClient:
    def __init__(self, key: str, secret: str, endpoint: str):
        self.key = key
        self.secret = secret
        self.endpoint = endpoint
        self.data_base = "https://data.alpaca.markets"

    def _request(self, method: str, url: str, data: dict = None):
        headers = {
            "APCA-API-KEY-ID": self.key,
            "APCA-API-SECRET-KEY": self.secret,
            "Content-Type": "application/json",
            "Accept": "application/json",
        }
        body = json.dumps(data).encode("utf-8") if data else None
        req = urllib.request.Request(url, data=body, headers=headers, method=method)
        try:
            with urllib.request.urlopen(req, timeout=10) as resp:
                if resp.status == 204:
                    return {}
                return json.loads(resp.read().decode("utf-8"))
        except urllib.error.HTTPError as e:
            err_body = e.read().decode("utf-8")
            if e.code == 422:
                # Terminal or unprocessable entity
                return {"_error_code": 422, "_message": err_body}
            raise RuntimeError(f"HTTP {e.code} error on {url}: {err_body}")

    def get_latest_quote(self, symbol: str) -> dict:
        if "/" in symbol:
            url = f"{self.data_base}/v1beta3/crypto/us/latest/quotes?symbols={symbol}"
        else:
            url = f"{self.data_base}/v2/stocks/quotes/latest?symbols={symbol}&feed=iex"
        data = self._request("GET", url)
        quotes = data.get("quotes", {})
        if symbol not in quotes:
            raise KeyError(f"Quote not found for {symbol}")
        return quotes[symbol]

    def submit_order(self, symbol: str, qty: float, side: str, order_type: str, limit_price: float = None, tif: str = "gtc"):
        payload = {
            "symbol": symbol,
            "qty": str(qty),
            "side": side,
            "type": order_type,
            "time_in_force": tif,
            "client_order_id": f"hunt-{side}-{uuid.uuid4()}",
        }
        if limit_price is not None:
            payload["limit_price"] = f"{limit_price:.2f}" if limit_price >= 1.0 else f"{limit_price:.4f}"
        return self._request("POST", f"{self.endpoint}/orders", payload)

    def cancel_order(self, order_id: str):
        return self._request("DELETE", f"{self.endpoint}/orders/{order_id}")

    def get_order(self, order_id: str):
        return self._request("GET", f"{self.endpoint}/orders/{order_id}")

    def get_open_orders(self, symbol: str = None):
        url = f"{self.endpoint}/orders?status=open"
        if symbol:
            url += f"&symbols={symbol}"
        return self._request("GET", url)

    def get_closed_orders(self, symbol: str = None, limit: int = 100):
        url = f"{self.endpoint}/orders?status=closed&limit={limit}&direction=asc"
        if symbol:
            url += f"&symbols={symbol}"
        return self._request("GET", url)

    def get_account(self) -> dict:
        return self._request("GET", f"{self.endpoint}/account")

    def get_positions(self, symbol: str = None):
        if symbol:
            try:
                # Need URL encoding for slash in symbol like SOL/USD
                encoded_sym = symbol.replace("/", "%2F")
                return self._request("GET", f"{self.endpoint}/positions/{encoded_sym}")
            except Exception:
                return None
        return self._request("GET", f"{self.endpoint}/positions")


class AdaptiveKalmanLive:
    def __init__(self, q_base: float, r_base: float, spread_weight: float, adapt_lambda: float, min_margin: float, max_margin: float):
        self.q_base = q_base
        self.r_base = r_base
        self.spread_weight = spread_weight
        self.adapt_lambda = adapt_lambda
        self.min_margin = min_margin
        self.max_margin = max_margin
        self.x_hat = 0.0
        self.p = 1.0
        self.q = q_base
        self.innovation_var = r_base
        self.sample_count = 0

    def update(self, bid: float, ask: float):
        mid = 0.5 * (bid + ask)
        spread = ask - bid

        if self.sample_count == 0 or self.x_hat <= 0:
            self.x_hat = mid
            self.p = self.r_base * mid * mid
            self.q = self.q_base * mid * mid
            self.innovation_var = self.p
            self.sample_count = 1
            return "none", 0.0, self.x_hat

        x_prior = self.x_hat
        p_prior = self.p + self.q

        half_spread = 0.5 * spread
        r_t = (self.r_base * mid * mid) + self.spread_weight * (half_spread * half_spread)
        nu = mid - x_prior
        margin = (x_prior - mid) / x_prior

        # Circuit breaker check
        if margin > self.max_margin:
            # Gated update
            k_gated = p_prior / (p_prior + r_t * 20.0)
            self.x_hat = x_prior + k_gated * nu
            self.p = (1.0 - k_gated) * p_prior
            self.sample_count += 1
            return "circuit_breaker", margin, x_prior

        s_t = p_prior + r_t
        k_t = min(0.99, max(0.01, p_prior / s_t if s_t > 0 else 0.5))
        self.x_hat = x_prior + k_t * nu
        self.p = (1.0 - k_t) * p_prior

        # Adaptation
        self.innovation_var = (1.0 - self.adapt_lambda) * self.innovation_var + self.adapt_lambda * (nu * nu)
        excess = self.innovation_var - r_t
        min_q = self.q_base * mid * mid
        self.q = max(min_q, self.adapt_lambda * excess)
        self.sample_count += 1

        if margin >= self.min_margin:
            return "buy_anomaly", margin, x_prior
        return "none", margin, x_prior


def calculate_shares_amount(
    margin: float,
    min_m: float,
    max_m: float,
    level: float,
    max_shares_param: float,
    equity: float = 100000.0,
    available_cash: float = 100000.0,
    buy_price: float = 75.0,
) -> float:
    """
    Dynamic position sizing:
    - If max_shares_param <= 1.0: Proportion of total portfolio equity (e.g., 0.20 = 20%, 1.00 = 100%).
    - If max_shares_param > 1.0: Absolute unit shares mode (legacy backwards compatibility).
    - If available cash is less than target capital, allocates all remaining cash ("sense asímptota").
    - Supports fractional quantities for crypto (4 decimals).
    """
    if margin < min_m or max_shares_param <= 0.0 or buy_price <= 0.0:
        return 0.0
    if margin > max_m:
        return 0.0

    denom = max_m - min_m
    if denom <= 0:
        return 0.0

    x = max(0.0, min(1.0, (margin - min_m) / denom))
    factor = 1.0 - ((1.0 - (x ** level)) ** (1.0 / level)) if level > 0 else x

    if max_shares_param <= 1.0:
        # Proportion of total portfolio equity mode
        target_capital = factor * (equity * max_shares_param)
        allocated_capital = min(target_capital, max(0.0, available_cash))

        # Enforce minimum order threshold ($10.0 on Alpaca)
        if allocated_capital < 10.0:
            return 0.0

        # Fractional shares for crypto (up to 4 decimals)
        qty = round(allocated_capital / buy_price, 4)
        return qty
    else:
        # Legacy absolute unit shares mode
        shares = int(factor * max_shares_param)
        return max(1.0, float(shares)) if x > 0 and shares == 0 else min(max_shares_param, float(shares))


def main():
    params, key, secret, endpoint = load_config()
    symbol = params["symbol"]
    client = AlpacaLiveClient(key, secret, endpoint)

    k_cfg = params["kalman"]
    kalman = AdaptiveKalmanLive(
        q_base=k_cfg["q_base"],
        r_base=k_cfg["r_base"],
        spread_weight=k_cfg["spread_weight"],
        adapt_lambda=k_cfg["adapt_lambda"],
        min_margin=params["min_margin"],
        max_margin=params["max_margin"],
    )

    cycle_secs = params.get("cycle_seconds", 30)
    hidden_mm_timeout = params.get("hidden_mm_timeout_seconds", 20)
    tif = "gtc" if "/" in symbol else "day"

    print("=" * 70)
    print(f"🚀 ANOMALIES TRADING ENGINE v2.0 (LIVE BOT)")
    print(f"   Symbol: {symbol} | Account: Alpaca Paper Trading")
    print(f"   Cycle: {cycle_secs}s | Sizing Level: {params['level']} | Max Shares/Cap: {params['max_shares']}")
    print(f"   Min Margin: {params['min_margin']*100:.3f}% | Max Margin (Circuit Breaker): {params['max_margin']*100:.2f}%")
    print("=" * 70)

    store = LocalPersistenceStore(DATA_DIR)

    # 1. Recover existing hunts from persistent store (last 50)
    trades_history = store.load_hunts(max_count=50)

    # 2. If empty, reconcile from Alpaca closed orders history
    if not trades_history:
        try:
            closed_orders = client.get_closed_orders(symbol=symbol, limit=100)
            trades_history = store.reconcile_alpaca_orders(closed_orders, symbol=symbol)
            if trades_history:
                print(f"📦 [PERSISTENCE] Successfully backfilled {len(trades_history)} completed hunts from Alpaca API history.")
                store.save_hunts(trades_history, max_count=50)
        except Exception as e:
            print(f"[WARN] Reconciling closed orders from Alpaca: {e}")

    # 3. Compute and persist initial metrics
    metrics = compute_performance_metrics(trades_history)
    store.save_metrics(metrics)
    total_realized_pnl = float(metrics.get("total_pnl", 0.0))

    print(f"📦 [PERSISTENCE] Active: {len(trades_history)} hunts loaded | Total Realized PnL: ${total_realized_pnl:+.2f} | Win Rate: {metrics.get('win_rate', 0.0)}% | Sharpe: {metrics.get('sharpe_ratio', 0.0)}")

    while RUNNING:
        cycle_start = time.time()
        now_str = datetime.now(timezone.utc).isoformat()

        try:
            quote = client.get_latest_quote(symbol)
            bid = float(quote["bp"])
            ask = float(quote["ap"])
            mid = 0.5 * (bid + ask)
            spread = ask - bid

            signal, margin, fair_price = kalman.update(bid, ask)

            print(f"[{now_str[:19]}] Bid: ${bid:.2f} | Ask: ${ask:.2f} (Spread: ${spread:.2f}) | Kalman: ${fair_price:.2f} | Margin: {margin*100:+.4f}%")

            if signal == "circuit_breaker":
                print(f"⚠️ [CIRCUIT BREAKER] Deviation {margin*100:.3f}% > {params['max_margin']*100:.2f}%. Trading HALTED.")
            elif signal == "buy_anomaly":
                # HiddenMM pricing
                tick = 0.01
                spread_ticks = round((ask - bid) / tick)
                buy_price = round(bid + tick, 2) if spread_ticks >= 2 else round(bid, 2)

                # Fetch real-time account equity and cash
                equity = 100000.0
                cash = 100000.0
                try:
                    acc = client.get_account()
                    equity = float(acc.get("equity", 100000.0))
                    cash = float(acc.get("cash", equity))
                except Exception as e:
                    print(f"   [Warn] Fetching account balance: {e}")

                max_shares_val = float(params["max_shares"])
                shares = calculate_shares_amount(
                    margin,
                    params["min_margin"],
                    params["max_margin"],
                    params["level"],
                    max_shares_val,
                    equity=equity,
                    available_cash=cash,
                    buy_price=buy_price,
                )

                print(f"🎯 [ANOMALY DETECTED] Margin {margin*100:.4f}% >= {params['min_margin']*100:.3f}% -> Sizing {shares} units (${shares * buy_price:.2f} | Equity: ${equity:,.2f}, Cash: ${cash:,.2f})")

                if shares > 0:
                    print(f"   [HiddenMM] Placing limit BUY {shares} @ ${buy_price:.2f} (${shares * buy_price:.2f}, maker queue priority)...")
                    buy_ord = client.submit_order(symbol, shares, "buy", "limit", buy_price, tif=tif)
                    ord_id = buy_ord.get("id")

                    # Wait up to 20s
                    start_wait = time.time()
                    filled_qty = 0
                    avg_price = buy_price

                    while time.time() - start_wait < hidden_mm_timeout and RUNNING:
                        time.sleep(1.5)
                        try:
                            chk = client.get_order(ord_id)
                            status = chk.get("status")
                            if status in ["filled", "partially_filled"]:
                                filled_qty = float(chk.get("filled_qty", 0))
                                avg_price = float(chk.get("filled_avg_price", buy_price))
                            if status in ["filled", "canceled", "rejected"]:
                                break
                        except Exception as e:
                            print(f"   [Warn] Polling status: {e}")

                    # 20s reached: cancel remaining
                    if ord_id:
                        client.cancel_order(ord_id)
                        time.sleep(1.0)
                        try:
                            chk = client.get_order(ord_id)
                            filled_qty = float(chk.get("filled_qty", filled_qty))
                            if chk.get("filled_avg_price"):
                                avg_price = float(chk["filled_avg_price"])
                        except Exception:
                            pass

                    if filled_qty > 0:
                        print(f"   ✅ [FILLED] {filled_qty} units @ ${avg_price:.2f}. Submitting Exit Limit SELL...")
                        exit_price = max(round(avg_price + tick, 2), round(fair_price, 2))
                        sell_ord = client.submit_order(symbol, filled_qty, "sell", "limit", exit_price, tif=tif)
                        sell_id = sell_ord.get("id")

                        trade_rec = {
                            "id": str(uuid.uuid4())[:8],
                            "symbol": symbol,
                            "buy_order_id": ord_id,
                            "sell_order_id": sell_id,
                            "qty": filled_qty,
                            "buy_price": avg_price,
                            "sell_price": exit_price,
                            "pnl": round((exit_price - avg_price) * filled_qty, 2),
                            "pnl_pct": round((exit_price - avg_price) / avg_price, 4),
                            "buy_timestamp": now_str,
                            "sell_timestamp": None,
                            "status": "completed",
                        }
                        total_realized_pnl += trade_rec["pnl"]
                        trades_history.append(trade_rec)
                        trades_history = trades_history[-50:]

                        # Recompute and persist metrics and hunts
                        metrics = compute_performance_metrics(trades_history)
                        store.save_hunts(trades_history, max_count=50)
                        store.save_metrics(metrics)

                        print(f"   💰 [EXIT PLACED] SELL {filled_qty} @ ${exit_price:.2f} (Target PnL: +${trade_rec['pnl']:.2f} | Total: ${total_realized_pnl:+.2f})")
                    else:
                        print("   ⏱️ [UNFILLED] 20s timeout expired. Remainder canceled. 0 shares bought.")

            # Save persistent snapshot to data/telemetry.json
            telemetry_snapshot = {
                "timestamp": now_str,
                "symbol": symbol,
                "best_bid": bid,
                "best_ask": ask,
                "market_mid": mid,
                "fair_price": fair_price,
                "undervaluation_margin": margin,
                "min_margin": params["min_margin"],
                "max_margin": params["max_margin"],
                "circuit_breaker_active": signal == "circuit_breaker",
                "active_hunts_count": 0,
                "completed_trades_count": len(trades_history),
                "total_realized_pnl": round(total_realized_pnl, 2),
            }
            store.save_telemetry_snapshot(telemetry_snapshot, trades_history, metrics)

        except Exception as e:
            print(f"[{now_str[:19]}] [Error in cycle]: {e}")

        elapsed = time.time() - cycle_start
        if elapsed < cycle_secs and RUNNING:
            time.sleep(cycle_secs - elapsed)


if __name__ == "__main__":
    main()
