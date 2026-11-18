"""
Unit tests for Mathematical Models, Dynamic Capital Allocation, and SEC Rule 612 Compliance.
"""

import math
import unittest


def calculate_shares_amount(margin: float, min_m: float, max_m: float, level: float, max_shares: int) -> int:
    """Python reference implementation of the asymptotic position sizing formula."""
    if margin < min_m or max_shares == 0 or level <= 0.0:
        return 0
    if margin > max_m:
        # Circuit breaker triggered
        return 0

    denom = max_m - min_m
    if denom <= 0:
        return 0

    x = max(0.0, min(1.0, (margin - min_m) / denom))
    x_pow = x ** level
    inner = max(0.0, min(1.0, 1.0 - x_pow))
    factor = 1.0 - (inner ** (1.0 / level))

    shares = int(math.floor(max_shares * factor))
    if shares == 0 and max_shares > 0 and x > 0.0:
        return 1
    return min(max_shares, shares)


def calculate_dynamic_portfolio_shares(
    margin: float,
    min_m: float,
    max_m: float,
    level: float,
    max_shares_fraction: float,
    total_equity: float,
    available_cash: float,
    buy_price: float,
) -> float:
    """Calculates position size as a dynamic proportion of total equity capped by available cash."""
    if margin < min_m or max_shares_fraction <= 0 or buy_price <= 0:
        return 0.0
    if margin > max_m:
        return 0.0

    denom = max_m - min_m
    if denom <= 0:
        return 0.0

    x = max(0.0, min(1.0, (margin - min_m) / denom))
    factor = 1.0 - ((1.0 - (x ** level)) ** (1.0 / level)) if level > 0 else x

    target_capital = factor * (total_equity * max_shares_fraction)
    allocated_capital = min(target_capital, max(0.0, available_cash))

    if allocated_capital < 10.0:
        return 0.0

    return round(allocated_capital / buy_price, 4)


def get_tick_size(price: float) -> float:
    return 0.01 if price >= 1.0 else 0.0001


def round_to_tick(price: float) -> float:
    tick = get_tick_size(price)
    return round(round(price / tick) * tick, 4)


def format_tick_price(price: float) -> str:
    tick = get_tick_size(price)
    p = round(round(price / tick) * tick, 4)
    if p >= 1.0:
        return f"{p:.2f}"
    return f"{p:.4f}"


def calculate_hidden_mm_buy_price(best_bid: float, best_ask: float) -> float:
    tick = get_tick_size(best_bid)
    b_tick = round_to_tick(best_bid)
    a_tick = round_to_tick(best_ask)
    spread_ticks = round((a_tick - b_tick) / tick)

    if spread_ticks >= 2:
        return round(b_tick + tick, 4)
    return b_tick


def calculate_exit_sell_price(entry_fill_price: float, fair_price: float) -> float:
    tick = get_tick_size(fair_price)
    target = math.floor(round(fair_price / tick, 6)) * tick
    min_profitable = round_to_tick(entry_fill_price + tick)
    return round(max(target, min_profitable), 4)


