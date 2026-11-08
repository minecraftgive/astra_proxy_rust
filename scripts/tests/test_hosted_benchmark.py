"""Idle tunnel measurements preserve socket ownership and group routing."""
import asyncio
import importlib.util
from pathlib import Path
import sys
import unittest
from unittest.mock import AsyncMock, Mock, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
spec = importlib.util.spec_from_file_location(
    'astra_hosted_benchmark_cli', Path(__file__).resolve().parents[1] / 'hosted_benchmark.py')
benchmark = importlib.util.module_from_spec(spec)
spec.loader.exec_module(benchmark)


def channel():
    reader = Mock(readexactly=AsyncMock(return_value=b'idle'), at_eof=Mock(return_value=False))
    writer = Mock(drain=AsyncMock(), wait_closed=AsyncMock())
    return reader, writer, 0.1


class IdleMemoryTests(unittest.IsolatedAsyncioTestCase):
    async def test_samples_retained_channels_and_reports_per_connection_delta(self):
        channels = [channel(), channel()]
        rss = iter([100] * 5 + [500] * 5)

        def sample():
            # None of the channels may close before the loaded sample.
            for _, writer, _ in channels:
                writer.close.assert_not_called()
            return next(rss)

        resources = Mock(rss=sample, errors=set())
        with patch.object(benchmark, 'connect', new=AsyncMock(side_effect=channels)), \
             patch.object(benchmark.asyncio, 'sleep', new=AsyncMock()):
            result = await benchmark.idle_memory(1080, 8080, 2, 0, resources)
        self.assertEqual(result['aggregate_rss_delta_bytes'], 400)
        self.assertEqual(result['rss_bytes_per_connection'], 200)
        for _, writer, _ in channels:
            writer.close.assert_called_once()
            writer.wait_closed.assert_awaited_once()

    async def test_failed_connect_closes_previous_channels(self):
        first = channel()
        with patch.object(benchmark, 'connect', new=AsyncMock(side_effect=[first, RuntimeError('failed')])), \
             patch.object(benchmark.asyncio, 'sleep', new=AsyncMock()):
            with self.assertRaisesRegex(RuntimeError, 'failed'):
                await benchmark.idle_memory(1080, 8080, 2, 0, Mock(rss=Mock(return_value=100), errors=set()))
        first[1].close.assert_called_once()
        first[1].wait_closed.assert_awaited_once()

    async def test_unavailable_rss_is_not_reported_as_zero(self):
        with patch.object(benchmark, 'connect', new=AsyncMock(return_value=channel())), \
             patch.object(benchmark.asyncio, 'sleep', new=AsyncMock()):
            result = await benchmark.idle_memory(1080, 8080, 1, 0, Mock(rss=Mock(return_value=None), errors=set()))
        self.assertIsNone(result['rss_bytes_per_connection'])

    async def test_closed_tunnel_invalidates_measurement(self):
        closed = channel()
        closed[0].at_eof.return_value = True
        with patch.object(benchmark, 'connect', new=AsyncMock(return_value=closed)), \
             patch.object(benchmark.asyncio, 'sleep', new=AsyncMock()):
            with self.assertRaisesRegex(RuntimeError, 'closed before RSS sample'):
                await benchmark.idle_memory(1080, 8080, 1, 0, Mock(rss=Mock(return_value=100), errors=set()))
        closed[1].close.assert_called_once()


class TrialConfigTests(unittest.TestCase):
    def test_group_trial_disables_local_bypass_and_routes_through_select_group(self):
        rule, proxy = benchmark.trial_config(Path('/tmp/route.sock'), 1080, 'group', 4096)
        self.assertIn('default_action = "proxy:bench"', rule)
        self.assertIn('direct_local = false', rule)
        self.assertIn('[proxy_groups.bench]', proxy)
        self.assertIn('selected = "DIRECT"', proxy)
        rule, proxy = benchmark.trial_config(Path('/tmp/route.sock'), 1080, 'direct', 4096)
        self.assertIn('default_action = "direct"', rule)
        self.assertNotIn('proxy_groups', proxy)


if __name__ == '__main__':
    unittest.main()
