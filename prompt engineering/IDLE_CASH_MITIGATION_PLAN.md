# Idle Cash Mitigation Strategy Catalog & Phased Plan (V2.0)

*Generated via 3-Agent Quantitative Collaboration (`web_quant_researcher` + `first_principles_ideator` + `strategy_synthesizer_architect`) on 2026-11-23.*

---

## 0. Structural Diagnosis: Why Cash Sits Idle ~95% of the Day

1. **Fee-Horizon Scaling Mismatch:** `AdaptiveKalmanLive` (`q_base=1.5e-7`, `r_base=4.0e-7`) has steady-state Kalman gain $K_\infty \approx 0.453$, giving a memory half-life of only **34.5 seconds**. Any smooth 3m–60m dip (`0.75%–1.80%`) is absorbed into $\hat{x}_t$ without firing, while Alpaca Crypto Tier 1 imposes a **`30.07 bps`** maker-maker round-trip wall (`0.15%` buy + `0.15%` sell, or `40.12 bps` if stale liquidation exits at `0.25%` taker).
2. **Nyquist Polling Loss (~85%):** Polling once per 30s and only *then* placing a 20s `HiddenMM` buy misses ~85% of transient 3–5s liquidity wicks and loses FIFO queue priority.
3. **Single-Asset & One-Sided Spot Blindness:** Monitoring only `SOL/USD` long-only fires ~4 times/day (~5% utilization) and ignores 50% of anomalies (upward spikes) plus wide spread vacuums.
4. **Doob's Optional Stopping Proof (Naive V1.0 Trap):** Buying blindly after $X$ idle hours with an upper limit sell $+U$ and a 12h time stop on a zero-drift martingale has $\mathbb{E}[\text{gross}] = 0$ and strictly negative net EV ($\approx -0.326\%$ per trade after Alpaca fees). To make V1.0 strictly $+EV$, entry requires a verified symmetry-breaking edge (positive micro-drift, verified stationary Variance Ratio chop, or passive discount + explicit stop-loss).

---

## 1. Agent 1 — 10 Existing Real-World Solutions (Web Research)

1. **Web #1 — Zero-Commission Spot Crypto ETF Proxies (`BSOL`, `GSOL`, `VSOL`, `IBIT`, `ETHA`) + Alpaca 24/5 BOATS Overnight Market:** Trade US-listed spot Solana/BTC/ETH ETFs at **`0.00%` commission** Sun–Fri (4x intraday margin, native staking yield pass-through in `BSOL`/`VSOL`), lowering profitable threshold from `~0.30%` to `~0.05%–0.10%`; fall back to 24/7 native `SOL/USD` on weekends.
2. **Web #2 — Idle-Cash Yield Sweep (`SGOV`, `BIL`, `BOXX` vs. Alpaca High-Yield Cash Program):** Earn ~3.5%–4.5% APY risk-free during idle periods. Keep crypto-ready buffer in Alpaca High-Yield Cash (instant settled `non_marginable_buying_power` for 24/7 crypto) and park excess reserve in `$0`-commission `SGOV`/`BOXX` (which also serves as 2x/4x Reg-T collateral for US equities).
3. **Web #3 — Avellaneda–Stoikov Inventory-Aware Market Making (with Hummingbot `add_transaction_costs`):** Continuous two-sided quoting around inventory-skewed reservation price $r = s - q\gamma\sigma^2(T-t)$, enforcing explicit fee floor $\delta_{\text{net}} \ge 2f_m + \text{edge}$.
4. **Web #4 — Guéant–Lehalle–Fernandez-Tapia (GLFT) Passive Resting Limit Ladders ("Liquidity Traps"):** HJB-optimal passive resting bid ladders permanently parked below fair value to catch sub-30s wicks at guaranteed maker fees (`0.15%` → `0.08%`), protected by toxicity pull-down guards.
5. **Web #5 — Shannon's Demon / Constant Proportion Rebalancing (CPR) & Geometric Grid Volatility Harvesting:** Dedicated 25%–40% sleeve running a geometric grid (step `0.60%–1.00%` on Tier 1 crypto, `0.20%–0.35%` on `$0`-fee ETFs) harvesting sideways chop continuously while reserving 60%–75% cash for major Kalman tail hunts.
6. **Web #6 — Multi-Asset Cross-Sectional Statistical Arbitrage & Kalman Pairs Trading (Avellaneda–Lee / Ernest Chan):** Expand breadth from 1 asset to $N=10\text{–}15$ crypto pairs (parallel dip hunting, accelerating Alpaca volume tiers) and/or `$0`-commission long/short US equity pairs (`NVDA/AMD`, `IBIT/MSTR`, `SPY/QQQ`).
7. **Web #7 — Leung–Li Optimal Double-Stopping & Volatility-Adaptive Thresholds (OU / ATR):** Fit Kalman residuals to an Ornstein–Uhlenbeck process $dX_t = \theta(\mu - X_t)dt + \sigma dW_t$ to compute dynamic entry, exit, and stop-loss boundaries maximizing expected profit *per unit of time* as a function of live fee rate $f_t$, reversion speed $\theta_t$, and volatility $\sigma_t$.
8. **Web #8 — Portable Alpha / Cash Equitization & Time-of-Day Equity Rotation:** Capture Overnight Drift (`SPY`/`QQQ` via MOC/MOO) + Intraday VWAP Mean Reversion using `$0`-commission 4x intraday margin (interest-free when flattened before 4:00 PM ET).
9. **Web #9 — Robert Carver's Multi-Strategy Forecast Blending & Continuous Volatility Targeting:** Convert rules into continuous scaled forecasts (`-20` to `+20`) blending fast mean-reversion (Kalman), medium/slow trend-following (EWMAC), and carry, buffered by an inertia no-trade band to control turnover.
10. **Web #10 — Alpaca Crypto Fee-Tier Volume Flywheel:** Alpaca's 8-tier trailing 30-day crypto volume schedule drops maker fees sharply: Tier 1 (`0.30%` RT), Tier 2 (`$100k+`: `0.24%` RT), Tier 3 (`$500k+`: `0.20%` RT), Tier 4 (`$1M+`: `0.16%` RT), Tier 5 (`$10M+`: `0.10%` RT). On `$167k` equity, 1 RT unlocks Tier 2 and just 3 RT/month unlock Tier 4—halving the entry fee barrier automatically.

