"""Build mode and artifact integration tests without invoking Rust or native tools."""
import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import Mock, patch

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))
from astra_tools.artifacts import ArtifactIndex, ROLES, role_packages, stage, write_release_manifest
from astra_tools.platform import executable
from astra_tools.workspace import BuildLayout, cargo_profile, resolve_binary_dir, resolve_build_mode

def load_cli(name):
    """Load each command entrypoint as a test subject, never as a scenario dependency."""
    spec = importlib.util.spec_from_file_location(f'astra_{name}_cli', SCRIPTS / f'{name}.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


build = load_cli('build')
astra = load_cli('astra')
test_cli = load_cli('test')


def artifact_fixture(root, *, profile='release', mode='runtime', role='client'):
    bins = root / 'custom-target' / 'host' / cargo_profile(profile, mode)
    bins.mkdir(parents=True, exist_ok=True)
    for binary in ROLES[role].binaries:
        (bins / executable(binary)).write_bytes(b'executable fixture')
    index = ArtifactIndex(root / 'run', 'host')
    archive = index.directory / 'fixture.tar.gz'
    archive.parent.mkdir(parents=True, exist_ok=True)
    archive.write_bytes(b'archive fixture')
    metadata = {} if mode is None else {'mode': mode, 'cargo_profile': cargo_profile(profile, mode)}
    index.add(role=role, profile=profile, binary_dir=bins, staged=index.directory / profile / role,
              archive=archive, **metadata)
    index.save()
    return index, bins.resolve()


class ModeResolutionTests(unittest.TestCase):
    def test_distinct_profile_directories_under_custom_target(self):
        layout = BuildLayout(Path('custom-target'), 'host')
        expected = {('debug', 'fast'): ('dev', 'debug'),
                    ('release', 'fast'): ('release-fast', 'release-fast'),
                    ('debug', 'runtime'): ('dev-runtime', 'dev-runtime'),
                    ('release', 'runtime'): ('release-runtime', 'release-runtime')}
        for (profile, mode), (cargo, directory) in expected.items():
            with self.subTest(profile=profile, mode=mode):
                self.assertEqual(cargo_profile(profile, mode), cargo)
                self.assertEqual(layout.binary_dir(profile, mode), Path('custom-target/host') / directory)
        self.assertEqual(layout.binary_dir('debug'), Path('custom-target/host/debug'))
        self.assertEqual(layout.binary_dir('release'), Path('custom-target/host/release'))
        for profile, mode in [('invalid', 'fast'), ('debug', 'invalid')]:
            with self.assertRaises(ValueError):
                layout.binary_dir(profile, mode)

    def test_new_and_legacy_artifacts_resolve_without_cargo(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for mode in ('fast', 'runtime', None):
                with self.subTest(mode=mode):
                    index, bins = artifact_fixture(root / str(mode), mode=mode)
                    with patch.object(BuildLayout, 'discover', side_effect=AssertionError('must not run Cargo')):
                        self.assertEqual(resolve_build_mode(index.directory), mode)
                        self.assertEqual(resolve_binary_dir(artifact_dir=index.directory), bins)
                        explicit = root / 'explicit'
                        self.assertEqual(resolve_binary_dir(explicit, artifact_dir=index.directory), explicit.resolve())
                    conflicting = 'fast' if mode != 'fast' else 'runtime'
                    with self.assertRaisesRegex(ValueError, 'conflicts'):
                        resolve_binary_dir(root / 'explicit', artifact_dir=index.directory, mode=conflicting)

    def test_invalid_and_partial_artifact_metadata_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            index, _ = artifact_fixture(Path(temporary))
            path = index.directory / 'artifacts.json'
            original = json.loads(path.read_text())
            for fields in ({'mode': 'runtime'}, {'mode': 'runtime', 'cargo_profile': 'release-fast'},
                           {'mode': 'invalid', 'cargo_profile': 'release'},
                           {'mode': None, 'cargo_profile': None}):
                with self.subTest(fields=fields):
                    data = json.loads(json.dumps(original))
                    data['entries'][0].pop('mode')
                    data['entries'][0].pop('cargo_profile')
                    data['entries'][0].update(fields)
                    path.write_text(json.dumps(data))
                    with self.assertRaises(ValueError):
                        ArtifactIndex.load(index.directory)

    def test_partial_manifest_metadata_rejected_before_staging(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            for fields in ({'profile': 'debug'}, {'mode': 'fast'}, {'cargo_profile': 'dev'},
                           {'profile': 'debug', 'mode': 'fast'}):
                with self.subTest(fields=fields):
                    with self.assertRaisesRegex(ValueError, 'requires profile, mode and cargo_profile together'):
                        stage('controller', root / 'bins', root / 'staged', target='host',
                              versions={'astra-manager': '0.1.0'}, label='0.1.0', **fields)
                    self.assertFalse((root / 'staged').exists())
                    with self.assertRaisesRegex(ValueError, 'requires profile, mode and cargo_profile together'):
                        write_release_manifest(root, '0.1.0', 'host', {'astra-manager': '0.1.0'},
                                               role='controller', **fields)
                    self.assertFalse((root / 'manifest.json').exists())


class BuildCommandTests(unittest.TestCase):
    def test_default_and_role_selection(self):
        args = build.argument_parser().parse_args([])
        self.assertEqual((args.profile, args.mode, args.role), ('debug', 'fast', 'client'))
        for roles in [('client',), ('controller',), tuple(ROLES)]:
            with self.subTest(roles=roles):
                command = build.build_command(roles, 'host', 'release', 'fast', timings=True)
                binaries = [command[i + 1] for i, item in enumerate(command) if item == '--bin']
                self.assertEqual(set(binaries), {binary for role in roles for binary in ROLES[role].binaries})
                self.assertEqual(len(binaries), len(set(binaries)))
                self.assertNotIn('--bins', command)
                self.assertIn('--timings', command)
                self.assertEqual(command[command.index('--profile') + 1], 'release-fast')

    def run_build(self, root, arguments):
        layout = BuildLayout(root / 'custom-target', 'host')
        versions = [{'name': name, 'version': '0.1.0'} for name in role_packages(tuple(ROLES))]
        calls = []
        web_built = []

        def run(command, **kwargs):
            if command[0] == 'cargo':
                selected = command[command.index('--profile') + 1]
                bins = layout.target_directory / layout.target / ('debug' if selected == 'dev' else selected)
                bins.mkdir(parents=True, exist_ok=True)
                names = [command[i + 1] for i, item in enumerate(command) if item == '--bin']
                if 'astra-web' in names:
                    self.assertTrue(web_built, 'web assets must be prepared before compilation')
                    self.assertEqual(kwargs['env']['ASTRA_WEB_REQUIRE_ASSETS'], '1')
                for name in names:
                    (bins / executable(name)).write_bytes(b'executable fixture')
                if '--timings' in command:
                    report = layout.target_directory / 'cargo-timings' / 'cargo-timing.html'
                    report.parent.mkdir(exist_ok=True)
                    report.write_text('<html>timing fixture</html>')
                calls.append(command)
            return Mock(returncode=0, stdout='revision\n')

        argv = ['build.py', '--non-interactive', '--output-dir', str(root / 'dist'), *arguments]
        with patch.object(sys, 'argv', argv), patch.object(build, 'ROOT', root), \
             patch.object(build, 'build_environment', return_value={'PATH': os.environ.get('PATH', '')}), \
             patch.object(build.BuildLayout, 'discover', return_value=(layout, {'packages': versions})), \
             patch.object(build.shutil, 'which', return_value='tool'), \
             patch.object(build.subprocess, 'run', side_effect=run), \
             patch.object(build, 'build_web_assets', side_effect=lambda env: web_built.append(True)), \
             contextlib.redirect_stdout(io.StringIO()):
            build.main()
        runs = list((root / 'dist').glob('*/artifacts.json'))
        self.assertEqual(len(runs), 1)
        return calls, ArtifactIndex.load(runs[0])

    def test_default_build_only_emits_fast_debug_with_metadata(self):
        with tempfile.TemporaryDirectory() as temporary:
            calls, index = self.run_build(Path(temporary), [])
            self.assertEqual(len(calls), 1)
            self.assertEqual(calls[0][calls[0].index('--profile') + 1], 'dev')
            entry, = index.entries
            self.assertEqual((entry['profile'], entry['mode'], entry['cargo_profile']), ('debug', 'fast', 'dev'))
            manifest = json.loads((index.directory / entry['staged'] / 'manifest.json').read_text())
            self.assertEqual(manifest['mode'], 'fast')
            self.assertEqual(manifest['cargo_profile'], 'dev')

    def test_both_modes_preserve_web_guard_and_copy_each_timing_report(self):
        with tempfile.TemporaryDirectory() as temporary:
            for mode, profiles in [('fast', ['dev', 'release-fast']),
                                   ('runtime', ['dev-runtime', 'release-runtime'])]:
                with self.subTest(mode=mode):
                    calls, index = self.run_build(Path(temporary) / mode,
                                                 ['--profile', 'both', '--role', 'web', '--mode', mode, '--timings'])
                    self.assertEqual([c[c.index('--profile') + 1] for c in calls], profiles)
                    self.assertEqual([entry['profile'] for entry in index.entries], ['debug', 'release'])
                    for selected in profiles:
                        self.assertTrue((index.directory / f'cargo-timing-{selected}.html').is_file())

    def test_debug_alias_overrides_release_selection(self):
        with tempfile.TemporaryDirectory() as temporary:
            calls, _ = self.run_build(Path(temporary), ['--profile', 'release', '--mode', 'runtime', '--debug'])
            self.assertEqual(calls[0][calls[0].index('--profile') + 1], 'dev-runtime')


class ConsumerCommandTests(unittest.TestCase):
    def test_test_cli_infers_profile_and_update_binary_directory(self):
        with tempfile.TemporaryDirectory() as temporary:
            index, bins = artifact_fixture(Path(temporary))
            argv = ['test.py', 'check', 'unit', '--artifact-dir', str(index.directory)]
            with patch.object(sys, 'argv', argv), patch.object(test_cli, 'build_environment', return_value={}), \
                 patch.object(test_cli, 'host_target', return_value='host'), \
                 patch.object(test_cli, 'run_steps', return_value=[]) as runner, \
                 contextlib.redirect_stdout(io.StringIO()):
                test_cli.main()
            steps = runner.call_args.args[0]
            commands = [s.command for s in steps if s.command[0] == 'cargo']
            self.assertEqual(len(commands), 2)
            for command in commands:
                self.assertEqual(command[command.index('--profile') + 1], 'release-runtime')
            self.assertEqual(runner.call_args.kwargs['env']['ASTRA_UPDATE_TEST_BIN'], str(bins))

    def test_test_cli_without_mode_retains_legacy_profiles(self):
        for extra, expected in [([], 'release'), (['--debug'], 'dev'),
                                (['--mode', 'fast'], 'release-fast')]:
            with self.subTest(extra=extra), patch.object(sys, 'argv', ['test.py', 'check', *extra]), \
                 patch.object(test_cli, 'build_environment', return_value={}), \
                 patch.object(test_cli, 'host_target', return_value='host'), \
                 patch.object(test_cli, 'run_steps', return_value=[]) as runner, \
                 contextlib.redirect_stdout(io.StringIO()):
                test_cli.main()
                command = runner.call_args.args[0][0].command
                self.assertEqual(command[command.index('--profile') + 1], expected)

    def test_package_discovery_cannot_bypass_mode_conflict(self):
        with tempfile.TemporaryDirectory() as temporary:
            index, _ = artifact_fixture(Path(temporary), role='controller')
            with patch.object(sys, 'argv', ['test.py', 'package', '--mode', 'fast']), \
                 patch.object(test_cli, 'discover_index', return_value=index), \
                 patch.object(test_cli, 'run_steps') as runner, contextlib.redirect_stderr(io.StringIO()):
                with self.assertRaises(SystemExit) as error:
                    test_cli.main()
                self.assertEqual(error.exception.code, 2)
                runner.assert_not_called()

    def test_legacy_web_contract_check_stays_in_dev_profile(self):
        with patch.object(sys, 'argv', ['test.py', 'web']), \
             patch.object(test_cli, 'host_target', return_value='host'), \
             patch.object(test_cli, 'run_steps', return_value=[]) as runner, \
             contextlib.redirect_stdout(io.StringIO()):
            test_cli.main()
        command = runner.call_args.args[0][0].command
        self.assertEqual(command[command.index('--profile') + 1], 'dev')

    def test_launcher_uses_index_path_and_rejects_explicit_conflicting_mode(self):
        with tempfile.TemporaryDirectory() as temporary:
            index, bins = artifact_fixture(Path(temporary))
            argv = ['astra.py', '--check', '--artifact-dir', str(index.directory)]
            with patch.object(sys, 'argv', argv), \
                 patch.object(astra.subprocess, 'check_output', return_value='standalone') as routing, \
                 patch.object(astra.subprocess, 'run') as validate:
                astra.main()
            self.assertEqual(routing.call_args.args[0][0], str(bins / executable('astra-proxy')))
            self.assertEqual(validate.call_args.args[0][0], str(bins / executable('astra-proxy')))
            with patch.object(sys, 'argv', [*argv, '--bin-dir', str(bins), '--mode', 'fast']), \
                 patch.object(astra.subprocess, 'run') as validate, contextlib.redirect_stderr(io.StringIO()):
                with self.assertRaises(SystemExit) as error:
                    astra.main()
                self.assertEqual(error.exception.code, 2)
                validate.assert_not_called()


if __name__ == '__main__':
    unittest.main()
