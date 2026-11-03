"""Fault-injected launcher cleanup; never touches host network settings."""
import pathlib
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import Mock, patch

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1]))
from astra_tools.launcher import stop_children, finish_state


class CleanupTests(unittest.TestCase):
    def children(self):
        return [Mock(pid=10, poll=Mock(return_value=None)), Mock(pid=11, poll=Mock(return_value=None))]

    def test_exit_between_poll_and_terminate_still_reaps_and_stops_rule(self):
        rule, proxy = self.children()
        proxy.terminate.side_effect = ProcessLookupError('already exited')
        order = []
        proxy.wait.side_effect = lambda **_: order.append('proxy')
        rule.wait.side_effect = lambda **_: order.append('rule')
        self.assertEqual(stop_children([rule, proxy], [['rule'], ['proxy']], ['a', 'b'], windows=False), [])
        self.assertEqual(order, ['proxy', 'rule'])

    def test_failed_kill_does_not_skip_other_children(self):
        rule, proxy = self.children()
        proxy.wait.side_effect = subprocess.TimeoutExpired('proxy', 30)
        proxy.kill.side_effect = PermissionError('denied')
        errors = stop_children([rule, proxy], [['rule'], ['proxy']], ['a', 'b'], windows=False)
        self.assertTrue(any('denied' in e for e in errors))
        rule.terminate.assert_called_once()
        rule.wait.assert_called_once_with(timeout=30)

    def test_windows_ipc_failure_falls_back_and_forced_wait_is_bounded(self):
        child = self.children()[0]
        child.wait.side_effect = [subprocess.TimeoutExpired('proxy', 30), 1]
        with patch('astra_tools.launcher.subprocess.run', side_effect=subprocess.TimeoutExpired('stop', 5)):
            self.assertEqual(stop_children([child], [['proxy']], ['control'], windows=True), [])
        child.terminate.assert_called_once()
        child.kill.assert_called_once()
        self.assertEqual(child.wait.call_args.kwargs, {'timeout': 5})

    def test_partial_start_only_cleans_successfully_spawned_children(self):
        child = self.children()[0]
        self.assertEqual(stop_children([child], [['rule'], ['proxy']], ['a'], windows=False), [])
        child.terminate.assert_called_once()

    def test_unknown_and_recovery_records_are_never_deleted(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            for name in ('core.system-proxy-state.toml', 'network-recovery.json', 'unknown'):
                state = root / name
                state.mkdir()
                record = state / name
                record.write_text('recovery evidence')
                self.assertIsNotNone(finish_state(state))
                self.assertEqual(record.read_text(), 'recovery evidence')
            empty = root / 'empty'
            empty.mkdir()
            self.assertIsNone(finish_state(empty))
            self.assertFalse(empty.exists())
