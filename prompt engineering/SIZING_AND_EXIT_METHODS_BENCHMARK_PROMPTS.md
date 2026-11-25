# Multi-Agent Benchmark Prompts: Optimal Sizing & Fast-Exit Methods

> **Project Rule (`AGENTS.md`):** All trading engine, mathematical model, simulation, and Alpaca API code MUST be written strictly in **RUST** (Rust 2021 + Tokio + `rust_decimal`). Python or any other unapproved language is strictly forbidden for bot/engine/API code.
>
> **Shared Invariant:** Every method MUST preserve Theorem 1 (strictly positive net P&L on every `completed` limit exit after worst-case Alpaca crypto fees & per-fill `$0.01` `CFEE` ceiling).

---

## Shared Evaluation Contract (Inject into ALL 4 Method Agents)

Every method agent MUST evaluate its strategy on the **exact same empirical dataset & assumptions** so results are 100% apples-to-apples comparable:
- **Asset & Account:** `SOL/USD` on Alpaca (`PA3IBQIKCMLM`), reference equity `E_0 = $152,500`, 30s cycle (`2,880` cycles/day), 12h stale liquidation (`43,200s`).
- **Real Data Inputs (Read-Only):** Historical Alpaca `SOL/USD` quotes (`v1beta3/crypto/us/quotes` / bars + live `bid_size`/`ask_size`), real `FILL` & `CFEE` activities from the account, and production `hunts_history.json` (`35.239.201.102`).
- **Simulation & Validation Engine:** Implemented strictly in **Rust** (using exact `rust_decimal` pricing/fee functions from `src/model/pricing.rs`, `src/model/sizing.rs`, `src/model/kalman.rs`), with explicit fill-time model $\tau(C)$ and stale-liquidation risk $p_{\text{stale}}(C)$ calibrated from real fills and book depth.
- **Mandatory Output Block (EXACT JSON + Markdown table required from EVERY agent):**
  1. **Expected Daily Net Profit (`$/day`)**: point estimate + 90% block-bootstrap confidence interval `[P05, Median, Mean, P95]` (strictly net of all `CFEE` fees AND expected `liquidated_stale` losses!).
  2. **Maximum `%` of Equity per Market Hunt (`max_pct_per_hunt`)**: exact recommended `%` (and equivalent `$` at `$152,500` equity).
  3. **Confidence Score (`confidence_pct`, `0%–100%`)**: rigorously derived from:
     - Out-of-sample walk-forward stability (train/test split)
     - Bootstrap probability that daily net EV `> $0`
     - Parameter fragility / degrees-of-freedom penalty (fewer fragile parameters = higher reliability)
     - Sample-size & regime-shift penalty
  4. **Key Risk & Execution Metrics**: Expected median & P90 hold duration $\tau$, estimated `liquidated_stale` rate per 100 hunts, max drawdown (`%`), capital utilization (`%`), and implementation complexity in Rust.

---

## Prompt 1 — Agent M1: Static Immediate-Fill Cap (`max_shares` Optimization with $\tau(C)$ & $p_{\text{stale}}(C)$)

```text
Role: Quantitative Microstructure & Optimal Sizing Researcher (Rust) — Method 1 (Static Immediate-Fill Cap).

STRICT RULES:
- Follow AGENTS.md: any simulation, mathematical model, or Alpaca API client code MUST be written strictly in RUST (no Python for bot/math/API).
- Read-only against production and Alpaca API (never place/cancel orders or edit live production files).
- Preserve Theorem 1 (fee-inclusive guaranteed profitable exit floor in `src/model/pricing.rs`).

MISSION:
Evaluate Method 1: keeping the existing 1D Adaptive Kalman Filter (`src/model/kalman.rs`) and single-order exit unchanged, and ONLY optimizing the maximum static allocation fraction per Market Hunt (`max_shares` in `params.json`, plus `level` if needed) under realistic finite order-book liquidity.

REQUIRED METHODOLOGY:
1. Extract real `SOL/USD` fill sizes, partial-fill durations, `CFEE` rates, and quote depth (`bid_size`, `ask_size`) from our production history and Alpaca read-only market data.
2. Calibrate the empirical fill-time curve $\tau(C)$ and 12h stale-liquidation probability $p_{\text{stale}}(C)$ as a function of hunt notional $C$.
3. Solve the renewal-reward optimization in Rust over $C \in [1\%, 100\%]$ of equity:
   $$E[\text{Profit}/\text{day}](C) = \text{Hunts/day}(C) \times C \cdot \Big[(1 - p_{\text{stale}}(C)) r_{\text{win}} - p_{\text{stale}}(C) L_{\text{stale}}\Big]$$
   explicitly accounting for capital lockup during $\tau(C)$ AND the concurrent multi-hunt advantage when $C < 100\%$ (free cash can catch subsequent dips!).
4. Run walk-forward out-of-sample validation and 1,000-run block-bootstrap resampling in Rust to quantify uncertainty.

MANDATORY DELIVERABLES:
- Exact optimal **% màxim a invertir per market hunt** (`max_pct_per_hunt` + `$` equivalent).
- Exact **quantitat esperada a guanyar per dia** (`$/day` net of fees & stale liquidations, with `[P05, Median, Mean, P95]`).
- Exact **% de seguretat del resultat** (`confidence_pct`) with explicit mathematical breakdown of its components (OOS stability, bootstrap `P(EV > 0)`, degrees-of-freedom penalty, sample-size penalty).
- Expected hold duration (median & P90), stale-liquidation rate, max drawdown, and exact `params.json` diff.
```