---

## 2. Agent 2 — 10 Original Mechanisms (First Principles, Zero Web Search)

1. **FP #1 — Drift-Gated & Passive-Discounted Renewal Evolution of V1.0 ("Safe Artificial Deployment"):** Fixes naive V1.0 via a 2-state velocity Kalman `[x_t, v_t]`, variance-time idle clock $\Theta_{\text{idle}} = \sum S_k/\hat{x}_k^2$, double-barrier positive-EV gate (verified positive drift $\hat{v}_t/\sigma_v \ge 1.5$ or passive discount $\delta_b$ + explicit stop-loss $L^*$), and fractional probe sizing (`15%–30%`).
2. **FP #2 — Multi-Horizon Parallel Kalman Filter Bank (35s / 3m / 15m / 60m):** Runs 4 log-spaced `AdaptiveKalmanLive` scales ($\rho_j = q/r$) to harvest 3m–60m dips (`0.75%–1.60%`) gated by fast-scale deceleration (`anti-falling-knife`) and scale-matched timeouts/stops.
3. **FP #3 — Continuous Proactive "Kalman-Shadow" Resting Limit Ladder:** Permanently rests $K=3$ GTC passive buy traps below $\hat{x}_t$ with self-protecting toxicity offset $c_{\text{adv}}\sqrt{Q_t}/\hat{x}_t$ and asymmetric hysteresis repricing (instant pull-down if $\hat{x}_t$ drops; delayed upward chase to preserve FIFO priority).
4. **FP #4 — Cross-Asset Renewal Pooling & Idiosyncratic Beta-Stripped Priority Queue:** Erlang-$c$ shared cash pool across $N=8\text{–}12$ cryptos; strips cross-sectional median macro factor $F_t$ (`Macro-Crash Shield` blocks correlated market plunges) and ranks fills greedily by net return per unit holding time $\mathcal{I}_{i,t} = (\mu_{i,t} - C_{\text{RT},i} - c_{\text{vol}}\sqrt{Q_{i,t}}/\hat{x}_{i,t}) / \hat{h}_i$.
5. **FP #5 — Two-Regime Zero-Fee Equity / 24/7 Crypto Capital Router:** Routes `70%–85%` of capital via live expected net return rate $\rho_m(t)$ into `$0`-fee US equities during US hours (`09:35–15:50 ET`, flattening cleanly before close) and `100%` back to 24/7 crypto overnight and weekends.
6. **FP #6 — Bidirectional Inventory-Cushion Market Making with Kalman-Coupled Reservation Price:** Regime-gated base inventory cushion (`20%–35%` in neutral/bull trends, `0%` in bear trends) + reservation price coupled to live Kalman variance $r_t^{\text{Kalman}} = \hat{x}_t(1 - \gamma q_t \sigma_{\text{rel},t}^2 \tau_{\text{inv}})$ to capture upward spikes (50% of anomalies currently ignored!) — and 100% market-neutral shorting on US equities.
7. **FP #7 — Mean-Reversion-Gated "Dip + Overshoot" Asymmetric Excursion Splitting:** Uses rolling Lo–MacKinlay Variance Ratio $\widehat{\text{VR}}_t(k) < 0.85$ + $Q_t/R_t$ to verify stationary chop; as idle time grows, shifts half of required fee-clearing excursion $G^* = 0.36\%$ from pre-entry dip ($a_t \to 0.18\%$, ~8x higher frequency) to post-mean overshoot ($b_t \to 0.18\%$) with $\alpha^2$ Kelly size damping.
8. **FP #8 — Fee-Optimal Geometric Volatility-Harvesting Sleeve ($\delta_g^* = 4f_m$):** Closed-form proof that optimal grid spacing is $\delta_g^* = 4f_m$ (`0.60%` on Tier 1 crypto, netting `+0.30%` per oscillation) in a capped `25%–35%` sleeve with a Kalman downward-drift circuit breaker.
9. **FP #9 — Newsvendor Optimal Two-Tier Liquidity Split (`SGOV` + Compound-Poisson Hot Buffer):** Exact Newsvendor fractile $C_{\text{hot}}^* = F_{M(\Delta T)}^{-1}(1 - r_f\Delta T / g_{\text{anom}})$ with Baumol–Tobin hysteresis band (`±3%–5%`) to park `65%–80%` idle cash in T-Bill ETFs with near-zero stockout drag.
10. **FP #10 — Spread-Vacuum Microstructure Capture (Kalman-Toxicity-Gated Inside-Spread Harvest):** Orthogonal trigger firing even when $\mu_t \approx 0$ whenever quoted spread exceeds round-trip fees (`> 34 bps`) AND Kalman toxicity ratio $\mathcal{T}_t = Q_t/(q_{\text{base}}m_t^2) \le 4.0$ confirms a benign liquidity vacuum rather than a crash.

