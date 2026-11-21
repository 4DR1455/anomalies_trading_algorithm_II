#!/usr/bin/env python3
"""
Anomalies Trading Algorithm v2.0 - Live Bot Runner
Connects directly to Alpaca API (Paper Trading) using params.json and .env.
Executes the Adaptive 1D Kalman Filter, HiddenMM entry, 20s cancel, fee-aware exit limit sells,
12-hour stale order liquidation, and continuous broker position reconciliation.
Writes real-time snapshots (including live Alpaca positions & reconciliation status) to data/telemetry.json.
"""

from decimal import Decimal, ROUND_DOWN, ROUND_UP
import json
import os
import signal
import sys
import time
import urllib.request
import urllib.error
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Dict, List, Optional, Tuple
import uuid

# Ensure root paths
ROOT_DIR = Path(__file__).resolve().parent
DATA_DIR = ROOT_DIR / "data"
DATA_DIR.mkdir(exist_ok=True)

sys.path.insert(0, str(ROOT_DIR))
sys.path.insert(0, str(ROOT_DIR / "dashboard"))

from persistence import (
    ACTIVE_HUNT_STATUSES,
    LocalPersistenceStore,
    trim_hunts_preserving_active,
)
from metrics import CLOSED_TRADE_STATUSES, compute_performance_metrics

RUNNING = True

TERMINAL_ORDER_STATUSES = {
    "filled",
    "canceled",
    "expired",
    "rejected",
    "done_for_day",
    "replaced",
}
OPEN_ORDER_STATUSES = {
    "new",
    "accepted",
    "pending_new",
    "partially_filled",
    "accepted_for_bidding",
    "pending_replace",
    "pending_cancel",
}

# Alpaca Crypto Tier 1 fee schedule (buy fees deducted in base asset; sell fees in USD)
CRYPTO_MAKER_FEE_RATE = 0.0015
CRYPTO_TAKER_FEE_RATE = 0.0025
CRYPTO_DUST_QTY = 0.001
MIN_PROTECT_NOTIONAL_USD = 1.00


def handle_sigint(sig, frame):
    global RUNNING
    print("\n[INFO] Shutdown signal received. Exiting cleanly...")
    RUNNING = False


signal.signal(signal.SIGINT, handle_sigint)
signal.signal(signal.SIGTERM, handle_sigint)


def floor_qty(qty: float, decimals: int = 9) -> float:
    """Floors a quantity down to `decimals` decimal places using Decimal to never exceed available balance."""
    if qty is None or qty <= 0:
        return 0.0
    quant = Decimal("1").scaleb(-decimals)
    d = Decimal(str(qty)).quantize(quant, rounding=ROUND_DOWN)
    return float(d)


def format_order_qty(qty: float, is_crypto: bool = True) -> str:
    """Formats order quantity safely without scientific notation and strictly floored."""
    if qty <= 0:
        return "0"
    decimals = 9 if is_crypto else 4
    quant = Decimal("1").scaleb(-decimals)
    d = Decimal(str(qty)).quantize(quant, rounding=ROUND_DOWN)
    s = format(d, "f")
    if "." in s:
        s = s.rstrip("0").rstrip(".")
    return s or "0"


def ceil_usd_cents(amount: float) -> float:
    """Rounds USD fee amount up to the nearest cent (matching Alpaca sell-side CFEE)."""
    if amount <= 0:
        return 0.0
    return float(Decimal(str(amount)).quantize(Decimal("0.01"), rounding=ROUND_UP))


def parse_iso_timestamp(ts_str: Optional[str]) -> Optional[datetime]:
    """Parses ISO-8601 timestamps (with Z or offset) into timezone-aware UTC datetimes."""
    if not ts_str:
        return None
    try:
        clean = str(ts_str).replace("Z", "+00:00")
        dt = datetime.fromisoformat(clean)
        if dt.tzinfo is None:
            dt = dt.replace(tzinfo=timezone.utc)
        return dt.astimezone(timezone.utc)
    except Exception:
        return None


def compute_net_trade_pnl(
    buy_price: float,
    sell_price: float,
    buy_qty: float,
    sell_qty: float,
    is_crypto: bool = True,
    buy_fee_rate: float = CRYPTO_MAKER_FEE_RATE,
    sell_fee_rate: float = CRYPTO_MAKER_FEE_RATE,
) -> Tuple[float, float, float, float]:
    """
    Computes exact fee-inclusive realized P&L in USD.
    On Alpaca crypto, buy fees are deducted in base asset (so sell_qty < buy_qty naturally reflects buy fees)
    and sell fees are deducted in USD (ceil to $0.01).
    Returns: (net_pnl_usd, net_pnl_pct, gross_pnl_usd, total_fees_usd)
    """
    b_qty = max(0.0, float(buy_qty or 0.0))
    s_qty = max(0.0, float(sell_qty or 0.0))
    if b_qty <= 0.0:
        b_qty = s_qty
    cost_basis = buy_price * b_qty
    gross_proceeds = sell_price * s_qty

    if not is_crypto:
        gross_pnl = (sell_price - buy_price) * s_qty
        pnl_pct = round(gross_pnl / cost_basis, 4) if cost_basis > 0 else 0.0
        return round(gross_pnl, 2), pnl_pct, round(gross_pnl, 2), 0.0

    sell_fee_usd = ceil_usd_cents(gross_proceeds * sell_fee_rate)
    if b_qty > s_qty + 1e-7:
        # Buy fee was already deducted in base asset (b_qty - s_qty)
        buy_fee_usd = (b_qty - s_qty) * buy_price
        gross_pnl = (sell_price - buy_price) * b_qty
        net_pnl = (gross_proceeds - sell_fee_usd) - cost_basis
    else:
        # Legacy / cushion trade where full gross buy_qty was sold
        buy_fee_usd = cost_basis * buy_fee_rate
        gross_pnl = (sell_price - buy_price) * s_qty
        net_pnl = (gross_proceeds - sell_fee_usd) - cost_basis - buy_fee_usd

    total_fees_usd = buy_fee_usd + sell_fee_usd
    pnl_pct = round(net_pnl / cost_basis, 4) if cost_basis > 0 else 0.0
    return round(net_pnl, 2), pnl_pct, round(gross_pnl, 2), round(total_fees_usd, 2)


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


