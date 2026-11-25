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

---

## Mandatory Branch-First Workflow Policy (Strictly Enforced)

1. **Same Scope as the Rust Policy:** Applies to **every** agent (main agent and all subagents) modifying any file that affects the trading bot's operational behavior (`src/`, `Cargo.toml`, `params.json`, `Dockerfile`, or any runtime/API/model/execution logic).
2. **Never Edit `main` Directly for Operational Changes:** Before making any operational code or parameter edits, the agent **must** create and work on a dedicated Git branch (e.g., `feat/<topic>`, `fix/<topic>`, `refactor/<topic>`, or an isolated branched workspace).
3. **Mandatory Testing & Validation on the Branch:** All changes must be compiled, linted (`cargo clippy --all-targets -- -D warnings`), tested (`RUSTFLAGS='-D warnings' cargo test`, including unit, integration, and formal verification suites), and verified working **on the branch** before integration.
4. **Merge Only After Validation Passes:** Merge the branch back into `main` (and deploy to production, if requested) **only after** all tests, lints, and mathematical/behavioral validations pass cleanly.
5. **Clean Up:** Remove temporary local feature branches after merging cleanly into `main`.

