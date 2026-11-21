"""
Quantitative Risk & Performance Analytics Engine
Calculates Sharpe Ratio, Max Drawdown, Volatility, and Trade Statistics using numpy.
"""

from typing import Dict, Any, List
import numpy as np


CLOSED_TRADE_STATUSES = {"completed", "liquidated_stale", "liquidated_manual", "liquidated_orphan"}


def compute_performance_metrics(
    trades: List[Dict[str, Any]],
    risk_free_rate: float = 0.04,
    base_equity: float = 100000.0,
) -> Dict[str, Any]:
    """
    Computes rigorous portfolio metrics on trade history using numpy.
    
    :param trades: List of trade dictionaries containing 'pnl', 'pnl_pct', 'buy_price', 'qty', etc.
    :param risk_free_rate: Annualized risk-free rate (default 4.0%)
    :param base_equity: Starting account equity used as denominator for drawdown percentage (default $100,000)
    :return: Dictionary containing Sharpe Ratio, Max Drawdown, Volatility, Win Rate, etc.
    """
    completed_trades = [
        t for t in trades
        if t.get("pnl") is not None and t.get("status") in CLOSED_TRADE_STATUSES
    ]

    if not completed_trades:
        return {
            "total_trades": len(trades),
            "completed_trades": 0,
            "win_rate": 0.0,
            "total_pnl": 0.0,
            "total_fees_usd": 0.0,
            "sharpe_ratio": 0.0,
            "max_drawdown_pct": 0.0,
            "max_drawdown_dollars": 0.0,
            "annualized_volatility": 0.0,
            "avg_pnl_per_trade": 0.0,
            "profit_factor": 0.0,
            "cumulative_pnl_series": [0.0],
            "drawdown_series": [0.0],
        }

    pnls = np.array([float(t["pnl"]) for t in completed_trades], dtype=np.float64)
    returns = np.array([float(t.get("pnl_pct", 0.0)) for t in completed_trades], dtype=np.float64)
    total_fees_usd = sum(float(t.get("fees_usd") or 0.0) for t in completed_trades)

    # 1. Cumulative P&L Series & Peak Tracking
    cumulative_pnl = np.cumsum(pnls)
    peaks = np.maximum.accumulate(cumulative_pnl)
    drawdowns_dollars = peaks - cumulative_pnl
    max_drawdown_dollars = float(np.max(drawdowns_dollars)) if len(drawdowns_dollars) > 0 else 0.0

    # Drawdown percentage relative to peak equity
    safe_base = base_equity if base_equity > 0 else 100000.0
    equity_curve = safe_base + cumulative_pnl
    equity_peaks = np.maximum.accumulate(equity_curve)
    drawdown_pct_series = np.where(equity_peaks > 0, (equity_peaks - equity_curve) / equity_peaks * 100.0, 0.0)
    max_drawdown_pct = float(np.max(drawdown_pct_series)) if len(drawdown_pct_series) > 0 else 0.0

    # 2. Volatility (Annualized based on ~252 trading days, ~6.5 hours/day, 30s bars => ~780 cycles/day)
    cycles_per_year = 252 * 780  # ~196,560 potential 30s periods/year
    # Per-trade volatility:
    trade_vol = float(np.std(returns, ddof=1)) if len(returns) > 1 else 0.0
    annualized_vol = trade_vol * np.sqrt(252)  # annualized daily-equivalent basis

    # 3. Sharpe Ratio
    # Annualized Sharpe = (mean_return - rf_per_trade) / std_return * sqrt(trades_per_year)
    mean_return = float(np.mean(returns)) if len(returns) > 0 else 0.0
    # Daily equivalent trades assumed at typical hunt rate ~10 trades/day
    trades_per_year = 252 * 10
    rf_per_trade = (1.0 + risk_free_rate) ** (1.0 / trades_per_year) - 1.0

    if trade_vol > 1e-8:
        sharpe_ratio = float((mean_return - rf_per_trade) / trade_vol * np.sqrt(trades_per_year))
    else:
        sharpe_ratio = 0.0

    # 4. Win Rate and Profit Factor
    winning_trades = pnls[pnls > 0]
    losing_trades = pnls[pnls < 0]
    win_rate = float(len(winning_trades) / len(pnls) * 100.0) if len(pnls) > 0 else 0.0

    gross_profit = float(np.sum(winning_trades)) if len(winning_trades) > 0 else 0.0
    gross_loss = float(np.abs(np.sum(losing_trades))) if len(losing_trades) > 0 else 0.0

    if gross_loss > 1e-8:
        profit_factor = float(gross_profit / gross_loss)
    elif gross_profit > 0:
        profit_factor = float("inf")
    else:
        profit_factor = 0.0

    return {
        "total_trades": len(trades),
        "completed_trades": len(completed_trades),
        "win_rate": round(win_rate, 2),
        "total_pnl": round(float(np.sum(pnls)), 2),
        "total_fees_usd": round(total_fees_usd, 2),
        "sharpe_ratio": round(sharpe_ratio, 2),
        "max_drawdown_pct": round(max_drawdown_pct, 2),
        "max_drawdown_dollars": round(max_drawdown_dollars, 2),
        "annualized_volatility": round(annualized_vol * 100.0, 2),
        "avg_pnl_per_trade": round(float(np.mean(pnls)), 2) if len(pnls) > 0 else 0.0,
        "profit_factor": round(profit_factor, 2) if profit_factor != float("inf") else 999.0,
        "cumulative_pnl_series": [round(float(x), 2) for x in cumulative_pnl],
        "drawdown_series": [round(float(x), 2) for x in drawdown_pct_series],
    }
