use crate::alpaca::PortfolioHistory;
use crate::model::pricing::decimal_to_f64;
use crate::state::TradeRecord;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Exact JSON schema parity with `dashboard/metrics.py` (`compute_performance_metrics`
/// and `compute_portfolio_history_metrics`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PerformanceMetrics {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_date: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_equity: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub current_equity: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_return_pct: Option<f64>,
    pub total_trades: usize,
    pub completed_trades: usize,
    pub win_rate: f64,
    pub total_pnl: f64,
    pub total_fees_usd: f64,
    pub sharpe_ratio: f64,
    pub max_drawdown_pct: f64,
    pub max_drawdown_dollars: f64,
    pub annualized_volatility: f64,
    pub avg_pnl_per_trade: f64,
    pub profit_factor: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub chart_labels: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub equity_series: Option<Vec<f64>>,
    pub cumulative_pnl_series: Vec<f64>,
    pub drawdown_series: Vec<f64>,
    pub timestamp: String,
}

fn round2(val: f64) -> f64 {
    if !val.is_finite() {
        return 0.0;
    }
    let r = (val * 100.0).round() / 100.0;
    if r == 0.0 {
        0.0
    } else {
        r
    }
}

fn sample_std_ddof1(values: &[f64]) -> f64 {
    let n = values.len();
    if n <= 1 {
        return 0.0;
    }
    let mean = values.iter().sum::<f64>() / (n as f64);
    let var = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / ((n - 1) as f64);
    var.max(0.0).sqrt()
}

/// Computes quantitative trade-based performance metrics matching `dashboard/metrics.py`.
pub fn compute_performance_metrics(
    trades: &[TradeRecord],
    risk_free_rate: f64,
    base_equity: f64,
    now_utc: DateTime<Utc>,
) -> PerformanceMetrics {
    let completed: Vec<&TradeRecord> = trades
        .iter()
        .filter(|t| t.pnl.is_some() && t.status.is_closed_strategy_trade())
        .collect();

    if completed.is_empty() {
        return PerformanceMetrics {
            source: None,
            start_date: None,
            start_equity: None,
            current_equity: None,
            total_return_pct: None,
            total_trades: trades.len(),
            completed_trades: 0,
            win_rate: 0.0,
            total_pnl: 0.0,
            total_fees_usd: 0.0,
            sharpe_ratio: 0.0,
            max_drawdown_pct: 0.0,
            max_drawdown_dollars: 0.0,
            annualized_volatility: 0.0,
            avg_pnl_per_trade: 0.0,
            profit_factor: 0.0,
            chart_labels: None,
            equity_series: None,
            cumulative_pnl_series: vec![0.0],
            drawdown_series: vec![0.0],
            timestamp: now_utc.to_rfc3339(),
        };
    }

    let pnls: Vec<f64> = completed
        .iter()
        .map(|t| t.pnl.map(decimal_to_f64).unwrap_or(0.0))
        .collect();
    let returns: Vec<f64> = completed
        .iter()
        .map(|t| t.pnl_pct.map(decimal_to_f64).unwrap_or(0.0))
        .collect();
    let total_fees_usd: f64 = completed
        .iter()
        .map(|t| t.fees_usd.map(decimal_to_f64).unwrap_or(0.0))
        .sum();

    let safe_base = if base_equity > 0.0 && base_equity.is_finite() {
        base_equity
    } else {
        100_000.0
    };

    let mut cumulative_pnl = Vec::with_capacity(pnls.len());
    let mut drawdown_pct_series = Vec::with_capacity(pnls.len());
    let mut running_pnl = 0.0;
    let mut peak_pnl = f64::NEG_INFINITY;
    let mut peak_equity = f64::NEG_INFINITY;
    let mut max_dd_dollars = 0.0_f64;
    let mut max_dd_pct = 0.0_f64;

    for &p in &pnls {
        running_pnl += p;
        cumulative_pnl.push(running_pnl);
        if running_pnl > peak_pnl {
            peak_pnl = running_pnl;
        }
        let dd_d = peak_pnl - running_pnl;
        if dd_d > max_dd_dollars {
            max_dd_dollars = dd_d;
        }

        let eq = safe_base + running_pnl;
        if eq > peak_equity {
            peak_equity = eq;
        }
        let dd_pct = if peak_equity > 0.0 {
            (peak_equity - eq) / peak_equity * 100.0
        } else {
            0.0
        };
        if dd_pct > max_dd_pct {
            max_dd_pct = dd_pct;
        }
        drawdown_pct_series.push(dd_pct);
    }

    let trade_vol = sample_std_ddof1(&returns);
    let annualized_vol = trade_vol * 252.0_f64.sqrt();

    let mean_return = returns.iter().sum::<f64>() / (returns.len() as f64);
    let trades_per_year = 252.0 * 10.0;
    let rf_per_trade = (1.0 + risk_free_rate).powf(1.0 / trades_per_year) - 1.0;
    let sharpe_ratio = if trade_vol > 1e-8 {
        (mean_return - rf_per_trade) / trade_vol * trades_per_year.sqrt()
    } else {
        0.0
    };

    let winning_count = pnls.iter().filter(|&&p| p > 0.0).count();
    let win_rate = (winning_count as f64) / (pnls.len() as f64) * 100.0;

    let gross_profit: f64 = pnls.iter().filter(|&&p| p > 0.0).sum();
    let gross_loss: f64 = pnls.iter().filter(|&&p| p < 0.0).sum::<f64>().abs();
    let profit_factor = if gross_loss > 1e-8 {
        gross_profit / gross_loss
    } else if gross_profit > 0.0 {
        999.0
    } else {
        0.0
    };

    let total_pnl: f64 = pnls.iter().sum();
    let avg_pnl = total_pnl / (pnls.len() as f64);

    PerformanceMetrics {
        source: None,
        start_date: None,
        start_equity: None,
        current_equity: None,
        total_return_pct: None,
        total_trades: trades.len(),
        completed_trades: completed.len(),
        win_rate: round2(win_rate),
        total_pnl: round2(total_pnl),
        total_fees_usd: round2(total_fees_usd),
        sharpe_ratio: round2(sharpe_ratio),
        max_drawdown_pct: round2(max_dd_pct),
        max_drawdown_dollars: round2(max_dd_dollars),
        annualized_volatility: round2(annualized_vol * 100.0),
        avg_pnl_per_trade: round2(avg_pnl),
        profit_factor: round2(profit_factor),
        chart_labels: None,
        equity_series: None,
        cumulative_pnl_series: cumulative_pnl.into_iter().map(round2).collect(),
        drawdown_series: drawdown_pct_series.into_iter().map(round2).collect(),
        timestamp: now_utc.to_rfc3339(),
    }
}

