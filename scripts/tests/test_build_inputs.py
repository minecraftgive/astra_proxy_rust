"""Dependency watch boundaries must not follow dev or unrelated packages."""
from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from astra_tools.build_inputs import input_table, render_inputs
from astra_tools.workspace import ROOT


class BuildInputTests(unittest.TestCase):
    def test_repository_table_matches_manifests_and_excludes_unrelated_frontend(self):
        self.assertEqual((ROOT / 'crates/astra-platform/build_metadata_inputs.rs').read_text(),
                         render_inputs(ROOT))
        table = input_table(ROOT)
        self.assertIn('crates/astra-model', table['astra-manager'])
        self.assertNotIn('crates/astra-gui', table['astra-manager'])
        self.assertNotIn('crates/astra-proxy', table['astra-client'])
        self.assertNotIn('crates/astra-manager', table['astra-client'])

    def test_platform_embedded_icons_are_watched_without_all_gui_sources(self):
        table = input_table(ROOT)
        icons = {'crates/astra-gui/assets/icon.svg', 'crates/astra-gui/assets/icon.icns'}
        for icon in icons:
            self.assertTrue((ROOT / icon).is_file())
        for package in ('astra-client', 'astra-gui', 'astra-manager', 'astra-tui'):
            with self.subTest(package=package):
                self.assertTrue(icons.issubset(table[package]))
                if package != 'astra-gui':
                    self.assertNotIn('crates/astra-gui', table[package])

    def test_target_dependencies_aliases_and_patches_but_not_dev_dependencies(self):
        with tempfile.TemporaryDirectory() as temp:
            root = Path(temp)
            (root / 'Cargo.toml').write_text('''[workspace]
members = ["app", "shared", "test-only"]
[patch.crates-io]
third-party = { path = "vendor/third-party" }
''')
            for name in ['app', 'shared', 'test-only', 'vendor/third-party']:
                directory = root / name
                directory.mkdir(parents=True)
                extra = '''
[target.'cfg(windows)'.dependencies]
shared = { path = "../shared" }
alias = { package = "third-party", version = "1", optional = true }
[dev-dependencies]
test-only = { path = "../test-only" }
''' if name == 'app' else ''
                (directory / 'Cargo.toml').write_text(
                    f'[package]\nname = "{directory.name}"\nversion = "1.0.0"\n{extra}')
            (root / 'app/build.rs').write_text('// build_metadata.rs')
            self.assertEqual(input_table(root), {'app': ['app', 'shared', 'vendor/third-party']})


if __name__ == '__main__':
    unittest.main()