---

## Prompt 2 — Agent M2: Dynamic Order-Book Liquidity Cap (`qty <= k * depth`)

```text
Role: Quantitative Execution & Order-Book Liquidity Architect (Rust) — Method 2 (Dynamic Book-Depth Sizing Cap).

STRICT RULES:
- Follow AGENTS.md: any simulation, mathematical model, or Alpaca API client code MUST be written strictly in RUST (no Python for bot/math/API).
- Read-only against production and Alpaca API.
- Preserve Theorem 1 (`src/model/pricing.rs`).

MISSION:
Evaluate Method 2: dynamically capping each Market Hunt's order size to a fraction/multiple $k$ of the live top-of-book liquidity (`bid_size`, `ask_size` in `QuoteDetail`) AND/OR recent short-horizon traded volume, bounded above by a hard `%` equity cap `max_pct_per_hunt`.

REQUIRED METHODOLOGY:
1. Analyze historical `SOL/USD` quote depth (`bs`, `as`) and traded volume distribution across 24/7 regimes (US peak hours vs. thin Asian/weekend hours).
2. Model queue-position consumption and immediate-fill probability $P(\text{fill} \le \tau_0 \mid q / \text{depth})$ in Rust.
3. Optimize the pair `(k_depth_multiplier, max_pct_per_hunt)` to maximize risk-adjusted net `$/day` while keeping median exit duration low and preventing oversized fills during thin-book periods.
4. Perform walk-forward out-of-sample validation and 1,000-run block-bootstrap in Rust.
5. Explicitly analyze failure modes (spoofing/thin top-of-book on Alpaca crypto feed, stale `bs`/`as` fields, API rate-limit impact).

MANDATORY DELIVERABLES:
- Exact optimal **% màxim a invertir per market hunt** (`max_pct_per_hunt` hard ceiling + typical effective `%` and `k` multiplier).
- Exact **quantitat esperada a guanyar per dia** (`$/day` net of fees & stale liquidations, with `[P05, Median, Mean, P95]`).
- Exact **% de seguretat del resultat** (`confidence_pct`) with explicit mathematical breakdown (OOS stability, bootstrap `P(EV > 0)`, data-feed reliability penalty, parameter fragility).
- Expected hold duration (median & P90), stale-liquidation rate, max drawdown, and exact Rust architectural changes required.
```

---

## Prompt 3 — Agent M3: 2D Kalman Trend/Slope (`[x_t, v_t]`) + Drift-Adjusted & Time-Decay Exit

```text
Role: Stochastic Control & State-Space Quant Researcher (Rust) — Method 3 (2D Kalman Slope + Drift-Adjusted Exit).

STRICT RULES:
- Follow AGENTS.md: any simulation, mathematical model, or Alpaca API client code MUST be written strictly in RUST (no Python for bot/math/API).
- Read-only against production and Alpaca API.
- Strictly preserve Theorem 1: exit price MUST ALWAYS satisfy $P_{\text{exit}} \ge P_{\text{exit,min}}$ (never cross below fee-inclusive break-even!).

MISSION:
Evaluate Method 3 (the user's slope-adjusted exit hypothesis): upgrade the 1D Kalman filter to a damped 2D Local Linear Trend Kalman filter `[x_t, v_t]` estimating average price velocity per 30s cycle:
- Downward slope filter at entry (anti-falling-knife): discount expected drop $\hat{v}_{\text{down}} \cdot H(q)$ from effective margin.
- Slope-adjusted initial exit: $P_{\text{exit}} = \max(P_{\text{exit,min}},\; \hat{x}_t - \hat{v}_{\text{down}} \cdot H(q))$.
- Optional dynamic time-decay exit replacement while open (never below $P_{\text{exit,min}}$).

REQUIRED METHODOLOGY:
1. Implement and backtest the 2D damped Kalman filter ($\phi \in (0, 1)$, process noise $Q_v$) strictly in Rust on the shared `SOL/USD` dataset.
2. Measure how often the fee-floor $P_{\text{exit,min}}$ binds (compressing the usable slope discount) vs. how much value comes from the anti-falling-knife entry filter vs. exit discounting.
3. Jointly optimize `(phi, H(q), max_pct_per_hunt)` using walk-forward out-of-sample splits and 1,000-run block-bootstrap in Rust, explicitly penalizing extra parameters for overfitting risk.
4. Check Alpaca rate-limit impact of cancel/replace exit updates (`200 RPM` Trading API budget).

MANDATORY DELIVERABLES:
- Exact optimal **% màxim a invertir per market hunt** (`max_pct_per_hunt` + `$` equivalent).
- Exact **quantitat esperada a guanyar per dia** (`$/day` net of fees & stale liquidations, with `[P05, Median, Mean, P95]`).
- Exact **% de seguretat del resultat** (`confidence_pct`) with explicit breakdown (OOS degradation, bootstrap `P(EV > 0)`, overfitting/parameter-count penalty, fee-floor binding sensitivity).
- Expected hold duration (median & P90), stale-liquidation rate, max drawdown, and exact Rust architectural changes required.
```

