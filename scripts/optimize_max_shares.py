#!/usr/bin/env python3
"""
Optimization of max_shares based on historical anomaly frequency and 3-hour expected profit.

Mathematical Formulation:
- Cycle interval: 30 seconds -> 3 hours = 360 cycles.
- Empirical linear probability of anomaly per cycle: p = N_anomalies / N_cycles.
- Expected anomalies in 3h: E[A] = 360 * p.
- Filled trades: E[N_trades] = E[A] * P_fill.
- Capital allocation: Target Capital = Factor(x) * (Total Equity * S).
  Capped at remaining available cash: min(Target Capital, Available Cash).
- Position hold duration: tau cycles (empirical average hold time before fair price exit).
- Queueing distribution of concurrent positions: Poisson(L) where L = (p * P_fill) * tau.
- Expected 3h profit function:
    G(S) = E[N_trades] * r_bar * E[Capital(S)]
"""

import json
import math
import sys
from pathlib import Path

# Paths
ROOT_DIR = Path(__file__).resolve().parent.parent
DATA_DIR = ROOT_DIR / "data"

def load_telemetry_and_history():
    """Load telemetry and historical performance metrics."""
    telem_path = DATA_DIR / "telemetry.json"
    if not telem_path.exists():
        # Fallback to sample data
        telem_path = ROOT_DIR / "dashboard" / "data" / "telemetry.json"
    
    with open(telem_path, "r", encoding="utf-8") as f:
        telem = json.load(f)
    return telem


def compute_empirical_parameters():
    """
    Empirical parameters computed from continuous 30s cycle logs on Alpaca SOL/USD:
    - 484 cycles logged in systemd production run.
    - 50 market hunt anomalies detected.
    - 26 completed trades filled and exited at fair price.
    - 24 orders timed out at 20s and canceled cleanly.
    - Average realized return per completed trade: r_bar = +0.2358%.
    - Average hold time until fair price exit: tau = 15.81 cycles (474.4 seconds).
    - Account equity: W = $100,047.50 USD.
    """
    total_cycles = 484
    total_anomalies = 50
    total_fills = 26
    r_bar = 0.002358          # +0.2358% average profit per trade
    tau_cycles = 15.81        # average duration in cycles
    equity = 100047.50        # total portfolio equity

    # Linear anomaly probability
    p_anomaly = total_anomalies / total_cycles
    p_fill = total_fills / total_anomalies

    return {
        "total_cycles": total_cycles,
        "total_anomalies": total_anomalies,
        "p_anomaly": p_anomaly,
        "p_fill": p_fill,
        "r_bar": r_bar,
        "tau_cycles": tau_cycles,
        "equity": equity,
        "cycles_3h": 360,
    }


def expected_3h_profit(S: float, params: dict) -> float:
    """
    Computes expected 3h profit G(S) for equity fraction S in (0, 1].
    """
    M = params["cycles_3h"]
    p = params["p_anomaly"]
    P_fill = params["p_fill"]
    r_bar = params["r_bar"]
    tau = params["tau_cycles"]
    W = params["equity"]

    # Arrival rate of filled trades per cycle
    lam = p * P_fill
    # Little's Law: average concurrent open positions
    L = lam * tau
    expected_filled_trades = M * lam

    # Calculate expected deployed capital per trade accounting for cash starvation
    # Number of open trades follows Poisson(L)
    expected_cap_ratio = 0.0
    for k in range(25):
        pk = (L**k * math.exp(-L)) / math.factorial(k)
        # When k trades of fraction S are open, remaining cash fraction is max(0, 1 - k*S)
        rem_cash_fraction = max(0.0, 1.0 - k * S)
        # User directive: "si no hi ha prous diners simplement posa tot el que queda"
        deployed_fraction = min(S, rem_cash_fraction)
        expected_cap_ratio += pk * deployed_fraction

    expected_capital_per_trade = expected_cap_ratio * W
    total_profit = expected_filled_trades * r_bar * expected_capital_per_trade
    return total_profit


def run_optimization():
    params = compute_empirical_parameters()
    p = params["p_anomaly"]
    p_fill = params["p_fill"]
    tau = params["tau_cycles"]
    W = params["equity"]

    print("=" * 75)
    print("📈 EMPIRICAL CALIBRATION & MAX_SHARES OPTIMIZATION (3-HOUR HORIZON)")
    print("=" * 75)
    print(f"Total Evaluated 30s Cycles:  {params['total_cycles']}")
    print(f"Total Detected Anomalies:    {params['total_anomalies']}")
    print(f"Linear Anomaly Probability:  p = {p:.5f} ({p*100:.3f}% per 30s cycle)")
    print(f"Anomaly Recurrence:          1 anomaly every {1/p:.2f} cycles (~{(1/p)*30:.1f}s / {(1/p)*30/60:.2f} min)")
    print(f"Maker Fill Probability:      P_fill = {p_fill*100:.2f}% (20s timeout)")
    print(f"Average Realized Return:     r_bar = +{params['r_bar']*100:.4f}%")
    print(f"Average Position Hold Time:  tau = {tau:.1f} cycles (~{tau*30:.1f}s / {tau*30/60:.2f} min)")
    print(f"Total Portfolio Equity:      W = ${W:,.2f}")
    print(f"3-Hour Evaluation Window:    360 cycles (10,800 seconds)")
    print("-" * 75)

    # Grid search for optimal S
    best_S = 0.0
    best_profit = -float("inf")
    grid = [i / 1000.0 for i in range(10, 1001)] # 0.010 to 1.000

    for S in grid:
        profit = expected_3h_profit(S, params)
        if profit > best_profit:
            best_profit = profit
            best_S = S

    print(f"\n🎯 OPTIMIZATION RESULT:")
    print(f"   Optimal max_shares (S*): {best_S:.3f} ({best_S*100:.1f}% of total equity)")
    print(f"   Max Expected 3h Profit:  ${best_profit:.2f} USD")
    print("\n📊 TRADE-OFF SENSITIVITY TABLE:")
    print(f"{'max_shares (S)':<16} | {'Max Cap / Trade':<18} | {'Concurrent Slots':<18} | {'Expected 3h Profit':<20}")
    print("-" * 78)

    sample_tiers = [0.05, 0.10, 0.15, 0.20, 0.25, 0.333, 0.50, 0.75, 1.00]
    for s_tier in sample_tiers:
        prof = expected_3h_profit(s_tier, params)
        slots = int(math.floor(1.0 / s_tier))
        cap_trade = s_tier * W
        star_flag = " 🏆 (OPTIMUM)" if abs(s_tier - best_S) < 0.005 else ""
        print(f"S = {s_tier:<5.3f} ({s_tier*100:>4.1f}%) | ${cap_trade:>14,.2f}  | {slots:>2} concurrent slot(s) | ${prof:>9,.2f} USD{star_flag}")

    print("=" * 75)
    print("💡 STRATEGIC CONCLUSIONS:")
    print("1. Unconstrained Profit Maximizer (S* = 1.00 / 100%):")
    print("   Maximizes total dollar return ($1,951.35 in 3h) by fully compounding every dollar")
    print("   into the high-edge anomaly (+0.2358%), putting all remaining cash into each hunt.")
    print("2. Balanced Multi-Slot Alternative (S = 0.50 / 50% or S = 0.33 / 33%):")
    print("   Supports 2 to 3 concurrent trades simultaneously, capturing >84% to >95% of all")
    print("   overlapping anomalies while still yielding $1,425 - $1,804 in 3h.")
    print("=" * 75)


if __name__ == "__main__":
    run_optimization()
