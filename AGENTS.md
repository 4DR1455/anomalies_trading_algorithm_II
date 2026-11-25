# Project Engineering Rules — Anomalies Trading Algorithm v2.1

## Mandatory Language Policy (Strictly Enforced)

1. **Strictly Prohibited:** It is **strictly forbidden** to write, recreate, migrate, or modify **any** trading bot operational code — including the mathematical model/engine (Kalman filter, sizing, pricing, fee modeling, optimization runtime), order execution, state reconciliation, or the Alpaca API interface — in **Python** or any other language not explicitly approved by the project owner.
2. **Explicitly Approved Languages for Trading Engine & API (as of 2026-11-25):**
   - **RUST** (Rust 2021 + Tokio + `rust_decimal`) — **ONLY approved language** for all bot execution, mathematical engine, and broker API code (`src/`).
3. **Legacy Python Files (`live_bot.py`, `persistence.py`):**
   - Treat legacy Python trading scripts as **deprecated/frozen**. Never add features, fixes, or new trading/API logic to them, and never deploy them to production.
4. **Scope Clarification (Allowed Outside Operational Bot Code):**
   - **Offline Research, Analysis & Exploratory Backtesting:** Any language that fits best (e.g., Python/NumPy/SciPy, Rust, etc.) is **explicitly allowed** for research, data exploration, statistical modeling, and offline simulations, as long as it does **not** run in production or affect the live bot's operational behavior.
   - **Read-Only Web Dashboard:** `dashboard/app.py` (strictly read-only JSON consumer with zero broker/API/trading logic) remains separate unless explicitly migrated.
   - **Rule of Thumb:** If it executes trades, computes live signals/orders/sizing, reconciles positions, or talks to Alpaca for trading operations → **RUST ONLY**. If it is purely offline research/analytics that does not touch live bot execution → **best tool for the job**.
5. **Amendment Rule:**
   - Adding any language other than **RUST** to the approved list for bot/engine/API code requires explicit written approval from the user.
