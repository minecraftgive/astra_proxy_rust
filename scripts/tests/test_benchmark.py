"""Benchmark failure cleanup and diagnostics without opening network sockets."""
import asyncio
import importlib.util
from pathlib import Path
import sys
import unittest
from unittest.mock import AsyncMock, Mock, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
# Load the CLI as a test subject without making it a scenario dependency.
spec = importlib.util.spec_from_file_location(
    'astra_benchmark_cli', Path(__file__).resolve().parents[1] / 'benchmark.py')
benchmark = importlib.util.module_from_spec(spec)
spec.loader.exec_module(benchmark)


class BenchmarkFailureTests(unittest.IsolatedAsyncioTestCase):
    async def test_socks_connect_error_reports_status_and_closes_writer(self):
        reader = Mock(readexactly=AsyncMock(side_effect=[
            b'\x05\x00', b'\x05\x05\x00\x01' + b'\x00' * 6,
        ]))
        writer = Mock(drain=AsyncMock(), wait_closed=AsyncMock())
        with patch.object(benchmark.asyncio, 'open_connection', new=AsyncMock(return_value=(reader, writer))):
            with self.assertRaisesRegex(RuntimeError, r'SOCKS5 CONNECT failed with status 5 \(0x05\)'):
                await benchmark.query(1080, 8080)
        writer.close.assert_called_once()
        writer.wait_closed.assert_awaited_once()

    async def test_failed_sample_waits_for_sibling_cleanup(self):
        pending = asyncio.Event()
        cleaned = asyncio.Event()
        never = asyncio.Event()
        calls = 0

        async def query(_port, _target):
            nonlocal calls
            calls += 1
            if calls == 1:
                pending.set()
                try:
                    await never.wait()
                finally:
                    await asyncio.sleep(0)
                    cleaned.set()
            else:
                await pending.wait()
                raise RuntimeError('sample failed')

        with patch.object(benchmark, 'query', new=query):
            with self.assertRaisesRegex(RuntimeError, 'sample failed'):
                await benchmark.samples(1080, 8080, 2, 2)
        self.assertTrue(cleaned.is_set(), 'failure propagated before sibling cleanup completed')

    async def test_cancelled_samples_wait_for_all_query_cleanup(self):
        started = asyncio.Event()
        never = asyncio.Event()
        active = 0
        cleaned = 0

        async def query(_port, _target):
            nonlocal active, cleaned
            active += 1
            if active == 2:
                started.set()
            try:
                await never.wait()
            finally:
                await asyncio.sleep(0)
                cleaned += 1

        with patch.object(benchmark, 'query', new=query):
            task = asyncio.create_task(benchmark.samples(1080, 8080, 2, 2))
            await started.wait()
            task.cancel()
            with self.assertRaises(asyncio.CancelledError):
                await task
        self.assertEqual(cleaned, 2)


if __name__ == '__main__':
    unittest.main()