---

## 3. Agent 3 — 8 Combined Hybrid Architectures

1. **Hybrid #1 (`Web #10 + FP #2 + Web #7`) — Fee-Flywheel Multi-Horizon OU Kalman Engine:** 4-scale Kalman bank (`35s/3m/15m/60m`) + live 30-day volume tier lookup + Leung–Li OU dynamic thresholds & scale-matched stops (`~5% → 25%–40%` crypto utilization; unlocks Tier 3–4 fees).
2. **Hybrid #2 (`V1.0 + FP #1 + FP #7 + Web #3`) — Drift- & Variance-Gated Safe V1.0 Deployment Engine:** 3-branch positive-EV evolution of V1.0 triggered only when inventory = 0 & $\Theta_{\text{idle}} > \Theta^*$: (a) chop mode (`VR < 0.85`: dip+overshoot `-0.18% → +0.18%`), (b) micro-bull mode (`v_t > v_min`: passive entry + Avellaneda–Stoikov fee-floored exit + hard stop $L^*$), (c) bearish hard veto.
3. **Hybrid #3 (`Web #6 + FP #4 + FP #3 + Web #4`) — Macro-Shielded Cross-Asset Renewal Pool & Liquidity Trap Matrix:** 8–12 cryptos monitored in parallel, median macro-crash shield ($F_t < -0.45\%$), Erlang-$c$ `3–4` funded slot allocator (`20%` each) rotating GLFT *Kalman-Shadow* resting traps (`45%–70%` 24/7 active alpha).
4. **Hybrid #4 (`Web #1 + FP #5 + Web #8`) — 24/7 Zero-Fee ETF / Native Crypto Seamless Regime Router:** Mon–Fri `09:35–15:50 ET` routes `75%–85%` to `$0`-fee ETFs (`BSOL`, `IBIT`, `ETHA`, `COIN`, `QQQ`) at `0.08%–0.12%` threshold with free intraday margin flattened by `15:50 ET`; BOATS overnight only if spread `< 15 bps`; weekends 100% native crypto.
5. **Hybrid #5 (`Web #2 + FP #9`) — Newsvendor Margin-Backed Yield Core + Compound-Poisson Hot Buffer:** `55%–70%` in `$0`-fee `SGOV`/`BOXX` (`~4.1%` APY + Reg-T collateral backing intraday US equity trades at `$0` interest!) + `30%–45%` settled High-Yield Cash for 24/7 crypto (`98%–100%` productive capital Day 1).
6. **Hybrid #6 (`Web #5 + FP #8 + Web #10`) — Regime-Gated Fee-Optimal Geometric Volatility Harvester:** Capped `25%–30%` sleeve spaced at $\delta_g^* = 4f_m$ (`0.60%` Tier 1 → `0.32%` Tier 4; `0.22%` on `$0`-fee ETFs) with 60m Kalman downward-drift buy freeze.
7. **Hybrid #7 (`Web #3 + FP #6 + FP #10`) — Bidirectional Kalman-Coupled Avellaneda–Stoikov + Spread-Vacuum Engine:** Captures upward spikes (market-neutral shorting on US equities; regime-gated `20%` cushion on crypto) + benign spread blowouts ($\mathcal{T}_t \le 4.0$).
8. **Hybrid #8 (`Web #9 + FP #2 + FP #4 + FP #9`) — Carver Multi-Sleeve Volatility-Targeted Capital Orchestrator:** Unifies all sleeves under a `12%–18%` annualized volatility target, fee-scaled inertia band, `-2.0%` daily drawdown kill-switch, and sleeve-isolated telemetry.