class AlpacaAPIError(RuntimeError):
    """Structured Alpaca HTTP error preserving status code and parsed JSON error payload."""

    def __init__(self, status_code: int, url: str, body: str):
        self.status_code = status_code
        self.url = url
        self.body = body
        try:
            parsed = json.loads(body) if body else {}
            self.error_json = parsed if isinstance(parsed, dict) else {}
        except Exception:
            self.error_json = {}
        super().__init__(f"HTTP {status_code} error on {url}: {body}")


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
            raise AlpacaAPIError(e.code, url, err_body)

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

    def submit_order(
        self,
        symbol: str,
        qty: Any,
        side: str,
        order_type: str,
        limit_price: float = None,
        tif: str = "gtc",
    ):
        is_crypto = "/" in symbol
        qty_str = qty if isinstance(qty, str) else format_order_qty(float(qty), is_crypto=is_crypto)
        payload = {
            "symbol": symbol,
            "qty": qty_str,
            "side": side,
            "type": order_type,
            "time_in_force": tif,
            "client_order_id": f"hunt-{side}-{uuid.uuid4()}",
        }
        if limit_price is not None:
            payload["limit_price"] = f"{limit_price:.2f}" if limit_price >= 1.0 else f"{limit_price:.4f}"
        return self._request("POST", f"{self.endpoint}/orders", payload)

    def cancel_order(self, order_id: str):
        try:
            return self._request("DELETE", f"{self.endpoint}/orders/{order_id}")
        except AlpacaAPIError as e:
            if e.status_code in (404, 422):
                # Order already terminal (filled/canceled)
                return {"_error_code": e.status_code, "_message": e.body}
            raise

    def get_order(self, order_id: str):
        return self._request("GET", f"{self.endpoint}/orders/{order_id}")

    def get_open_orders(self, symbol: str = None):
        url = f"{self.endpoint}/orders?status=open"
        if symbol:
            url += f"&symbols={symbol}"
        return self._request("GET", url)

    def get_closed_orders(self, symbol: str = None, limit: int = 500, after: str = None):
        url = f"{self.endpoint}/orders?status=closed&limit={limit}&direction=asc"
        if symbol:
            url += f"&symbols={symbol}"
        if after:
            url += f"&after={after}"
        return self._request("GET", url)

    def get_account(self) -> dict:
        return self._request("GET", f"{self.endpoint}/account")

    def get_positions(self, symbol: str = None):
        if symbol:
            # Alpaca /v2/positions/{symbol} expects crypto without slash (e.g., SOLUSD, not SOL%2FUSD)
            pos_sym = symbol.replace("/", "")
            try:
                return self._request("GET", f"{self.endpoint}/positions/{pos_sym}")
            except AlpacaAPIError as e:
                if e.status_code == 404:
                    return None
                raise
        return self._request("GET", f"{self.endpoint}/positions")

    def close_position(self, symbol: str):
        pos_sym = symbol.replace("/", "")
        return self._request("DELETE", f"{self.endpoint}/positions/{pos_sym}")


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


def wait_for_terminal_order(
    client: Any,
    order_id: str,
    max_wait_secs: float = 8.0,
    poll_interval: float = 0.5,
    fallback_order: Optional[Dict[str, Any]] = None,
) -> Dict[str, Any]:
    """
    Polls client.get_order(order_id) until the order reaches a strictly terminal state
    (filled, canceled, expired, rejected, done_for_day, replaced) or max_wait_secs elapses.
    Prevents cancel-vs-fill race conditions where late fills land after cancel_order().
    """
    latest = dict(fallback_order) if fallback_order else {}
    deadline = time.time() + max_wait_secs

    while True:
        try:
            chk = client.get_order(order_id)
            if isinstance(chk, dict) and chk.get("id"):
                latest = chk
                if latest.get("status") in TERMINAL_ORDER_STATUSES:
                    return latest
        except Exception as exc:
            print(f"   [Warn] Polling terminal status for order {order_id}: {exc}")

        if time.time() >= deadline:
            return latest
        if poll_interval > 0:
            time.sleep(poll_interval)


def compute_safe_exit_sell_qty(
    filled_qty: float,
    symbol: str,
    client: Any,
    avail_before: Optional[float] = None,
    buy_fee_rate: float = CRYPTO_MAKER_FEE_RATE,
    settle_retries: int = 3,
    settle_sleep: float = 0.4,
) -> Tuple[float, float]:
    """
    Determines the exact fee-adjusted, balance-verified quantity to submit on the exit Limit SELL.
    For crypto pairs ('/' in symbol), Alpaca immediately deducts buy fees (0.15% maker / 0.25% taker)
    in the base asset (SOL). Using live position `qty_available` and pre-buy delta guarantees we NEVER
    request more than the account actually holds, preventing HTTP 403 insufficient balance rejections.

    Returns: (safe_sell_qty, live_qty_available)
    """
    is_crypto = "/" in symbol
    decimals = 9 if is_crypto else 4

    if filled_qty <= 0:
        return 0.0, 0.0

    # Conservative fee-adjusted estimate
    if is_crypto:
        est_net = floor_qty(
            float(Decimal(str(filled_qty)) * (Decimal("1") - Decimal(str(buy_fee_rate)))),
            decimals=decimals,
        )
    else:
        est_net = floor_qty(filled_qty, decimals=decimals)

    live_avail = None
    for attempt in range(max(1, settle_retries)):
        try:
            pos = client.get_positions(symbol)
            if pos and float(pos.get("qty_available") or 0.0) > 0:
                live_avail = floor_qty(float(pos.get("qty_available") or 0.0), decimals=decimals)
                # If we know pre-buy available balance, wait briefly if position hasn't reflected the fill yet
                if avail_before is None or live_avail > avail_before + 1e-8 or attempt == settle_retries - 1:
                    break
            elif pos is None and attempt < settle_retries - 1:
                pass
            else:
                live_avail = 0.0
                break
        except Exception as exc:
            print(f"   [Warn] Fetching live position qty_available for {symbol} (attempt {attempt + 1}): {exc}")

        if attempt < settle_retries - 1 and settle_sleep > 0:
            time.sleep(settle_sleep)

    if live_avail is None:
        # Position endpoint unreachable; fall back strictly to conservative fee-adjusted net qty
        return est_net, est_net

    if not is_crypto:
        return floor_qty(min(filled_qty, live_avail), decimals=decimals), live_avail

    if avail_before is not None:
        delta_d = Decimal(str(live_avail)) - Decimal(str(avail_before))
        delta = floor_qty(float(delta_d), decimals=decimals)
        if 0.0 < delta <= filled_qty:
            # Exact net delta credited by Alpaca (covers maker 0.15%, taker 0.25%, and per-fill ceil rounding)
            safe_qty = floor_qty(min(delta, live_avail), decimals=decimals)
            return safe_qty, live_avail

    safe_qty = floor_qty(min(est_net, live_avail), decimals=decimals)
    return safe_qty, live_avail


