import json
import math
import unittest
from decimal import Decimal

from pathlib import Path
from tempfile import TemporaryDirectory

from scripts.pairs_bot import PairParams, PairSignalEngine, PairSide, RuntimeState, rolling_zscores, json_ready


class RollingZScoresTests(unittest.TestCase):
    def test_returns_none_until_window_is_available(self):
        values = [1.0, 2.0, 3.0, 4.0]
        out = rolling_zscores(values, window=3)
        self.assertEqual(out[:3], [None, None, None])
        self.assertIsNotNone(out[3])


class PairSignalEngineTests(unittest.TestCase):
    def test_reward_risk_is_exactly_three_to_one(self):
        p = PairParams()
        self.assertAlmostEqual(p.reward_risk_ratio(), 3.0)

    def test_short_spread_signal_on_high_positive_z(self):
        p = PairParams(entry_z=3.5)
        engine = PairSignalEngine(p)
        signal = engine.entry_signal(3.6)
        self.assertEqual(signal, PairSide.SHORT_SPREAD)

    def test_long_spread_signal_on_low_negative_z(self):
        p = PairParams(entry_z=3.5)
        engine = PairSignalEngine(p)
        signal = engine.entry_signal(-3.6)
        self.assertEqual(signal, PairSide.LONG_SPREAD)

    def test_no_signal_inside_band(self):
        p = PairParams(entry_z=3.5)
        engine = PairSignalEngine(p)
        self.assertIsNone(engine.entry_signal(1.2))

    def test_short_spread_hits_target_when_z_reverts(self):
        p = PairParams(entry_z=3.5, stop_z=4.5, target_z=0.5)
        engine = PairSignalEngine(p)
        self.assertEqual(engine.exit_reason(PairSide.SHORT_SPREAD, 0.4, age_bars=1), 'target')

    def test_long_spread_hits_stop_when_z_keeps_widening(self):
        p = PairParams(entry_z=3.5, stop_z=4.5, target_z=0.5)
        engine = PairSignalEngine(p)
        self.assertEqual(engine.exit_reason(PairSide.LONG_SPREAD, -4.6, age_bars=1), 'stop')

    def test_time_stop_exits_after_max_hold(self):
        p = PairParams(max_hold_bars=72)
        engine = PairSignalEngine(p)
        self.assertEqual(engine.exit_reason(PairSide.SHORT_SPREAD, 2.0, age_bars=73), 'time')

    def test_json_ready_serializes_decimals(self):
        payload = {"x": Decimal("25"), "nested": [Decimal("1.5")]}
        out = json_ready(payload)
        self.assertEqual(out, {"x": "25", "nested": ["1.5"]})
        json.dumps(out)

    def test_runtime_state_persists_heartbeat_fields(self):
        with TemporaryDirectory() as td:
            path = Path(td) / 'state.json'
            s = RuntimeState(params=PairParams(), last_bar_ms=123, last_loop_wall_time=456.0, last_seen_z=1.25, last_seen_signal='x')
            s.save(path)
            loaded = RuntimeState.from_file(path, PairParams())
            self.assertEqual(loaded.last_bar_ms, 123)
            self.assertEqual(loaded.last_loop_wall_time, 456.0)
            self.assertEqual(loaded.last_seen_z, 1.25)
            self.assertEqual(loaded.last_seen_signal, 'x')


if __name__ == '__main__':
    unittest.main()