---

## 4. Phased Implementation Roadmap

### Phase 1 — Quick Wins & P0 Fee-Floor Fixes (Days 1–3)
- Dynamic 30-day crypto volume tier lookup + hard fee-clearing exit floor `exit_price >= avg_price * (1 + 2*f_maker + 0.0006)`.
- Deploy **Hybrid #5** (`60% SGOV` + `40%` hot crypto cash, `±3%` hysteresis, Friday `15:30 ET` `>= 45%` cash floor, `treasury_symbols` whitelist in `reconcile_and_maintain_positions`).
- Upgrade to **4-scale `MultiHorizonKalmanBank`** (`35s / 3m / 15m / 60m`) with anti-falling-knife gate and scale-matched stops/timeouts.
- *Expected Result:* Productive capital `~5% → 85%–95%`, active alpha `~5% → 25%–35%`.

### Phase 2 — Core Engine Upgrade (Weeks 1–2)
- Deploy **Hybrid #4** (`$0`-fee US equity/spot ETF router `BSOL`/`IBIT`/`COIN`/`QQQ`, `09:35–15:50 ET`, strict `15:50 ET` flatten).
- Deploy **Hybrid #2** (Drift- & Variance-Ratio-gated Safe V1.0 probes).
- Deploy **Hybrid #6** (capped `25%` fee-optimal geometric grid sleeve $\delta_g^* = 4f_m$ isolated via `grid-` order IDs).
- *Expected Result:* Productive capital `95%–100%`, active alpha `50%–70%`, Tier 3–4 crypto fees.

### Phase 3 — Institutional Full Stack (Weeks 3–4)
- Async `asyncio` + WebSockets + **Hybrid #3** (8–10 cryptos, Macro-Crash Shield, Erlang-$c$ GLFT resting traps).
- Deploy **Hybrid #7** (bidirectional shorting on ETFs, regime-gated crypto cushion, spread-vacuum harvest).
- Deploy **Hybrid #8** (Carver portfolio orchestrator, `15%` vol target, `-2%` daily kill-switch, per-sleeve telemetry).
- *Expected Result:* `98%–100%` productive capital 24/7, `70%–85%` active alpha duty cycle.

### Mandatory Safety Invariants
1. **Fee-Clearance Invariant:** No normal exit limit order below `entry * (1 + 2*live_maker_fee + 0.0005)`.
2. **Sleeve Isolation Invariant:** Explicit `sleeve_id` / symbol whitelists (`hunt`, `v1_probe`, `grid`, `etf_intraday`, `treasury`) so reconciliation & 12h stale timers never cross-contaminate sleeves.
3. **Zero-Overnight-Interest & Settlement Invariant:** Crypto strictly capped at `non_marginable_buying_power`; leveraged intraday US equities strictly flattened at `15:50 ET`.
4. **72h Paper Validation:** Validate every phase for 72h on Paper Trading with per-sleeve P&L attribution before advancing.
