"""Build profile and artifact integration tests without invoking Rust or native tools."""
import contextlib
import importlib.util
import io
import json
import os
from pathlib import Path
import sys
import tempfile
import tomllib
import unittest
from unittest.mock import Mock, patch

SCRIPTS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(SCRIPTS))
from astra_tools.artifacts import ArtifactIndex, ROLES, role_binaries, role_packages, stage, write_release_manifest
from astra_tools.package_checks import verify_manifest
from astra_tools.platform import executable
from astra_tools.workspace import BuildLayout, cargo_profile, resolve_binary_dir

def load_cli(name):
    """Load each command entrypoint as a test subject, never as a scenario dependency."""
    spec = importlib.util.spec_from_file_location(f'astra_{name}_cli', SCRIPTS / f'{name}.py')
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


build = load_cli('build')
astra = load_cli('astra')
test_cli = load_cli('test')


def artifact_fixture(root, *, profile='release', role='client'):
    bins = root / 'custom-target' / 'host' / profile
    bins.mkdir(parents=True, exist_ok=True)
    for binary in ROLES[role].binaries:
        (bins / executable(binary)).write_bytes(b'executable fixture')
    index = ArtifactIndex(root / 'run', 'host')
    archive = index.directory / 'fixture.tar.gz'
    archive.parent.mkdir(parents=True, exist_ok=True)
    archive.write_bytes(b'archive fixture')
    index.add(role=role, profile=profile, binary_dir=bins, staged=index.directory / profile / role, archive=archive)
    index.save()
    return index, bins.resolve()