def submit_exit_sell_with_retry(
    client: Any,
    symbol: str,
    sell_qty: float,
    exit_price: float,
    tif: str = "gtc",
    max_retries: int = 2,
) -> Tuple[Optional[Dict[str, Any]], float]:
    """
    Submits an exit Limit SELL order with automatic recovery on HTTP 403/422 insufficient balance.
    Never raises an uncaught exception out of the cycle; returns (order_dict_or_None, submitted_qty).
    """
    is_crypto = "/" in symbol
    decimals = 9 if is_crypto else 4
    current_qty = floor_qty(sell_qty, decimals=decimals)

    for attempt in range(max_retries + 1):
        if current_qty <= 0:
            return None, 0.0

        qty_str = format_order_qty(current_qty, is_crypto=is_crypto)
        try:
            ord_resp = client.submit_order(symbol, qty_str, "sell", "limit", exit_price, tif=tif)
            if isinstance(ord_resp, dict) and ord_resp.get("id") and not ord_resp.get("_error_code"):
                return ord_resp, current_qty
            print(f"   ⚠️ [EXIT REJECTED] Unexpected order response on attempt {attempt + 1}: {ord_resp}")
        except AlpacaAPIError as e:
            print(f"   ⚠️ [EXIT HTTP {e.status_code}] Attempt {attempt + 1} failed for {qty_str} {symbol}: {e.body}")
            if e.status_code in (403, 422) and attempt < max_retries:
                # Reconcile against error payload 'available' AND fresh position query
                candidates = []
                if e.error_json.get("available") is not None:
                    try:
                        candidates.append(floor_qty(float(e.error_json["available"]), decimals=decimals))
                    except Exception:
                        pass
                try:
                    pos = client.get_positions(symbol)
                    if pos is not None:
                        candidates.append(floor_qty(float(pos.get("qty_available") or 0.0), decimals=decimals))
                except Exception as pe:
                    print(f"   [Warn] Refreshing position after HTTP {e.status_code}: {pe}")

                if candidates:
                    fresh_avail = min(candidates)
                    # If same qty was just rejected, step down by 1e-9 on retry to beat rounding boundary
                    if fresh_avail >= current_qty and is_crypto:
                        fresh_avail = floor_qty(fresh_avail - 1e-9, decimals=decimals)
                    current_qty = floor_qty(min(current_qty, fresh_avail), decimals=decimals)
                    if current_qty > 0:
                        print(f"   🔄 [EXIT RETRY] Adjusting exit SELL quantity to live available {format_order_qty(current_qty, is_crypto)}...")
                        continue
                return None, 0.0
            else:
                return None, 0.0
        except Exception as exc:
            print(f"   ⚠️ [EXIT ERROR] Attempt {attempt + 1} failed for {qty_str} {symbol}: {exc}")
            if attempt >= max_retries:
                return None, 0.0

    return None, 0.0


def build_broker_telemetry(
    account: Optional[Dict[str, Any]],
    position: Optional[Dict[str, Any]],
    open_orders: List[Dict[str, Any]],
    trades_history: List[Dict[str, Any]],
) -> Dict[str, Any]:
    """
    Constructs real-time Alpaca account, position, open-order, and share-reconciliation telemetry
    so the read-only dashboard always mirrors every single share held at the broker.
    """
    broker_account = {
        "equity": round(float(account.get("equity") or 0.0), 2) if account else None,
        "cash": round(float(account.get("cash") or 0.0), 2) if account else None,
        "buying_power": round(float(account.get("buying_power") or 0.0), 2) if account else None,
    }

    broker_qty = floor_qty(float(position.get("qty") or 0.0), 9) if position else 0.0
    broker_avail = floor_qty(float(position.get("qty_available") or 0.0), 9) if position else 0.0
    locked_qty = floor_qty(max(0.0, float(Decimal(str(broker_qty)) - Decimal(str(broker_avail)))), 9)

    broker_position = {
        "qty": broker_qty,
        "qty_available": broker_avail,
        "locked_in_orders": locked_qty,
        "avg_entry_price": round(float(position.get("avg_entry_price") or 0.0), 6) if position else 0.0,
        "market_value": round(float(position.get("market_value") or 0.0), 2) if position else 0.0,
        "unrealized_pl": round(float(position.get("unrealized_pl") or 0.0), 2) if position else 0.0,
        "current_price": round(float(position.get("current_price") or 0.0), 4) if position else 0.0,
    }

    open_sell_orders_summary = []
    open_orders_locked_qty = Decimal("0")
    for o in open_orders or []:
        o_qty = Decimal(str(o.get("qty") or "0"))
        o_filled = Decimal(str(o.get("filled_qty") or "0"))
        o_rem = max(Decimal("0"), o_qty - o_filled)
        if o.get("side") == "sell":
            open_orders_locked_qty += o_rem
        open_sell_orders_summary.append({
            "id": o.get("id"),
            "side": o.get("side"),
            "qty": float(o_qty),
            "filled_qty": float(o_filled),
            "remaining_qty": float(o_rem),
            "limit_price": float(o.get("limit_price") or 0.0) if o.get("limit_price") else None,
            "status": o.get("status"),
            "created_at": o.get("created_at"),
        })

    tracked_active_qty = Decimal("0")
    for h in trades_history:
        if h.get("status") in ACTIVE_HUNT_STATUSES:
            h_qty = Decimal(str(h.get("qty") or "0"))
            h_filled = Decimal(str(h.get("filled_exit_qty") or "0"))
            tracked_active_qty += max(Decimal("0"), h_qty - h_filled)

    tracked_active_f = floor_qty(float(tracked_active_qty), 9)
    open_locked_f = floor_qty(float(open_orders_locked_qty), 9)
    untracked_qty = round(broker_qty - tracked_active_f, 9)
    is_synced = (abs(untracked_qty) <= 0.01) and (broker_avail <= 0.01)

    return {
        "broker_account": broker_account,
        "broker_position": broker_position,
        "broker_open_orders": open_sell_orders_summary,
        "reconciliation": {
            "broker_qty": broker_qty,
            "broker_qty_available": broker_avail,
            "open_orders_locked_qty": open_locked_f,
            "tracked_active_qty": tracked_active_f,
            "untracked_qty": untracked_qty,
            "is_synced": is_synced,
        },
    }