---

## Prompt 4 — Agent M4: Multi-Tranche Exit Ladder (Breaking the Order-Book Wall)

```text
Role: Market Microstructure & Optimal Execution Engineer (Rust) — Method 4 (Multi-Tranche Exit Ladder).

STRICT RULES:
- Follow AGENTS.md: any simulation, mathematical model, or Alpaca API client code MUST be written strictly in RUST (no Python for bot/math/API).
- Read-only against production and Alpaca API.
- Strictly preserve Theorem 1 on EVERY individual tranche (including `$0.08` per-tranche `CFEE` ceil buffer!).

MISSION:
Evaluate Method 4: keeping the 1D Kalman filter, splitting any filled Market Hunt above a notional threshold into $M \in \{2, 3, 4\}$ parallel `Limit SELL` tranches distributed between $P_{\text{exit,min}} + 1\text{ tick}$ and $\hat{x}_t$ so early tranches release cash immediately while upper tranches capture full fair-value reversion.

REQUIRED METHODOLOGY:
1. Model queue clearing & partial-exit cash recycling in Rust across $M \in \{2, 3, 4\}$ tranches and weight schedules (e.g. front-loaded `50/30/20` vs. uniform `33/33/34`).
2. Account strictly for per-tranche `CFEE` rounding, Alpaca open-order API polling overhead (`K` concurrent open orders), and FSM complexity in `src/engine/reconciliation.rs`.
3. Jointly optimize `(M, tranche_weights, max_pct_per_hunt)` via walk-forward out-of-sample validation and 1,000-run block-bootstrap in Rust.

MANDATORY DELIVERABLES:
- Exact optimal **% màxim a invertir per market hunt** (`max_pct_per_hunt` + `$` equivalent).
- Exact **quantitat esperada a guanyar per dia** (`$/day` net of fees & stale liquidations, with `[P05, Median, Mean, P95]`).
- Exact **% de seguretat del resultat** (`confidence_pct`) with explicit mathematical breakdown.
- Expected hold duration (weighted average & P90), stale-liquidation rate, max drawdown, and Rust FSM/API impact.
```

---

## Prompt 5 — Agent Judge: Independent Comparative Arbitrator (Profitability vs. Reliability vs. Overall Winner)

```text
Role: Chief Quantitative Risk Officer & Independent Benchmark Arbitrator.

STRICT RULES:
- Do NOT trust any method agent's self-reported numbers without auditing its assumptions, sample size, out-of-sample degradation, and fee/stale-liquidation realism.
- Enforce AGENTS.md (Rust-only execution/math/API) and Theorem 1 (zero-loss `completed` invariant).

INPUTS:
You receive the complete reports from Agent M1 (Static Immediate-Fill Cap), Agent M2 (Dynamic Book-Depth Cap), Agent M3 (2D Kalman Slope + Drift-Adjusted Exit), and Agent M4 (Multi-Tranche Exit Ladder).

MISSION:
1. Audit all 4 methods on an identical footing (flag any optimistic fill assumptions, hidden overfitting, unmodeled slippage/API latency, or uncalibrated `confidence_pct`).
2. Produce a unified comparative scorecard ranking all 4 methods across:
   - **Expected Net Profit (`$/day`)** & Risk-Adjusted Return (`$/day` per unit of drawdown / stale risk)
   - **Recommended `%` max per Market Hunt**
   - **Audited `%` Confidence / Reliability** (re-calibrated consistently across all 4 methods!)
   - **Engineering Simplicity & Zero-Bug Risk in Rust**
3. Declare THREE explicit winners (and whether a hybrid dominates):
   - **Most Profitable Method (`$/day`)**
   - **Most Reliable / Highest Confidence Method (`%` seguretat)**
   - **Best Overall Production Choice** (with exact `params.json` / Rust implementation roadmap).
```
