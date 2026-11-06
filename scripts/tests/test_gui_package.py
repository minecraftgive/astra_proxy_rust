"""Desktop archive layout checks with fake binaries; no GUI or cores are started."""
from pathlib import Path
import plistlib
import sys
import tarfile
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from astra_tools.artifacts import role_binaries
from package_gui import FRONTENDS, MACOS_MINIMUM_VERSION, package_frontend


class DesktopPackageTests(unittest.TestCase):
    def test_native_archives_keep_frontend_runtime_examples_and_windows_driver(self):
        for target in ('x86_64-unknown-linux-gnu', 'x86_64-pc-windows-msvc',
                       'aarch64-apple-darwin', 'x86_64-apple-darwin'):
            for role, binary, title in FRONTENDS:
                with self.subTest(target=target, role=role), tempfile.TemporaryDirectory() as temp:
                    source, output = Path(temp) / 'bin', Path(temp) / 'output'
                    source.mkdir()
                    extension = '.exe' if 'windows' in target else ''
                    for component in [binary, *role_binaries((role,), target=target)]:
                        program = source / (component + extension)
                        program.write_bytes(b'fixture')
                        program.chmod(0o755)
                    (source / 'wintun.dll').write_bytes(b'optional driver')
                    archive = package_frontend(source, output, target, role, binary, title, '1.2.3')
                    prefix = f'{binary}-{target}/'
                    macos = 'apple-darwin' in target
                    programs = prefix + (f'{title}.app/Contents/MacOS/' if macos else '')
                    resources = prefix + (f'{title}.app/Contents/Resources/' if macos else '')
                    with tarfile.open(archive) as packaged:
                        names = set(packaged.getnames())
                        for component in [binary, *role_binaries((role,), target=target)]:
                            self.assertIn(programs + component + extension, names)
                        self.assertIn(resources + f'{role}.example.toml', names)
                        self.assertEqual(programs + 'wintun.dll' in names, 'windows' in target and role == 'client')
                        if macos:
                            info = plistlib.load(packaged.extractfile(prefix + f'{title}.app/Contents/Info.plist'))
                            self.assertEqual(info['CFBundleExecutable'], binary)
                            self.assertEqual(info['CFBundleShortVersionString'], '1.2.3')
                            self.assertEqual(info['LSMinimumSystemVersion'], MACOS_MINIMUM_VERSION)
                            self.assertTrue(packaged.getmember(programs + binary).mode & 0o111)

    def test_incomplete_runtime_does_not_produce_an_archive(self):
        with tempfile.TemporaryDirectory() as temp:
            source, output = Path(temp) / 'bin', Path(temp) / 'output'
            source.mkdir()
            (source / 'astra-gui').write_bytes(b'fixture')
            with self.assertRaisesRegex(FileNotFoundError, 'runtime role'):
                package_frontend(source, output, 'x86_64-unknown-linux-gnu', 'client', 'astra-gui', 'Astra', '0.1.0')
            self.assertFalse(output.exists())


if __name__ == '__main__':
    unittest.main()