def reconcile_and_maintain_positions(
    client: Any,
    symbol: str,
    trades_history: List[Dict[str, Any]],
    params: Dict[str, Any],
    fair_price: float = 0.0,
    bid: float = 0.0,
    now_dt: Optional[datetime] = None,
    tif: str = "gtc",
    store: Optional[Any] = None,
) -> Tuple[bool, Dict[str, Any]]:
    """
    Per-cycle Broker Reconciliation & Order Hygiene Engine:
    1. Retries any `exit_submit_pending` hunts that failed exit placement.
    2. Monitors `open_exit_pending` and `exit_partially_filled` hunts:
       - Finalizes filled exits with fee-inclusive net P&L.
       - Tracks partial exit fills.
       - Automatically re-raises protective sells if an exit order was canceled/rejected/expired.
       - Enforces `stale_order_max_age_seconds` (12h default) by canceling and liquidating at market.
    3. Adopts untracked open Alpaca sell orders and auto-protects any unencumbered orphaned `qty_available`.
    4. Returns (history_changed, broker_telemetry_dict).
    """
    is_crypto = "/" in symbol
    decimals = 9 if is_crypto else 4
    tick = 0.01
    stale_max_age = float(params.get("stale_order_max_age_seconds", 43200))
    now_dt = now_dt or datetime.now(timezone.utc)
    now_str = now_dt.isoformat()
    market_tif = "gtc" if is_crypto else "day"

    history_changed = False

    # Initial broker fetch
    account = None
    position = None
    open_orders: List[Dict[str, Any]] = []
    position_fetch_ok = False
    open_orders_fetch_ok = False

    try:
        account = client.get_account()
    except Exception as exc:
        print(f"   [Warn] Reconciliation get_account failed: {exc}")

    try:
        position = client.get_positions(symbol)
        position_fetch_ok = True
    except Exception as exc:
        print(f"   [Warn] Reconciliation get_positions({symbol}) failed: {exc}")

    try:
        open_orders = client.get_open_orders(symbol) or []
        open_orders_fetch_ok = True
    except Exception as exc:
        print(f"   [Warn] Reconciliation get_open_orders({symbol}) failed: {exc}")

    avail_pool = floor_qty(float(position.get("qty_available") or 0.0), decimals) if position else 0.0

    # --- STEP A: Retry hunts stuck in `exit_submit_pending` (persisted before sell succeeded) ---
    for hunt in trades_history:
        if hunt.get("status") != "exit_submit_pending":
            continue
        if not position_fetch_ok:
            continue

        desired_qty = floor_qty(float(hunt.get("qty") or 0.0), decimals)
        sell_qty = floor_qty(min(desired_qty, avail_pool), decimals)
        ref_price = bid or float(position.get("current_price") or 0.0) if position else bid

        if sell_qty <= CRYPTO_DUST_QTY or (ref_price > 0 and sell_qty * ref_price < MIN_PROTECT_NOTIONAL_USD):
            if avail_pool <= CRYPTO_DUST_QTY and not any(o.get("side") == "sell" for o in open_orders):
                print(f"   ⚠️ [EXIT RECOVERY] Hunt {hunt['id']} had status=exit_submit_pending but 0 available {symbol} remains. Closing record.")
                hunt["status"] = "completed"
                hunt["sell_timestamp"] = now_str
                hunt["pnl"] = 0.0
                hunt["pnl_pct"] = 0.0
                history_changed = True
            continue

        exit_px = float(hunt.get("sell_price") or 0.0)
        if exit_px <= 0:
            buy_px = float(hunt.get("buy_price") or ref_price)
            exit_px = max(round(buy_px + tick, 2), round(fair_price, 2)) if fair_price > 0 else round(buy_px + tick, 2)

        print(f"   🔄 [EXIT RECOVERY] Submitting pending Limit SELL for Hunt {hunt['id']}: {format_order_qty(sell_qty, is_crypto)} @ ${exit_px:.2f}...")
        sell_ord, submitted_qty = submit_exit_sell_with_retry(client, symbol, sell_qty, exit_px, tif=tif)
        if sell_ord and sell_ord.get("id"):
            hunt["sell_order_id"] = sell_ord["id"]
            hunt["qty"] = submitted_qty
            hunt["sell_price"] = exit_px
            if sell_ord.get("status") == "filled":
                fill_px = float(sell_ord.get("filled_avg_price") or exit_px)
                fill_qty = float(sell_ord.get("filled_qty") or submitted_qty)
                net_pnl, pnl_pct, gross_pnl, fees_usd = compute_net_trade_pnl(
                    float(hunt.get("buy_price") or 0.0),
                    fill_px,
                    float(hunt.get("buy_qty") or fill_qty),
                    fill_qty,
                    is_crypto=is_crypto,
                )
                hunt["sell_price"] = fill_px
                hunt["qty"] = fill_qty
                hunt["sell_timestamp"] = sell_ord.get("filled_at") or now_str
                hunt["pnl"] = net_pnl
                hunt["pnl_pct"] = pnl_pct
                hunt["gross_pnl"] = gross_pnl
                hunt["fees_usd"] = fees_usd
                hunt["status"] = "completed"
            else:
                hunt["status"] = "open_exit_pending"
            avail_pool = floor_qty(max(0.0, avail_pool - submitted_qty), decimals)
            history_changed = True

    # --- STEP B: Monitor `open_exit_pending` / `exit_partially_filled` + Stale 12h Liquidation ---
    for hunt in trades_history:
        if hunt.get("status") not in ("open_exit_pending", "exit_partially_filled"):
            continue

        sell_oid = hunt.get("sell_order_id")
        if not sell_oid:
            # Guard against legacy broken records with sell_order_id=None
            hunt["status"] = "exit_submit_pending"
            history_changed = True
            continue

        try:
            chk_sell = client.get_order(sell_oid)
        except Exception as ex:
            print(f"   [Warn] Polling pending exit order {sell_oid}: {ex}")
            continue

        sell_status = chk_sell.get("status")
        buy_px = float(hunt.get("buy_price") or 0.0)
        buy_qty = float(hunt.get("buy_qty") or hunt.get("qty") or 0.0)

        if sell_status == "filled":
            fill_px = float(chk_sell.get("filled_avg_price") or hunt.get("sell_price") or 0.0)
            fill_qty = float(chk_sell.get("filled_qty") or hunt.get("qty") or 0.0)
            fill_time = chk_sell.get("filled_at") or chk_sell.get("created_at") or now_str
            net_pnl, pnl_pct, gross_pnl, fees_usd = compute_net_trade_pnl(
                buy_px, fill_px, buy_qty, fill_qty, is_crypto=is_crypto
            )
            hunt["sell_price"] = fill_px
            hunt["qty"] = fill_qty
            hunt["filled_exit_qty"] = fill_qty
            hunt["sell_timestamp"] = fill_time
            hunt["pnl"] = net_pnl
            hunt["pnl_pct"] = pnl_pct
            hunt["gross_pnl"] = gross_pnl
            hunt["fees_usd"] = fees_usd
            hunt["status"] = "completed"
            history_changed = True
            print(
                f"   ✅ [EXIT FILLED] Hunt {hunt['id']}: Sold {format_order_qty(fill_qty, is_crypto)} @ ${fill_px:.2f} "
                f"(Net PnL: ${net_pnl:+.2f} | Gross: ${gross_pnl:+.2f} | Fees: ${fees_usd:.2f})"
            )

        elif sell_status in ("canceled", "rejected", "expired"):
            filled_so_far = float(chk_sell.get("filled_qty") or 0.0)
            order_qty = float(chk_sell.get("qty") or hunt.get("qty") or 0.0)
            rem_qty = floor_qty(max(0.0, order_qty - filled_so_far), decimals)
            print(
                f"   ⚠️ [EXIT {sell_status.upper()}] Hunt {hunt['id']} order {sell_oid} ended as {sell_status} "
                f"(filled={filled_so_far}, remaining={rem_qty}). Checking recovery..."
            )
            if rem_qty > CRYPTO_DUST_QTY and position_fetch_ok:
                # Re-query live available balance and re-submit protective exit sell
                try:
                    fresh_pos = client.get_positions(symbol)
                    fresh_avail = floor_qty(float(fresh_pos.get("qty_available") or 0.0), decimals) if fresh_pos else 0.0
                except Exception:
                    fresh_avail = avail_pool

                re_qty = floor_qty(min(rem_qty, fresh_avail), decimals)
                if re_qty > CRYPTO_DUST_QTY:
                    exit_px = float(hunt.get("sell_price") or 0.0)
                    if exit_px <= 0:
                        exit_px = max(round(buy_px + tick, 2), round(fair_price, 2))
                    sell_ord, submitted_qty = submit_exit_sell_with_retry(client, symbol, re_qty, exit_px, tif=tif)
                    if sell_ord and sell_ord.get("id"):
                        if filled_so_far > 0:
                            hunt["buy_qty"] = floor_qty(max(submitted_qty, buy_qty - filled_so_far), decimals)
                        hunt["sell_order_id"] = sell_ord["id"]
                        hunt["qty"] = submitted_qty
                        hunt["filled_exit_qty"] = 0.0
                        hunt["status"] = "open_exit_pending"
                        history_changed = True
                        print(f"   🔄 [EXIT RE-RAISED] Hunt {hunt['id']}: New SELL {format_order_qty(submitted_qty, is_crypto)} @ ${exit_px:.2f} ({sell_ord['id']})")
                    else:
                        if filled_so_far > 0:
                            hunt["buy_qty"] = floor_qty(max(re_qty, buy_qty - filled_so_far), decimals)
                        hunt["sell_order_id"] = None
                        hunt["qty"] = re_qty
                        hunt["status"] = "exit_submit_pending"
                        history_changed = True
                else:
                    # Shares no longer exist on broker (liquidated manually/externally)
                    fill_px = float(chk_sell.get("filled_avg_price") or bid or buy_px)
                    net_pnl, pnl_pct, gross_pnl, fees_usd = compute_net_trade_pnl(
                        buy_px, fill_px, buy_qty, filled_so_far if filled_so_far > 0 else buy_qty, is_crypto=is_crypto
                    )
                    hunt["sell_price"] = fill_px
                    hunt["sell_timestamp"] = chk_sell.get("canceled_at") or now_str
                    hunt["pnl"] = net_pnl
                    hunt["pnl_pct"] = pnl_pct
                    hunt["gross_pnl"] = gross_pnl
                    hunt["fees_usd"] = fees_usd
                    hunt["status"] = "liquidated_manual"
                    history_changed = True
            else:
                hunt["status"] = "completed" if filled_so_far > 0 else "liquidated_manual"
                hunt["sell_timestamp"] = chk_sell.get("filled_at") or chk_sell.get("canceled_at") or now_str
                if hunt.get("pnl") is None:
                    fill_px = float(chk_sell.get("filled_avg_price") or hunt.get("sell_price") or buy_px)
                    net_pnl, pnl_pct, gross_pnl, fees_usd = compute_net_trade_pnl(
                        buy_px, fill_px, buy_qty, filled_so_far if filled_so_far > 0 else buy_qty, is_crypto=is_crypto
                    )
                    hunt["pnl"] = net_pnl
                    hunt["pnl_pct"] = pnl_pct
                    hunt["gross_pnl"] = gross_pnl
                    hunt["fees_usd"] = fees_usd
                history_changed = True

        elif sell_status in OPEN_ORDER_STATUSES:
            # Update partial fill progress if applicable
            if sell_status == "partially_filled":
                partial_qty = float(chk_sell.get("filled_qty") or 0.0)
                if hunt.get("status") != "exit_partially_filled" or abs(float(hunt.get("filled_exit_qty") or 0.0) - partial_qty) > 1e-8:
                    hunt["filled_exit_qty"] = partial_qty
                    hunt["status"] = "exit_partially_filled"
                    history_changed = True

            # Check 12-hour Stale Order Liquidation timeout (stale_order_max_age_seconds)
            created_dt = parse_iso_timestamp(chk_sell.get("created_at")) or parse_iso_timestamp(hunt.get("buy_timestamp"))
            if created_dt and stale_max_age > 0:
                age_secs = (now_dt - created_dt).total_seconds()
                if age_secs >= stale_max_age:
                    print(
                        f"   ⏰ [STALE LIQUIDATION] Hunt {hunt['id']} exit order {sell_oid} age "
                        f"{age_secs / 3600:.2f}h >= {stale_max_age / 3600:.2f}h. Canceling & liquidating at MARKET..."
                    )
                    try:
                        client.cancel_order(sell_oid)
                        term_sell = wait_for_terminal_order(client, sell_oid, max_wait_secs=8.0, poll_interval=0.5, fallback_order=chk_sell)
                    except Exception as ce:
                        print(f"   [Warn] Failed canceling stale order {sell_oid}: {ce}")
                        continue

                    partial_qty = float(term_sell.get("filled_qty") or 0.0)
                    partial_px = float(term_sell.get("filled_avg_price") or hunt.get("sell_price") or 0.0)
                    order_qty = float(term_sell.get("qty") or hunt.get("qty") or 0.0)
                    rem_to_liq = floor_qty(max(0.0, order_qty - partial_qty), decimals)

                    if term_sell.get("status") == "filled" or rem_to_liq <= CRYPTO_DUST_QTY:
                        # Filled right before cancel!
                        fill_qty = partial_qty if partial_qty > 0 else order_qty
                        net_pnl, pnl_pct, gross_pnl, fees_usd = compute_net_trade_pnl(
                            buy_px, partial_px, buy_qty, fill_qty, is_crypto=is_crypto
                        )
                        hunt["sell_price"] = partial_px
                        hunt["qty"] = fill_qty
                        hunt["filled_exit_qty"] = fill_qty
                        hunt["sell_timestamp"] = term_sell.get("filled_at") or now_str
                        hunt["pnl"] = net_pnl
                        hunt["pnl_pct"] = pnl_pct
                        hunt["gross_pnl"] = gross_pnl
                        hunt["fees_usd"] = fees_usd
                        hunt["status"] = "completed"
                        history_changed = True
                        continue

                    # Query fresh available balance after cancel releases locked shares
                    try:
                        fresh_pos = client.get_positions(symbol)
                        fresh_avail = floor_qty(float(fresh_pos.get("qty_available") or 0.0), decimals) if fresh_pos else 0.0
                    except Exception as pe:
                        print(f"   [Warn] Fetching position for stale liquidation: {pe}")
                        fresh_avail = rem_to_liq

                    liq_qty = floor_qty(min(rem_to_liq, fresh_avail), decimals)
                    if liq_qty > CRYPTO_DUST_QTY:
                        try:
                            mkt_ord = client.submit_order(
                                symbol,
                                format_order_qty(liq_qty, is_crypto=is_crypto),
                                "sell",
                                "market",
                                limit_price=None,
                                tif=market_tif,
                            )
                            mkt_id = mkt_ord.get("id")
                            mkt_term = wait_for_terminal_order(client, mkt_id, max_wait_secs=10.0, poll_interval=0.5, fallback_order=mkt_ord) if mkt_id else mkt_ord
                            mkt_filled_qty = float(mkt_term.get("filled_qty") or liq_qty)
                            mkt_fill_px = float(mkt_term.get("filled_avg_price") or bid or buy_px)
                            mkt_time = mkt_term.get("filled_at") or now_str

                            total_sold_qty = partial_qty + mkt_filled_qty
                            blended_px = (
                                (partial_qty * partial_px + mkt_filled_qty * mkt_fill_px) / total_sold_qty
                                if total_sold_qty > 0
                                else mkt_fill_px
                            )
                            # Market liquidation pays taker sell fee (0.25% on crypto)
                            net_pnl, pnl_pct, gross_pnl, fees_usd = compute_net_trade_pnl(
                                buy_px,
                                blended_px,
                                buy_qty,
                                total_sold_qty,
                                is_crypto=is_crypto,
                                sell_fee_rate=CRYPTO_TAKER_FEE_RATE if is_crypto else 0.0,
                            )
                            hunt["sell_order_id"] = mkt_id or sell_oid
                            hunt["sell_price"] = round(blended_px, 4)
                            hunt["qty"] = floor_qty(total_sold_qty, decimals)
                            hunt["filled_exit_qty"] = floor_qty(total_sold_qty, decimals)
                            hunt["sell_timestamp"] = mkt_time
                            hunt["pnl"] = net_pnl
                            hunt["pnl_pct"] = pnl_pct
                            hunt["gross_pnl"] = gross_pnl
                            hunt["fees_usd"] = fees_usd
                            hunt["status"] = "liquidated_stale"
                            history_changed = True
                            print(
                                f"   🔴 [LIQUIDATED STALE] Hunt {hunt['id']}: Market sold {format_order_qty(mkt_filled_qty, is_crypto)} "
                                f"@ ${mkt_fill_px:.2f} (Net PnL: ${net_pnl:+.2f})"
                            )
                        except Exception as le:
                            print(f"   ❌ [STALE LIQUIDATION ERROR] Market sell failed for Hunt {hunt['id']}: {le}")
                            hunt["sell_order_id"] = None
                            hunt["qty"] = liq_qty
                            hunt["status"] = "exit_submit_pending"
                            history_changed = True

    # --- STEP C: Refresh Live Position & Open Orders + Adopt Untracked Orders & Protect Orphans ---
    try:
        position = client.get_positions(symbol)
        position_fetch_ok = True
    except Exception as exc:
        print(f"   [Warn] Post-hygiene get_positions({symbol}) failed: {exc}")

    try:
        open_orders = client.get_open_orders(symbol) or []
        open_orders_fetch_ok = True
    except Exception as exc:
        print(f"   [Warn] Post-hygiene get_open_orders({symbol}) failed: {exc}")

    if open_orders_fetch_ok:
        tracked_sell_ids = {
            h.get("sell_order_id")
            for h in trades_history
            if h.get("status") in ACTIVE_HUNT_STATUSES and h.get("sell_order_id")
        }
        avg_entry_ref = float(position.get("avg_entry_price") or bid or 0.0) if position else bid

        for o in open_orders:
            if o.get("side") != "sell":
                continue
            oid = o.get("id")
            if oid and oid not in tracked_sell_ids:
                o_qty = float(o.get("qty") or 0.0)
                o_filled = float(o.get("filled_qty") or 0.0)
                o_limit = float(o.get("limit_price") or 0.0)
                adopted_status = "exit_partially_filled" if o.get("status") == "partially_filled" else "open_exit_pending"
                adopted_rec = {
                    "id": f"adp-{str(oid)[:4]}",
                    "symbol": symbol,
                    "buy_order_id": None,
                    "sell_order_id": oid,
                    "buy_qty": o_qty,
                    "qty": o_qty,
                    "filled_exit_qty": o_filled,
                    "buy_price": round(avg_entry_ref, 4),
                    "sell_price": o_limit,
                    "pnl": None,
                    "pnl_pct": None,
                    "gross_pnl": None,
                    "fees_usd": None,
                    "buy_timestamp": o.get("created_at") or now_str,
                    "sell_timestamp": None,
                    "status": adopted_status,
                    "adopted_from_broker": True,
                }
                trades_history.append(adopted_rec)
                tracked_sell_ids.add(oid)
                history_changed = True
                print(
                    f"   🧭 [ADOPTED OPEN ORDER] Tracking untracked broker SELL {oid}: "
                    f"{format_order_qty(o_qty, is_crypto)} {symbol} @ ${o_limit:.2f}"
                )

    # Auto-protect any orphaned unencumbered shares sitting on Alpaca with zero exit sell order
    has_submit_pending = any(h.get("status") == "exit_submit_pending" for h in trades_history)
    if position_fetch_ok and position and not has_submit_pending:
        orphan_avail = floor_qty(float(position.get("qty_available") or 0.0), decimals)
        cur_px = bid or float(position.get("current_price") or 0.0)
        avg_entry = float(position.get("avg_entry_price") or cur_px)

        if orphan_avail > CRYPTO_DUST_QTY and (cur_px <= 0 or orphan_avail * cur_px >= MIN_PROTECT_NOTIONAL_USD):
            exit_px = (
                max(round(avg_entry + tick, 2), round(fair_price, 2))
                if fair_price > 0
                else max(round(avg_entry + tick, 2), round(cur_px + tick, 2))
            )
            print(
                f"   🚨 [ORPHAN DETECTED] Found {format_order_qty(orphan_avail, is_crypto)} unprotected {symbol} "
                f"(~${orphan_avail * cur_px:,.2f}) with NO open exit sell! Placing protective Limit SELL @ ${exit_px:.2f}..."
            )
            orphan_rec = {
                "id": f"rec-{str(uuid.uuid4())[:4]}",
                "symbol": symbol,
                "buy_order_id": None,
                "sell_order_id": None,
                "buy_qty": orphan_avail,
                "qty": orphan_avail,
                "filled_exit_qty": 0.0,
                "buy_price": round(avg_entry, 4),
                "sell_price": exit_px,
                "pnl": None,
                "pnl_pct": None,
                "gross_pnl": None,
                "fees_usd": None,
                "buy_timestamp": now_str,
                "sell_timestamp": None,
                "status": "exit_submit_pending",
                "recovered_orphan": True,
            }
            trades_history.append(orphan_rec)
            history_changed = True
            if store is not None:
                store.save_hunts(trades_history, max_count=50)

            sell_ord, submitted_qty = submit_exit_sell_with_retry(client, symbol, orphan_avail, exit_px, tif=tif)
            if sell_ord and sell_ord.get("id"):
                orphan_rec["sell_order_id"] = sell_ord["id"]
                orphan_rec["qty"] = submitted_qty
                orphan_rec["status"] = "completed" if sell_ord.get("status") == "filled" else "open_exit_pending"
                if orphan_rec["status"] == "completed":
                    fill_px = float(sell_ord.get("filled_avg_price") or exit_px)
                    fill_qty = float(sell_ord.get("filled_qty") or submitted_qty)
                    net_pnl, pnl_pct, gross_pnl, fees_usd = compute_net_trade_pnl(
                        avg_entry, fill_px, orphan_avail, fill_qty, is_crypto=is_crypto
                    )
                    orphan_rec["sell_price"] = fill_px
                    orphan_rec["sell_timestamp"] = sell_ord.get("filled_at") or now_str
                    orphan_rec["pnl"] = net_pnl
                    orphan_rec["pnl_pct"] = pnl_pct
                    orphan_rec["gross_pnl"] = gross_pnl
                    orphan_rec["fees_usd"] = fees_usd
                print(
                    f"   🛡️ [ORPHAN PROTECTED] Placed protective SELL {format_order_qty(submitted_qty, is_crypto)} "
                    f"@ ${exit_px:.2f} (Order ID: {sell_ord['id']})"
                )
                # Refresh position & open orders so telemetry immediately reflects protected state
                try:
                    position = client.get_positions(symbol)
                    open_orders = client.get_open_orders(symbol) or []
                except Exception:
                    pass

    broker_telemetry = build_broker_telemetry(account, position, open_orders, trades_history)
    return history_changed, broker_telemetry