class ProfileResolutionTests(unittest.TestCase):
    def test_only_dev_and_release_cargo_profiles_exist(self):
        layout = BuildLayout(Path('custom-target'), 'host')
        for profile, cargo in [('debug', 'dev'), ('release', 'release')]:
            with self.subTest(profile=profile):
                self.assertEqual(cargo_profile(profile), cargo)
                self.assertEqual(layout.binary_dir(profile), Path('custom-target/host') / profile)
        for profile in ('invalid', 'dev', 'release-fast', 'release-runtime', 'dev-runtime'):
            with self.subTest(profile=profile), self.assertRaises(ValueError):
                layout.binary_dir(profile)

    def test_workspace_manifests_define_only_dev_and_release(self):
        manifest = tomllib.loads((SCRIPTS.parent / 'Cargo.toml').read_text(encoding='utf-8'))
        self.assertEqual(set(manifest['profile']), {'dev', 'release'})
        for path in SCRIPTS.parent.glob('crates/*/Cargo.toml'):
            with self.subTest(crate=path.parent.name):
                self.assertNotIn('profile', tomllib.loads(path.read_text(encoding='utf-8')))
        config = tomllib.loads((SCRIPTS.parent / '.cargo/config.toml').read_text(encoding='utf-8'))
        self.assertNotIn('profile', config)
        # build.incremental would override the release profile's incremental = false.
        self.assertNotIn('incremental', config.get('build', {}))

    def test_artifacts_resolve_without_cargo(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            index, bins = artifact_fixture(root)
            with patch.object(BuildLayout, 'discover', side_effect=AssertionError('must not run Cargo')):
                self.assertEqual(resolve_binary_dir(artifact_dir=index.directory), bins)
                explicit = root / 'explicit'
                self.assertEqual(resolve_binary_dir(explicit, artifact_dir=index.directory), explicit.resolve())
            self.assertEqual(index.entries[0]['cargo_profile'], 'release')

    def test_artifacts_from_removed_profiles_rejected(self):
        with tempfile.TemporaryDirectory() as temporary:
            index, _ = artifact_fixture(Path(temporary))
            path = index.directory / 'artifacts.json'
            original = json.loads(path.read_text())
            for cargo in ('release-runtime', 'release-fast', 'dev', None):
                with self.subTest(cargo=cargo):
                    data = json.loads(json.dumps(original))
                    data['entries'][0]['cargo_profile'] = cargo
                    path.write_text(json.dumps(data))
                    with self.assertRaisesRegex(ValueError, 'removed Cargo profile'):
                        ArtifactIndex.load(index.directory)

    def test_manifest_records_derived_cargo_profile(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            (root / 'payload.bin').write_bytes(b'x')
            manifest = write_release_manifest(root, '0.1.0', 'host', {'astra-manager': '0.1.0'},
                                              role='controller', profile='debug')
            self.assertEqual((manifest['profile'], manifest['cargo_profile']), ('debug', 'dev'))
            with self.assertRaises(ValueError):
                stage('controller', root / 'bins', root / 'staged', target='host',
                      versions={'astra-manager': '0.1.0'}, label='0.1.0', profile='invalid')
            self.assertFalse((root / 'staged').exists())


class BuildCommandTests(unittest.TestCase):
    def test_silent_startup_helper_is_built_only_for_windows_tui_roles(self):
        helper = 'astra-tui-startup'
        for target in ('x86_64-pc-windows-msvc', 'aarch64-pc-windows-msvc',
                       'x86_64-unknown-linux-gnu', 'aarch64-apple-darwin'):
            for role in ROLES:
                with self.subTest(target=target, role=role):
                    command = build.build_command((role,), target, 'release')
                    binaries = [command[i + 1] for i, item in enumerate(command) if item == '--bin']
                    self.assertEqual(helper in binaries, 'windows' in target and role in ('client', 'server'))
                    self.assertEqual('astra-service' in binaries, 'windows' in target and role == 'client')
                    packages = [command[i + 1] for i, item in enumerate(command) if item == '-p']
                    self.assertNotIn(helper, packages, 'the helper belongs to the astra-tui crate')

    def test_silent_helper_is_staged_manifested_and_indexed_for_windows(self):
        helper = 'astra-tui-startup'
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            bins = root / 'bins'
            bins.mkdir()
            for binary in role_binaries(('client', 'server'), target='x86_64-pc-windows-msvc'):
                (bins / executable(binary)).write_bytes(b'executable fixture')
            versions = {name: '0.1.0' for name in role_packages(('client', 'server'))}
            for target in ('x86_64-pc-windows-msvc', 'x86_64-unknown-linux-gnu'):
                index = ArtifactIndex(root / target, target)
                for role in ('client', 'server'):
                    with self.subTest(target=target, role=role):
                        staged = stage(role, bins, index.directory / role, target=target,
                                       versions=versions, label='0.1.0', profile='release')
                        manifest = verify_manifest(staged, role)
                        index.add(role=role, profile='release', binary_dir=bins, staged=staged)
                        included = 'windows' in target
                        self.assertEqual((staged / executable(helper)).is_file(), included)
                        self.assertEqual(helper in manifest['components'], included)
                        self.assertEqual(helper in index.entries[-1]['binaries'], included)
                        if included:
                            self.assertEqual(manifest['components'][helper], versions['astra-tui'])
                        service = included and role == 'client'
                        self.assertEqual((staged / executable('astra-service')).is_file(), service)
                        self.assertEqual('astra-service' in manifest['components'], service)
                        self.assertEqual('astra-service' in index.entries[-1]['binaries'], service)

    def test_default_and_role_selection(self):
        args = build.argument_parser().parse_args([])
        self.assertEqual((args.profile, args.role), ('debug', 'client'))
        for roles in [('client',), ('controller',), tuple(ROLES)]:
            with self.subTest(roles=roles):
                command = build.build_command(roles, 'host', 'release', timings=True)
                binaries = [command[i + 1] for i, item in enumerate(command) if item == '--bin']
                self.assertEqual(set(binaries), {binary for role in roles for binary in ROLES[role].binaries})
                self.assertEqual(len(binaries), len(set(binaries)))
                self.assertNotIn('--bins', command)
                self.assertIn('--timings', command)
                self.assertEqual(command[command.index('--profile') + 1], 'release')

    def run_build(self, root, arguments, *, form_answers=None, cfg='target_pointer_width="64"\n'):
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

        interaction = '--non-interactive' if form_answers is None else '--interactive'
        argv = ['build.py', interaction, '--output-dir', str(root / 'dist'), *arguments]
        with patch.object(sys, 'argv', argv), patch.object(build, 'ROOT', root), \
             patch('builtins.input', side_effect=form_answers or AssertionError('must not prompt')), \
             patch.object(build, 'build_environment', return_value={'PATH': os.environ.get('PATH', '')}), \
             patch.object(build.BuildLayout, 'discover', return_value=(layout, {'packages': versions})), \
             patch.object(build.shutil, 'which', return_value='tool'), \
             patch.object(build.subprocess, 'run', side_effect=run), \
             patch.object(build.subprocess, 'check_output', return_value=cfg), \
             patch.object(build, 'build_web_assets', side_effect=lambda env: web_built.append(True)), \
             contextlib.redirect_stdout(io.StringIO()):
            build.main()
        runs = list((root / 'dist').glob('*/artifacts.json'))
        self.assertEqual(len(runs), 1)
        return calls, ArtifactIndex.load(runs[0])

    def test_default_build_only_emits_debug_with_metadata(self):
        with tempfile.TemporaryDirectory() as temporary:
            calls, index = self.run_build(Path(temporary), [])
            self.assertEqual(len(calls), 1)
            self.assertEqual(calls[0][calls[0].index('--profile') + 1], 'dev')
            entry, = index.entries
            self.assertEqual((entry['profile'], entry['cargo_profile']), ('debug', 'dev'))
            self.assertNotIn('mode', entry)
            manifest = json.loads((index.directory / entry['staged'] / 'manifest.json').read_text())
            self.assertEqual(manifest['cargo_profile'], 'dev')
            self.assertNotIn('mode', manifest)

    def test_both_profiles_preserve_web_guard_and_copy_each_timing_report(self):
        with tempfile.TemporaryDirectory() as temporary:
            calls, index = self.run_build(Path(temporary), ['--profile', 'both', '--role', 'web', '--timings'])
            self.assertEqual([c[c.index('--profile') + 1] for c in calls], ['dev', 'release'])
            self.assertEqual([entry['profile'] for entry in index.entries], ['debug', 'release'])
            for selected in ('dev', 'release'):
                self.assertTrue((index.directory / f'cargo-timing-{selected}.html').is_file())

    def test_debug_alias_overrides_release_selection(self):
        with tempfile.TemporaryDirectory() as temporary:
            calls, _ = self.run_build(Path(temporary), ['--profile', 'release', '--debug'])
            self.assertEqual(calls[0][calls[0].index('--profile') + 1], 'dev')

    def test_form_choices_drive_build_profiles_and_artifacts(self):
        with tempfile.TemporaryDirectory() as temporary:
            calls, index = self.run_build(Path(temporary), [], form_answers=[
                '1', 'both', '2', '64bits', '3', 'controller', '5', 'yes', 'b'])
            self.assertEqual([c[c.index('--profile') + 1] for c in calls], ['dev', 'release'])
            self.assertTrue(all('--timings' in c for c in calls))
            self.assertEqual([(e['role'], e['profile']) for e in index.entries],
                             [('controller', 'debug'), ('controller', 'release')])

    def test_64bits_rejects_32bit_target_before_creating_artifacts(self):
        with tempfile.TemporaryDirectory() as temporary, contextlib.redirect_stderr(io.StringIO()):
            root = Path(temporary)
            with self.assertRaises(SystemExit) as error:
                self.run_build(root, ['--arch', '64bits'], cfg='target_pointer_width="32"\n')
            self.assertEqual(error.exception.code, 2)
            self.assertFalse((root / 'dist').exists())
            self.assertFalse((root / 'custom-target').exists())


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
                self.assertEqual(command[command.index('--profile') + 1], 'release')
            self.assertEqual(runner.call_args.kwargs['env']['ASTRA_UPDATE_TEST_BIN'], str(bins))

    def test_test_cli_selects_dev_or_release(self):
        for extra, expected in [([], 'release'), (['--debug'], 'dev')]:
            with self.subTest(extra=extra), patch.object(sys, 'argv', ['test.py', 'check', *extra]), \
                 patch.object(test_cli, 'build_environment', return_value={}), \
                 patch.object(test_cli, 'host_target', return_value='host'), \
                 patch.object(test_cli, 'run_steps', return_value=[]) as runner, \
                 contextlib.redirect_stdout(io.StringIO()):
                test_cli.main()
                command = runner.call_args.args[0][0].command
                self.assertEqual(command[command.index('--profile') + 1], expected)

    def test_mode_option_was_removed_from_consumers(self):
        for module, name in [(test_cli, 'test.py'), (astra, 'astra.py')]:
            with self.subTest(command=name), patch.object(sys, 'argv', [name, '--mode', 'fast']), \
                 contextlib.redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as error:
                module.main()
            self.assertEqual(error.exception.code, 2)

    def test_web_contract_check_stays_in_dev_profile(self):
        with patch.object(sys, 'argv', ['test.py', 'web']), \
             patch.object(test_cli, 'host_target', return_value='host'), \
             patch.object(test_cli, 'run_steps', return_value=[]) as runner, \
             contextlib.redirect_stdout(io.StringIO()):
            test_cli.main()
        command = runner.call_args.args[0][0].command
        self.assertEqual(command[command.index('--profile') + 1], 'dev')

    def test_launcher_uses_index_path(self):
        with tempfile.TemporaryDirectory() as temporary:
            index, bins = artifact_fixture(Path(temporary))
            argv = ['astra.py', '--check', '--artifact-dir', str(index.directory)]
            with patch.object(sys, 'argv', argv), \
                 patch.object(astra.subprocess, 'check_output', return_value='standalone') as routing, \
                 patch.object(astra.subprocess, 'run') as validate:
                astra.main()
            self.assertEqual(routing.call_args.args[0][0], str(bins / executable('astra-proxy')))
            self.assertEqual(validate.call_args.args[0][0], str(bins / executable('astra-proxy')))


if __name__ == '__main__':
    unittest.main()