/// Computes official Alpaca portfolio equity curve metrics matching
/// `compute_portfolio_history_metrics` in `dashboard/metrics.py`.
pub fn compute_portfolio_history_metrics(
    portfolio_history: &PortfolioHistory,
    live_equity: Option<f64>,
    trades: &[TradeRecord],
    risk_free_rate: f64,
    now_utc: DateTime<Utc>,
) -> PerformanceMetrics {
    let mut points: Vec<(String, f64)> = Vec::new();

    for (&ts, &eq_opt) in portfolio_history
        .timestamp
        .iter()
        .zip(portfolio_history.equity.iter())
    {
        if let Some(eq) = eq_opt {
            if eq.is_finite() && eq > 0.0 {
                if let Some(dt) = DateTime::<Utc>::from_timestamp(ts, 0) {
                    points.push((dt.format("%Y-%m-%d").to_string(), eq));
                }
            }
        }
    }

    if let Some(live_eq) = live_equity {
        if live_eq.is_finite() && live_eq > 0.0 {
            let today_str = now_utc.format("%Y-%m-%d").to_string();
            if let Some(last) = points.last_mut() {
                if last.0 == today_str {
                    last.1 = live_eq;
                } else {
                    points.push((today_str, live_eq));
                }
            } else {
                points.push((today_str, live_eq));
            }
        }
    }

    let trade_metrics = compute_performance_metrics(trades, risk_free_rate, 100_000.0, now_utc);

    if points.is_empty() {
        return trade_metrics;
    }

    let labels: Vec<String> = points.iter().map(|(d, _)| d.clone()).collect();
    let eq_arr: Vec<f64> = points.iter().map(|(_, eq)| *eq).collect();
    let start_equity = eq_arr[0];
    let current_equity = *eq_arr.last().unwrap_or(&start_equity);

    let cum_pnl: Vec<f64> = eq_arr.iter().map(|&e| e - start_equity).collect();
    let total_pnl = current_equity - start_equity;
    let total_return_pct = if start_equity > 0.0 {
        total_pnl / start_equity * 100.0
    } else {
        0.0
    };

    let mut peak = f64::NEG_INFINITY;
    let mut dd_pct_series = Vec::with_capacity(eq_arr.len());
    let mut max_dd_dollars = 0.0_f64;
    let mut max_dd_pct = 0.0_f64;

    for &eq in &eq_arr {
        if eq > peak {
            peak = eq;
        }
        let dd_d = peak - eq;
        if dd_d > max_dd_dollars {
            max_dd_dollars = dd_d;
        }
        let dd_p = if peak > 0.0 {
            (peak - eq) / peak * 100.0
        } else {
            0.0
        };
        if dd_p > max_dd_pct {
            max_dd_pct = dd_p;
        }
        dd_pct_series.push(dd_p);
    }

    let (annualized_vol_pct, sharpe_ratio) = if eq_arr.len() > 1 {
        let daily_returns: Vec<f64> = eq_arr
            .windows(2)
            .filter_map(|w| {
                if w[0] > 0.0 {
                    Some((w[1] - w[0]) / w[0])
                } else {
                    None
                }
            })
            .collect();
        let daily_vol = sample_std_ddof1(&daily_returns);
        let ann_vol = daily_vol * 365.0_f64.sqrt() * 100.0;
        let rf_daily = risk_free_rate / 365.0;
        let mean_ret = if daily_returns.is_empty() {
            0.0
        } else {
            daily_returns.iter().sum::<f64>() / (daily_returns.len() as f64)
        };
        let sharpe = if daily_vol > 1e-10 {
            (mean_ret - rf_daily) / daily_vol * 365.0_f64.sqrt()
        } else {
            0.0
        };
        (ann_vol, sharpe)
    } else {
        (0.0, 0.0)
    };

    PerformanceMetrics {
        source: Some("alpaca_portfolio_history".to_string()),
        start_date: Some(labels[0].clone()),
        start_equity: Some(round2(start_equity)),
        current_equity: Some(round2(current_equity)),
        total_return_pct: Some(round2(total_return_pct)),
        total_trades: trade_metrics.total_trades,
        completed_trades: trade_metrics.completed_trades,
        win_rate: trade_metrics.win_rate,
        total_pnl: round2(total_pnl),
        total_fees_usd: trade_metrics.total_fees_usd,
        sharpe_ratio: round2(sharpe_ratio),
        max_drawdown_pct: round2(max_dd_pct),
        max_drawdown_dollars: round2(max_dd_dollars),
        annualized_volatility: round2(annualized_vol_pct),
        avg_pnl_per_trade: trade_metrics.avg_pnl_per_trade,
        profit_factor: trade_metrics.profit_factor,
        chart_labels: Some(labels),
        equity_series: Some(eq_arr.into_iter().map(round2).collect()),
        cumulative_pnl_series: cum_pnl.into_iter().map(round2).collect(),
        drawdown_series: dd_pct_series.into_iter().map(round2).collect(),
        timestamp: now_utc.to_rfc3339(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_portfolio_history_metrics_exact_parity_with_python() {
        let ph = PortfolioHistory {
            timestamp: vec![
                1770595200, // 2026-02-09 (pre-funding 0)
                1770681600, // 2026-02-10 ($100,000)
                1770768000, // 2026-02-11 ($120,000)
                1770854400, // 2026-02-12 ($108,000 -> -10% DD / -$12,000)
                1770940800, // 2026-02-13 ($150,000)
            ],
            equity: vec![
                Some(0.0),
                Some(100_000.0),
                Some(120_000.0),
                Some(108_000.0),
                Some(150_000.0),
            ],
            ..Default::default()
        };

        let m = compute_portfolio_history_metrics(&ph, None, &[], 0.04, Utc::now());
        assert_eq!(m.source.as_deref(), Some("alpaca_portfolio_history"));
        assert_eq!(m.start_date.as_deref(), Some("2026-02-10"));
        assert_eq!(m.start_equity, Some(100_000.0));
        assert_eq!(m.current_equity, Some(150_000.0));
        assert_eq!(m.total_pnl, 50_000.0);
        assert_eq!(m.total_return_pct, Some(50.0));
        assert_eq!(m.max_drawdown_pct, 10.0);
        assert_eq!(m.max_drawdown_dollars, 12_000.0);
        assert_eq!(
            m.cumulative_pnl_series,
            vec![0.0, 20_000.0, 8_000.0, 50_000.0]
        );
        assert_eq!(
            m.chart_labels,
            Some(vec![
                "2026-02-10".to_string(),
                "2026-02-11".to_string(),
                "2026-02-12".to_string(),
                "2026-02-13".to_string(),
            ])
        );
        assert!(m.sharpe_ratio > 0.0);
    }
}