class TestCapitalAllocationAndPricing(unittest.TestCase):
    def setUp(self):
        self.min_margin = 0.0008  # 8 bps
        self.max_margin = 0.0120  # 120 bps (Circuit breaker)
        self.max_shares = 50
        self.level = 2.0

    def test_asymptotic_boundary_conditions(self):
        """Test strict boundary limits of SharesAmount(x)."""
        # 1. Below min_margin -> 0 shares
        self.assertEqual(
            calculate_shares_amount(0.0005, self.min_margin, self.max_margin, self.level, self.max_shares),
            0,
        )

        # 2. Exactly at min_margin -> minimal entry (0 or 1 share)
        at_min = calculate_shares_amount(self.min_margin, self.min_margin, self.max_margin, self.level, self.max_shares)
        self.assertIn(at_min, [0, 1])

        # 3. Exactly at max_margin -> max_shares
        self.assertEqual(
            calculate_shares_amount(self.max_margin, self.min_margin, self.max_margin, self.level, self.max_shares),
            self.max_shares,
        )

        # 4. Beyond max_margin (Circuit Breaker) -> 0 shares (Halt!)
        self.assertEqual(
            calculate_shares_amount(0.0150, self.min_margin, self.max_margin, self.level, self.max_shares),
            0,
        )

    def test_convexity_levels(self):
        """
        Verify that varying the level parameter changes aggressiveness:
        - level < 1.0 reacts strong at minimum anomalies (aggressive entry).
        - level = 1.0 is linear.
        - level > 1.0 waits for larger anomalies (patient / sniper style).
        """
        mid_margin = (self.min_margin + self.max_margin) / 2.0
        shares_aggressive = calculate_shares_amount(mid_margin, self.min_margin, self.max_margin, 0.5, self.max_shares)
        shares_linear = calculate_shares_amount(mid_margin, self.min_margin, self.max_margin, 1.0, self.max_shares)
        shares_sniper = calculate_shares_amount(mid_margin, self.min_margin, self.max_margin, 2.0, self.max_shares)

        self.assertGreater(shares_aggressive, shares_linear)
        self.assertGreater(shares_linear, shares_sniper)

    def test_sec_rule_612_tick_rules(self):
        """Ensure no sub-penny pricing on stocks >= $1.00."""
        # Stock >= $1.00 must have at most 2 decimal places
        self.assertEqual(format_tick_price(182.508), "182.51")
        self.assertEqual(format_tick_price(182.502), "182.50")
        self.assertEqual(format_tick_price(500.0049), "500.00")

        # Sub-dollar stock allows up to 4 decimal places
        self.assertEqual(format_tick_price(0.50254), "0.5025")
        self.assertEqual(format_tick_price(0.50256), "0.5026")

    def test_hidden_mm_queue_priority_and_safety(self):
        """Verify HiddenMM places limit order legally without crossing Best Ask."""
        # Case A: Wide Spread ($0.03): should step inside by +$0.01
        bid = 100.00
        ask = 100.03
        buy_price = calculate_hidden_mm_buy_price(bid, ask)
        self.assertEqual(buy_price, 100.01)
        self.assertLess(buy_price, ask)
        self.assertGreater(buy_price, bid)

        # Case B: 1-Tick Spread ($0.01): must join Best Bid, never cross Ask!
        bid = 100.00
        ask = 100.01
        buy_price = calculate_hidden_mm_buy_price(bid, ask)
        self.assertEqual(buy_price, 100.00)
        self.assertLess(buy_price, ask)

    def test_exit_limit_sell_pricing(self):
        """Ensure sell price is floored to tick and always above entry price."""
        entry = 100.01
        fair = 100.158
        sell_p = calculate_exit_sell_price(entry, fair)
        self.assertEqual(sell_p, 100.15)
        self.assertGreater(sell_p, entry)

    def test_dynamic_portfolio_equity_sizing(self):
        """
        Verify dynamic portfolio equity fraction sizing:
        - max_shares = 1.0 (100% equity): up to $100k
        - max_shares = 0.2 (20% equity): up to $20k
        - Cash starvation cap: if available cash < target, allocate remaining cash without asymptote
        - Fractional crypto share precision
        """
        equity = 100000.0
        cash = 100000.0
        price = 75.0

        # Full anomaly (x = 1.0) with max_shares = 1.0 (100%) -> $100,000 / $75 = 1333.3333 shares
        shares_100pct = calculate_dynamic_portfolio_shares(
            self.max_margin, self.min_margin, self.max_margin, 2.0, 1.0, equity, cash, price
        )
        self.assertAlmostEqual(shares_100pct, 1333.3333, places=3)

        # Full anomaly (x = 1.0) with max_shares = 0.2 (20%) -> $20,000 / $75 = 266.6667 shares
        shares_20pct = calculate_dynamic_portfolio_shares(
            self.max_margin, self.min_margin, self.max_margin, 2.0, 0.2, equity, cash, price
        )
        self.assertAlmostEqual(shares_20pct, 266.6667, places=3)

        # Cash starvation: target is $20,000, but only $5,000 cash remaining -> allocate all remaining $5,000 / $75 = 66.6667 shares
        limited_cash = 5000.0
        shares_starved = calculate_dynamic_portfolio_shares(
            self.max_margin, self.min_margin, self.max_margin, 2.0, 0.2, equity, limited_cash, price
        )
        self.assertAlmostEqual(shares_starved, 66.6667, places=3)

        # Circuit breaker trigger (> max_margin) -> 0 shares
        shares_cb = calculate_dynamic_portfolio_shares(
            0.0250, self.min_margin, self.max_margin, 2.0, 1.0, equity, cash, price
        )
        self.assertEqual(shares_cb, 0.0)


if __name__ == "__main__":
    unittest.main()
