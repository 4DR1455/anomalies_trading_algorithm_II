#!/usr/bin/env python3
"""
Master Execution Test Runner for Anomalies Trading Algorithm v2.0
Runs unit tests for capital allocation, SEC Rule 612 compliance, market hunt simulation,
circuit breaker response, and numpy risk metrics.
"""

import sys
import unittest
from pathlib import Path

# Ensure paths
test_dir = Path(__file__).resolve().parent
sys.path.insert(0, str(test_dir.parent))
sys.path.insert(0, str(test_dir))

import test_model
import test_simulation


def run_all_tests():
    print("=" * 70)
    print("🚀 RUNNING QUANTITATIVE & EXECUTION TEST SUITE (v2.0)")
    print("=" * 70)

    loader = unittest.TestLoader()
    suite = unittest.TestSuite()

    suite.addTests(loader.loadTestsFromModule(test_model))
    suite.addTests(loader.loadTestsFromModule(test_simulation))

    runner = unittest.TextTestRunner(verbosity=2)
    result = runner.run(suite)

    print("=" * 70)
    if result.wasSuccessful():
        print(f"✅ ALL {result.testsRun} TESTS PASSED SUCCESSFULLY!")
        print("   - Asymptotic Capital Allocation: PASSED")
        print("   - SEC Rule 612 Sub-Penny Constraints: PASSED")
        print("   - HiddenMM Maker Queue Priority: PASSED")
        print("   - Circuit Breaker Volatility Ceiling: PASSED")
        print("   - 20s Fill/Cancel & Partial Fill Recovery: PASSED")
        print("   - 12-Hour Stale Order Liquidation: PASSED")
        print("   - NumPy Sharpe Ratio & Risk Metrics: PASSED")
        print("=" * 70)
        return 0
    else:
        print(f"❌ TEST SUITE FAILED with {len(result.failures)} failures and {len(result.errors)} errors.")
        print("=" * 70)
        return 1


if __name__ == "__main__":
    sys.exit(run_all_tests())
