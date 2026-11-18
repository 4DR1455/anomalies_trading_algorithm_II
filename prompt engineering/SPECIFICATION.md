# Mean Reversion Trading System with Hybrid Execution & Adaptive Capital Allocation

**A resilient, low-latency trading engine designed to autonomously detect and exploit market price anomalies on equity markets (Alpaca API).**

---

## 1. System Vision & Trading Logic

### 1.1 Market Cycle (Every 30 Seconds)
Every 30 seconds, the system samples the market price/quotes and compares them against a theoretical fair-price prediction model.

1. **Market Entry ("HiddenMM"):**
   - When an undervaluation anomaly is detected (market price is below predicted value), the bot places a limit BUY order slightly better than the current market Best Bid (e.g., Best Bid + $0.01 or +0.01%), aiming to capture **maker fee priority** while acting as an aggressive liquidity taker.
   - **SEC Compliance (Rule 612 Sub-Penny Rule):** Order pricing must strictly respect minimum tick sizes ($0.01 for equities over $1.00) to ensure zero rejections by Alpaca. The implementation agent will optimize this exact pricing formula based on the findings of the research agent.
   - The bot waits up to **20 seconds** for fill.
   - If unfilled after 20 seconds, cancel the remaining unfilled shares immediately.

2. **Market Exit:**
   - Immediately place a limit SELL order for the exact amount of filled shares at or slightly below the predicted value to lock in profits and recover liquidity.
   - *A complete HiddenMM entry + market exit cycle constitutes a "Market Hunt".*

3. **Risk Management & Order Hygiene:**
   - Check all open/pending orders. If any order remains open for more than **12 hours**, cancel and liquidate at market price (avoiding overnight risk).
   - If an order was pulled down or dropped unexpectedly, evaluate and re-raise it if the anomaly persists.

---

## 2. Decision Algorithm & Capital Allocation

### 2.1 Dynamic Position Sizing (Asymptotic Formula)
Position sizes are not static lots. The allocation scales dynamically according to the normalized anomaly ratio:

$$SharesAmount(x) = Max \times \left(1 - (1 - x^{level})^{\frac{1}{level}}\right)$$

*Where:*
* $x$: Normalized deviation ratio ($\frac{CurrentRange}{MaxRange}$), with $0 \leq x \leq 1$.
* $Max$: Maximum shares permitted per trade (`max_shares`).
* $level$: Convexity/aggressiveness parameter (`level` in `params.json`).

### 2.2 Parameters Configuration (`params.json`)
All strategy knobs must be dynamically configurable via `params.json`:
* `symbol`: Equity ticker (e.g., `"SPY"`).
* `level`: Non-linear convexity parameter for position sizing.
* `max_shares`: Maximum lot limit per trade.
* `min_margin`: Minimum anomaly threshold to trigger an entry signal.
* `max_margin` (**Circuit Breaker / Volatility Ceiling**): If the detected price deviation exceeds `max_margin`, the market is deemed abnormally erratic (e.g., flash crash or earnings dump). **Trading must halt immediately** to prevent catching falling knives.
* **Model-specific Hyperparameters:** Only the parameters corresponding to the chosen model should be included (e.g., `alpha` only if an EMA is selected; mean-reversion speed $\theta$ / variance if an Ornstein-Uhlenbeck or Kalman model is chosen).

---

## 3. Directives for Autonomous AI Agents

> [!IMPORTANT]
> The developing agents are NOT constrained to rigid technology choices. They are explicitly tasked to explore, brainstorm, and select the optimal mathematical models and tech stack.

### Directive A: Open Exploration of Mathematical Models
* Do not blindly settle on a plain Exponential Moving Average (EMA).
* The research agent must survey quantitative finance literature for short-horizon (30s) mean-reversion models (e.g., **Ornstein-Uhlenbeck stochastic processes**, **Adaptive Kalman Filters**, or exponential smoothing variants).
* Select and justify the model that provides the highest signal-to-noise ratio with minimal lag. The chosen model dictates the hyperparameters in `params.json`.

### Directive B: Open Exploration of Language & Architecture
* C++ (Engine) and OCaml (Brain) were initial concepts, but the architecture must be evaluated freely:
  * Would a unified **Rust** engine (with lock-free channels and memory safety) outperform multi-process IPC?
  * Would a **C++ / Haskell** hybrid offer superior mathematical provability and resilience?
  * Would an **Erlang/OTP-style** supervisor tree offer the ultimate crash-proof uptime?
* The agent must choose and justify the combination that guarantees **hyper-persistence**, **zero memory leaks**, and **zero unhandled crashes**.

### Directive C: Ultra-Efficient IPC Protocol (If Multi-Process)
* If a multi-process architecture is chosen, generic JSON over pipes on every tick is discouraged.
* The agent must design the most lightweight, zero-copy or minimal-overhead protocol (e.g., fixed-width binary structs or flat delimited frames) over Linux pipes.

### Directive D: Microstructure & Legal Execution Optimization
* The research agent will investigate Alpaca's order book quoting conventions and SEC Rule 612 constraints.
* The implementation agent will optimize the pricing offset to maximize queue priority at the best legal bid.

---

## 4. Mandatory Hard Constraints

1. **Autonomous Operation:** The system must run end-to-end without manual intervention, handling partial fills and API timeouts gracefully.
2. **Alpaca API:** Must interact with `alpaca.markets`. Credentials must be loaded strictly from environment variables via `.env`.
3. **Isolated Docker Container:** The entire trading engine must run inside a hardened Docker container under a non-root user.
4. **Strict Network Isolation:** **No ports shall be exposed to the public internet.**
5. **Local Reporting Dashboard:** Performance metrics (**Sharpe Ratio, Max Drawdown, Volatility**, P&L history) must be calculated using Python (`numpy`) and displayed via a local-only HTML file or localhost-bound interface (`127.0.0.1`).
6. **Modular Clean Code:** Code MUST be decomposed into small, cohesive, single-responsibility files. Monolithic files are strictly forbidden.