def main():
    params, key, secret, endpoint = load_config()
    symbol = params["symbol"]
    is_crypto = "/" in symbol
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
    stale_max_age = params.get("stale_order_max_age_seconds", 43200)
    tif = "gtc" if is_crypto else "day"

    print("=" * 70)
    print("🚀 ANOMALIES TRADING ENGINE v2.0 (LIVE BOT - FEE-AWARE & RECONCILED)")
    print(f"   Symbol: {symbol} | Account: Alpaca Paper Trading")
    print(f"   Cycle: {cycle_secs}s | Sizing Level: {params['level']} | Max Shares/Cap: {params['max_shares']}")
    print(f"   Min Margin: {params['min_margin']*100:.3f}% | Max Margin (Circuit Breaker): {params['max_margin']*100:.2f}%")
    print(f"   Stale Liquidation Limit: {stale_max_age}s ({stale_max_age/3600:.1f}h)")
    print("=" * 70)

    store = LocalPersistenceStore(DATA_DIR)

    # 1. Recover existing hunts from persistent store (preserving all active + last 50 completed)
    trades_history = store.load_hunts(max_count=50)

    # 2. If empty, reconcile from Alpaca closed orders history
    if not trades_history:
        try:
            closed_orders = client.get_closed_orders(symbol=symbol, limit=500)
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

    print(f"📦 [PERSISTENCE] Loaded: {len(trades_history)} hunts | Total Realized PnL: ${total_realized_pnl:+.2f} | Win Rate: {metrics.get('win_rate', 0.0)}% | Sharpe: {metrics.get('sharpe_ratio', 0.0)}")

    while RUNNING:
        cycle_start = time.time()
        now_dt = datetime.now(timezone.utc)
        now_str = now_dt.isoformat()

        try:
            # 1. Fetch live quote & update Adaptive 1D Kalman Filter first (needed for orphan exit pricing)
            quote = client.get_latest_quote(symbol)
            bid = float(quote["bp"])
            ask = float(quote["ap"])
            mid = 0.5 * (bid + ask)
            spread = ask - bid

            sig, margin, fair_price = kalman.update(bid, ask)

            # 2. Run full Broker Reconciliation, Exit Monitoring, 12h Stale Liquidation & Orphan Protection
            pending_updated, broker_telemetry = reconcile_and_maintain_positions(
                client=client,
                symbol=symbol,
                trades_history=trades_history,
                params=params,
                fair_price=fair_price,
                bid=bid,
                now_dt=now_dt,
                tif=tif,
                store=store,
            )

            if pending_updated:
                trades_history = trim_hunts_preserving_active(trades_history, max_count=50)
                metrics = compute_performance_metrics(trades_history)
                store.save_hunts(trades_history, max_count=50)
                store.save_metrics(metrics)
                total_realized_pnl = float(metrics.get("total_pnl", 0.0))

            recon = broker_telemetry["reconciliation"]
            sync_badge = "SYNCED" if recon["is_synced"] else f"DRIFT={recon['untracked_qty']:+.4f}"
            print(
                f"[{now_str[:19]}] Bid: ${bid:.2f} | Ask: ${ask:.2f} (Spread: ${spread:.2f}) | "
                f"Kalman: ${fair_price:.2f} | Margin: {margin*100:+.4f}% | "
                f"Pos: {recon['broker_qty']:.4f} ({sync_badge})"
            )

            if sig == "circuit_breaker":
                print(f"⚠️ [CIRCUIT BREAKER] Deviation {margin*100:.3f}% > {params['max_margin']*100:.2f}%. Trading HALTED.")
            elif sig == "buy_anomaly":
                # HiddenMM pricing
                tick = 0.01
                spread_ticks = round((ask - bid) / tick)
                buy_price = round(bid + tick, 2) if spread_ticks >= 2 else round(bid, 2)

                # Fetch fresh account balance and pre-buy available position
                equity = float((broker_telemetry["broker_account"].get("equity")) or 100000.0)
                cash = float((broker_telemetry["broker_account"].get("cash")) or equity)
                avail_before = float(broker_telemetry["broker_position"].get("qty_available") or 0.0)
                try:
                    acc = client.get_account()
                    equity = float(acc.get("equity", equity))
                    cash = float(acc.get("cash", cash))
                    pos_pre = client.get_positions(symbol)
                    avail_before = floor_qty(float(pos_pre.get("qty_available") or 0.0), 9 if is_crypto else 4) if pos_pre else 0.0
                except Exception as e:
                    print(f"   [Warn] Refreshing pre-buy account/position: {e}")

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
                    buy_ord = None
                    try:
                        buy_ord = client.submit_order(symbol, shares, "buy", "limit", buy_price, tif=tif)
                    except Exception as be:
                        print(f"   ⚠️ [BUY REJECTED] Could not submit BUY {shares} @ ${buy_price:.2f}: {be}")

                    ord_id = buy_ord.get("id") if isinstance(buy_ord, dict) else None
                    if ord_id:
                        start_wait = time.time()
                        filled_qty = float(buy_ord.get("filled_qty") or 0.0)
                        avg_price = float(buy_ord.get("filled_avg_price") or buy_price)
                        last_status = buy_ord.get("status")
                        chk = buy_ord

                        while time.time() - start_wait < hidden_mm_timeout and RUNNING:
                            if last_status in TERMINAL_ORDER_STATUSES:
                                break
                            time.sleep(1.5)
                            try:
                                chk = client.get_order(ord_id)
                                last_status = chk.get("status")
                                if chk.get("filled_qty"):
                                    filled_qty = float(chk.get("filled_qty") or 0.0)
                                if chk.get("filled_avg_price"):
                                    avg_price = float(chk.get("filled_avg_price") or buy_price)
                                if last_status in TERMINAL_ORDER_STATUSES:
                                    break
                            except Exception as e:
                                print(f"   [Warn] Polling buy order {ord_id}: {e}")

                        # Only cancel if not already terminal, then ALWAYS wait until strictly terminal status
                        if last_status not in TERMINAL_ORDER_STATUSES:
                            client.cancel_order(ord_id)
                        chk = wait_for_terminal_order(client, ord_id, max_wait_secs=8.0, poll_interval=0.5, fallback_order=chk)
                        filled_qty = float(chk.get("filled_qty") or filled_qty or 0.0)
                        if chk.get("filled_avg_price"):
                            avg_price = float(chk["filled_avg_price"])

                        if filled_qty > 0:
                            buy_fill_time = chk.get("filled_at") or now_str
                            exit_price = max(round(avg_price + tick, 2), round(fair_price, 2))

                            # Compute exact fee-adjusted & balance-verified exit sell quantity
                            safe_sell_qty, live_avail = compute_safe_exit_sell_qty(
                                filled_qty=filled_qty,
                                symbol=symbol,
                                client=client,
                                avail_before=avail_before,
                            )
                            print(
                                f"   ✅ [FILLED] Bought {format_order_qty(filled_qty, is_crypto)} @ ${avg_price:.2f} | "
                                f"Net sellable (after crypto fee): {format_order_qty(safe_sell_qty, is_crypto)} "
                                f"(Live avail: {format_order_qty(live_avail, is_crypto)}). Persisting & submitting Exit Limit SELL @ ${exit_price:.2f}..."
                            )

                            # CRITICAL: Persist hunt BEFORE submitting exit sell so crashes/HTTP 403 can NEVER drop a hunt
                            trade_rec = {
                                "id": str(uuid.uuid4())[:8],
                                "symbol": symbol,
                                "buy_order_id": ord_id,
                                "sell_order_id": None,
                                "buy_qty": filled_qty,
                                "qty": safe_sell_qty,
                                "filled_exit_qty": 0.0,
                                "buy_price": avg_price,
                                "sell_price": exit_price,
                                "pnl": None,
                                "pnl_pct": None,
                                "gross_pnl": None,
                                "fees_usd": None,
                                "buy_timestamp": buy_fill_time,
                                "sell_timestamp": None,
                                "status": "exit_submit_pending",
                            }
                            trades_history.append(trade_rec)
                            trades_history = trim_hunts_preserving_active(trades_history, max_count=50)
                            store.save_hunts(trades_history, max_count=50)

                            sell_ord, submitted_qty = submit_exit_sell_with_retry(
                                client=client,
                                symbol=symbol,
                                sell_qty=safe_sell_qty,
                                exit_price=exit_price,
                                tif=tif,
                            )

                            if sell_ord and sell_ord.get("id"):
                                sell_id = sell_ord.get("id")
                                sell_status = sell_ord.get("status")
                                is_filled = (sell_status == "filled")
                                trade_rec["sell_order_id"] = sell_id
                                trade_rec["qty"] = submitted_qty

                                if is_filled:
                                    fill_px = float(sell_ord.get("filled_avg_price") or exit_price)
                                    fill_s_qty = float(sell_ord.get("filled_qty") or submitted_qty)
                                    net_pnl, pnl_pct, gross_pnl, fees_usd = compute_net_trade_pnl(
                                        avg_price, fill_px, filled_qty, fill_s_qty, is_crypto=is_crypto
                                    )
                                    trade_rec["sell_price"] = fill_px
                                    trade_rec["qty"] = fill_s_qty
                                    trade_rec["filled_exit_qty"] = fill_s_qty
                                    trade_rec["sell_timestamp"] = sell_ord.get("filled_at") or now_str
                                    trade_rec["pnl"] = net_pnl
                                    trade_rec["pnl_pct"] = pnl_pct
                                    trade_rec["gross_pnl"] = gross_pnl
                                    trade_rec["fees_usd"] = fees_usd
                                    trade_rec["status"] = "completed"
                                else:
                                    trade_rec["status"] = "open_exit_pending"
                            else:
                                print(f"   ⚠️ [EXIT DEFERRED] Hunt {trade_rec['id']} persisted as exit_submit_pending; will auto-retry next cycle.")

                            # Recompute and persist metrics and hunts
                            metrics = compute_performance_metrics(trades_history)
                            store.save_hunts(trades_history, max_count=50)
                            store.save_metrics(metrics)
                            total_realized_pnl = float(metrics.get("total_pnl", 0.0))

                            if trade_rec["status"] == "completed":
                                print(
                                    f"   💰 [EXIT FILLED] SELL {format_order_qty(trade_rec['qty'], is_crypto)} @ ${trade_rec['sell_price']:.2f} "
                                    f"(Net PnL: ${trade_rec['pnl']:+.2f} | Total: ${total_realized_pnl:+.2f})"
                                )
                            elif trade_rec["status"] == "open_exit_pending":
                                print(
                                    f"   ⏳ [EXIT PLACED] SELL {format_order_qty(trade_rec['qty'], is_crypto)} @ ${exit_price:.2f} "
                                    f"(Status: open_exit_pending | Order ID: {trade_rec['sell_order_id']})"
                                )

                            # Refresh broker telemetry immediately so telemetry.json mirrors the new position
                            try:
                                acc_post = client.get_account()
                                pos_post = client.get_positions(symbol)
                                open_post = client.get_open_orders(symbol) or []
                                broker_telemetry = build_broker_telemetry(acc_post, pos_post, open_post, trades_history)
                            except Exception:
                                pass
                        else:
                            print("   ⏱️ [UNFILLED] 20s timeout expired. Remainder canceled. 0 shares bought.")

            # Save persistent snapshot to data/telemetry.json (including live broker reconciliation!)
            active_hunts_count = len([t for t in trades_history if t.get("status") in ACTIVE_HUNT_STATUSES])
            completed_trades_count = len([t for t in trades_history if t.get("status") in CLOSED_TRADE_STATUSES])
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
                "circuit_breaker_active": sig == "circuit_breaker",
                "active_hunts_count": active_hunts_count,
                "completed_trades_count": completed_trades_count,
                "total_realized_pnl": round(total_realized_pnl, 2),
                **broker_telemetry,
            }
            store.save_telemetry_snapshot(telemetry_snapshot, trades_history, metrics)

        except Exception as e:
            print(f"[{now_str[:19]}] [Error in cycle]: {e}")

        elapsed = time.time() - cycle_start
        if elapsed < cycle_secs and RUNNING:
            time.sleep(cycle_secs - elapsed)


if __name__ == "__main__":
    main()
