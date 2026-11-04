"""Independent infrastructure tests; no proxy settings, TUN or installed cores."""
import ast
import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import Mock, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from astra_tools.artifacts import ArtifactIndex, ROLES, discover_index, package, role_packages, stage
from astra_tools.net import exact, socks_connect
from astra_tools.package_checks import verify_manifest
from astra_tools.platform import executable
from astra_tools.processes import StopPolicy, process, stop_process, wait_ready
from astra_tools.runner import Step, run_steps
from astra_tools.workspace import BuildLayout, resolve_binary_dir, _windows_toolchain


class LayoutTests(unittest.TestCase):
    def test_windows_discovery_skips_removed_compiler_and_imports_only_toolchain_environment(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            installer = root/'Microsoft Visual Studio/Installer/vswhere.exe'
            installer.parent.mkdir(parents=True)
            installer.touch()
            removed, actual = root/'Removed Preview', root/'Community'
            compiler = actual/'VC/Tools/MSVC/14.51/bin/Hostx64/x64/cl.exe'
            setup = actual/'Common7/Tools/VsDevCmd.bat'
            for file in (compiler, setup):
                file.parent.mkdir(parents=True, exist_ok=True)
                file.touch()
            instances = [{'installationVersion': '18.2.0', 'installationPath': str(removed)},
                         {'installationVersion': '18.1.0', 'installationPath': str(actual)}]
            env = {'ProgramFiles(x86)': str(root), 'PATH': 'original',
                   'VS180COMNTOOLS': str(removed / 'Common7/Tools')}
            replies = [Mock(stdout=json.dumps(instances)), Mock(stdout='Path=compiler-path\nINCLUDE=include-path\nPRIVATE_TOKEN=must-not-copy\n')]
            with patch('astra_tools.workspace.subprocess.run', side_effect=replies), patch('astra_tools.workspace.platform.machine', return_value='AMD64'):
                _windows_toolchain(env)
            self.assertEqual(env['CC'], str(compiler))
            self.assertEqual(env['CMAKE_GENERATOR_INSTANCE'], str(actual))
            self.assertEqual(env['CMAKE_GENERATOR'], 'Visual Studio 18 2026')
            self.assertEqual(env['VS180COMNTOOLS'], str(actual / 'Common7/Tools'))
            self.assertEqual(env['PATH'], 'compiler-path')
            self.assertNotIn('PRIVATE_TOKEN', env)

    def test_explicit_target_and_profile(self):
        layout = BuildLayout(Path('/custom/target'), 'x86_64-pc-windows-msvc')
        self.assertEqual(layout.binary_dir('debug'), Path('/custom/target/x86_64-pc-windows-msvc/debug'))
        self.assertEqual(layout.binary_dir('release').name, 'release')
        with self.assertRaises(ValueError):
            layout.binary_dir('../debug')

    def test_explicit_bin_directory_does_not_discover_cargo(self):
        with patch.object(BuildLayout, 'discover', side_effect=AssertionError('must not run Cargo')):
            self.assertEqual(resolve_binary_dir('somewhere'), Path('somewhere').resolve())

    def test_binary_ownership_and_roles(self):
        self.assertEqual(set(ROLES['client'].binaries), {'astra-rule','astra-proxy','astra-tui','astra-host','astra-manager','astra-convert'})
        self.assertEqual(set(ROLES['server'].binaries), {'astra-server-core','astra-server-tui','astra-host','astra-manager'})
        self.assertEqual(ROLES['controller'].binaries, ('astra-manager',))
        self.assertEqual(role_packages(['server']), ['astra-manager','astra-server-core','astra-tui'])


class ArtifactTests(unittest.TestCase):
    def test_index_roundtrip_nested_package_and_integrity(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            # Exercise a noncanonical spelling on every platform. macOS /var
            # aliases and Windows 8.3 temp paths expose the same distinction.
            (root / 'alias').mkdir()
            root = root / 'alias' / '..'
            binaries = root / 'target/host/debug'
            binaries.mkdir(parents=True)
            (binaries / executable('astra-manager')).write_bytes(b'controller executable fixture')
            run = root / 'dist/run-1'
            staged = stage('controller', binaries, run / 'debug/controller', target='test-target',
                           versions={'astra-manager':'7.8.9'}, label='7.8.9')
            manifest = verify_manifest(staged, 'controller')
            self.assertEqual(manifest['components'], {'astra-manager':'7.8.9'})
            archive = package('controller', staged, run / 'debug', '7.8.9', target='test-target')
            index = ArtifactIndex(run, 'test-target', 'abc123')
            index.add(role='controller', profile='debug', binary_dir=binaries, staged=staged, archive=archive)
            index.save()
            loaded = discover_index(root / 'dist')
            self.assertNotEqual(archive, archive.resolve())
            self.assertEqual(loaded.directory, run.resolve())
            self.assertEqual(loaded.archives(profile='debug'), [('controller', archive.resolve())])
            self.assertEqual(resolve_binary_dir(artifact_dir=run, profile='debug'), binaries.resolve())
            with self.assertRaises(ValueError):
                loaded.select(profile='release')
            archive.write_bytes(b'tampered')
            with self.assertRaisesRegex(ValueError, 'checksum'):
                loaded.archives()

    def test_multiple_runs_require_an_explicit_selection(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for name in ('one', 'two'):
                ArtifactIndex(root / name, 'test-target').save()
            with self.assertRaisesRegex(ValueError, 'select one'):
                discover_index(root)

    def test_path_escape_and_duplicate_are_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            index = ArtifactIndex(root, 'test-target')
            index.entries = [{'role':'web','profile':'release','staged':'../outside','archive':None}]
            index.save()
            with self.assertRaisesRegex(ValueError, 'escapes'):
                ArtifactIndex.load(root)
            index.entries = [{'role':'web','profile':'release','staged':'web','archive':None}] * 2
            index.save()
            with self.assertRaisesRegex(ValueError, 'duplicate'):
                ArtifactIndex.load(root)


class ProcessTests(unittest.TestCase):
    def test_real_process_cleanup_on_body_error_preserves_recovery_file(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            recovery = root / 'owned.system-proxy-state.toml'
            recovery.write_text('recovery fixture')
            with self.assertRaisesRegex(RuntimeError, 'scenario failed'):
                with process([sys.executable, '-c', 'import time; time.sleep(60)'], root, 'child',
                             stop=StopPolicy(mode='kill', timeout=2)) as (child, log):
                    raise RuntimeError('scenario failed')
            self.assertIsNotNone(child.poll())
            self.assertTrue(recovery.is_file())
            log.rename(root / 'closed.log')  # Windows detects unclosed file handles.

    def test_start_failure_closes_log(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            with self.assertRaises(OSError):
                with process([str(root / 'does-not-exist')], root, 'missing'):
                    self.fail('must not yield')
            (root / 'missing.log').rename(root / 'closed.log')

    def test_timeout_escalates_but_natural_exit_does_not_signal(self):
        child = Mock()
        child.poll.return_value = None
        child.wait.side_effect = [subprocess.TimeoutExpired('child', .01), 0]
        stop_process(child, StopPolicy(mode='terminate', timeout=.01))
        child.terminate.assert_called_once()
        child.kill.assert_called_once()
        exited = Mock()
        exited.poll.return_value = 0
        stop_process(exited)
        exited.send_signal.assert_not_called()

    def test_control_policy_and_readiness_failure(self):
        child = Mock()
        child.poll.return_value = None
        with patch('astra_tools.processes.subprocess.run') as run:
            stop_process(child, StopPolicy(mode='control', command=('core','--stop','endpoint')))
            self.assertEqual(run.call_args.args[0], ['core','--stop','endpoint'])
        with self.assertRaises(TimeoutError):
            wait_ready(lambda: False, child, timeout=.01, interval=.001)
        child.poll.return_value = 9
        child.returncode = 9
        with self.assertRaisesRegex(RuntimeError, 'Process exited'):
            wait_ready(lambda: True, child)


class NetworkTests(unittest.TestCase):
    def test_partial_reads_and_eof(self):
        source = Mock()
        source.recv.side_effect = [b'a', b'bc']
        self.assertEqual(exact(source, 3), b'abc')
        source.recv.side_effect = [b'x', b'']
        with self.assertRaises(EOFError):
            exact(source, 2)

    def test_socks_variable_reply_and_failure_cleanup(self):
        connection = Mock()
        connection.recv.side_effect = [b'\x05\x00', b'\x05\x00\x00\x03', b'\x04', b'host\x00\x01']
        with patch('astra_tools.net.socket.create_connection', return_value=connection):
            result, status = socks_connect(1234, 'example.test', 443)
        self.assertEqual(status, 0)
        self.assertIs(result, connection)
        connection.close.assert_not_called()
        connection.recv.side_effect = [b'']
        with patch('astra_tools.net.socket.create_connection', return_value=connection), self.assertRaises(EOFError):
            socks_connect(1234, 'example.test', 443)
        connection.close.assert_called_once()


class DependencyTests(unittest.TestCase):
    def test_scenarios_do_not_import_each_other(self):
        scripts = Path(__file__).resolve().parents[1]
        entrypoints = {p.stem for p in scripts.glob('*.py')}
        violations = []
        for path in scripts.rglob('*.py'):
            if '__pycache__' in path.parts:
                continue
            for node in ast.walk(ast.parse(path.read_text(encoding='utf-8'))):
                modules = ([node.module] if isinstance(node, ast.ImportFrom) and node.module else
                           [alias.name for alias in node.names] if isinstance(node, ast.Import) else [])
                for module in modules:
                    if module.split('.')[0] in entrypoints:
                        violations.append(f'{path.name}: {module}')
        self.assertEqual(violations, [])

    def test_runner_records_failure_and_stops(self):
        with tempfile.TemporaryDirectory() as temporary:
            report = Path(temporary) / 'report.json'
            results = run_steps([Step('fail',[sys.executable,'-c','raise SystemExit(7)']),
                                 Step('never',[sys.executable,'-c','raise SystemExit(0)'])],
                                env=dict(os.environ), report=report)
            self.assertEqual(len(results), 1)
            self.assertEqual(results[0]['exit_code'], 7)
            self.assertFalse(json.loads(report.read_text())['passed'])


if __name__ == '__main__':
    unittest.main()
