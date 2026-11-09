"""Paired comparison contracts, without launching binaries or opening sockets."""
import argparse
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import Mock, patch

spec = importlib.util.spec_from_file_location(
    'astra_optimization_benchmark_cli', Path(__file__).resolve().parents[1] / 'optimization_benchmark.py')
benchmark = importlib.util.module_from_spec(spec)
spec.loader.exec_module(benchmark)


def child_report(family, hashes=None, throughput=100):
    quantiles = {'p50': 1, 'p95': 2, 'p99': 3}
    if family == 'hosted':
        rows = [
            {'mode': mode, 'route': route, 'setup_ms': dict(quantiles),
             'idle_memory': {'rss_bytes_per_connection': 2048, 'collection_errors': []},
             'throughput': {'echo_payload_mib_per_second': throughput,
                            'resources': {'collector': 'psutil proxy tree plus rule process',
                                          'cpu_seconds_during_throughput': .2,
                                          'cpu_percent_one_core_100': 10,
                                          'sampled_peak_aggregate_rss_bytes': 1048576,
                                          'peak_process_count': 2, 'collection_errors': []}}}
            for mode in ('ordinary', 'hosted') for route in ('direct', 'group')]
    else:
        rows = [
            {'rules': count, 'cache': cache, 'concurrency': concurrency,
             'setup_ms': dict(quantiles), 'echo_ms': dict(quantiles),
             'completed_per_second_median': throughput}
            for count in (1000, 10000) for cache in ('disabled', 'warm') for concurrency in (1, 32)]
    return {'results': rows, 'binary_sha256': hashes or {}}


def trial(side, **metrics):
    return {'side': side, 'scenarios': {'example': {'metrics': metrics, 'resource_issues': []}}}


class AggregateTests(unittest.TestCase):
    def test_medians_ignore_outlier_and_missing_resource_without_substituting_zero(self):
        result = benchmark.aggregate([
            trial('before', throughput_connections_per_second=100, setup_p99_ms=100, rss=None),
            trial('before', throughput_connections_per_second=100, setup_p99_ms=100, rss=None),
            trial('before', throughput_connections_per_second=1000, setup_p99_ms=1000, rss=None),
            trial('after', throughput_connections_per_second=97, setup_p99_ms=105, rss=200),
            trial('after', throughput_connections_per_second=97, setup_p99_ms=105, rss=None),
            trial('after', throughput_connections_per_second=1, setup_p99_ms=1, rss=400),
        ])
        metrics = result['scenarios']['example']['metrics']
        self.assertEqual(metrics['throughput_connections_per_second']['before'], 100)
        self.assertEqual(metrics['throughput_connections_per_second']['after'], 97)
        self.assertIsNone(metrics['rss']['before'])
        self.assertEqual(metrics['rss']['after'], 300)
        self.assertEqual(metrics['rss']['valid_samples'], {'before': 0, 'after': 2})
        self.assertEqual(result['regressions'], [], 'exact thresholds must not trigger extension')

    def test_flags_throughput_and_both_tail_latencies_but_not_p50(self):
        result = benchmark.aggregate([
            trial('before', throughput_mib_per_second=100, setup_p50_ms=100,
                  setup_p95_ms=100, setup_p99_ms=100, echo_p99_ms=100),
            trial('after', throughput_mib_per_second=96, setup_p50_ms=200,
                  setup_p95_ms=106, setup_p99_ms=106, echo_p99_ms=106),
        ])
        self.assertEqual({row['metric'] for row in result['regressions']}, {
            'throughput_mib_per_second', 'setup_p95_ms', 'setup_p99_ms', 'echo_p99_ms'})

    def test_normalize_reads_hosted_resources_and_rejects_incomplete_scenarios(self):
        report = child_report('hosted')
        rows = benchmark.normalize('hosted', report)
        self.assertEqual(len(rows), 4)
        self.assertEqual(rows['hosted/ordinary/direct']['metrics']['cpu_seconds_during_throughput'], .2)
        self.assertEqual(rows['hosted/hosted/group']['metrics']['rss_bytes_per_connection'], 2048)
        report['results'].pop()
        with self.assertRaisesRegex(ValueError, 'Incomplete hosted scenario set'):
            benchmark.normalize('hosted', report)


class DriverTests(unittest.TestCase):
    def test_alternates_pairs_and_extends_all_scenarios_to_ten_on_regression(self):
        with tempfile.TemporaryDirectory(prefix='astra-comparison-test-') as td:
            root = Path(td)
            args = argparse.Namespace(before=root / 'before', after=root / 'after',
                                      output_dir=root / 'results', python='python3',
                                      seconds=3, rounds=5, max_rounds=10, timeout=300, pause_seconds=0)
            for side in (args.before, args.after):
                side.mkdir()
                for name in ('astra-proxy', 'astra-rule'):
                    extension = '.exe' if benchmark.os.name == 'nt' else ''
                    (side / (name + extension)).write_bytes(side.name.encode())
            observed = []

            def child(command, **kwargs):
                family = 'hosted' if Path(command[1]).name == 'hosted_benchmark.py' else 'rule'
                directory = Path(command[command.index('--bin-dir') + 1])
                side = directory.name
                observed.append((family, side))
                self.assertEqual(command[command.index('--rounds') + 1], '1')
                self.assertEqual(command[command.index('--connections') + 1], '1000')
                if family == 'hosted':
                    self.assertEqual(command[command.index('--idle-connections') + 1], '100')
                    self.assertEqual(command[command.index('--streams') + 1], '4')
                    self.assertEqual(command[command.index('--seconds') + 1], '3')
                    self.assertEqual(command[command.index('--settle-seconds') + 1], '0.5')
                else:
                    self.assertEqual(command[command.index('--rules') + 1:command.index('--concurrency')],
                                     ['1000', '10000'])
                    self.assertEqual(command[command.index('--concurrency') + 1:command.index('--connections')],
                                     ['1', '32'])
                self.assertEqual(kwargs['env']['TMPDIR'], '/tmp')
                self.assertNotIn('ASTRA_HOSTED', kwargs['env'])
                output = Path(command[command.index('--output') + 1])
                report = child_report(family, benchmark.binary_hashes(directory),
                                      throughput=90 if side == 'after' else 100)
                output.write_text(json.dumps(report), encoding='utf-8')
                return Mock(returncode=0)

            with patch.object(benchmark.subprocess, 'run', side_effect=child), \
                 patch.object(benchmark.shutil, 'which', return_value='/usr/bin/python3'), \
                 patch.object(benchmark.platform, 'platform', return_value='test-platform'), \
                 patch.object(benchmark.time, 'sleep'), patch('builtins.print'), \
                 patch.dict(benchmark.os.environ, {'ASTRA_HOSTED': '1'}):
                benchmark.run(args)
            report = json.loads((args.output_dir / 'comparison.json').read_text(encoding='utf-8'))
            self.assertEqual(observed[:8], [
                ('hosted', 'before'), ('hosted', 'after'), ('rule', 'before'), ('rule', 'after'),
                ('rule', 'after'), ('rule', 'before'), ('hosted', 'after'), ('hosted', 'before')])
            self.assertEqual(len(observed), 40)
            self.assertEqual(report['completed_pairs'], 10)
            self.assertEqual(report['status'], 'complete')
            self.assertFalse(report['performance_gate_passed'])
            self.assertEqual(len(report['aggregate']['scenarios']), 12)
            for scenario in report['aggregate']['scenarios'].values():
                self.assertEqual(scenario['rounds'], {'before': 10, 'after': 10})


if __name__ == '__main__':
    unittest.main()
