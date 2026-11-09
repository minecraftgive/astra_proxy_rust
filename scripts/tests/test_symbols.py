"""Release symbol archives preserve originals and match published executables."""
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import struct
import subprocess
import sys
import tarfile
import tempfile
import unittest
from unittest.mock import Mock, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from astra_tools import symbols


def fixture(root, names=('astra-one', 'astra-two'), content=b'original executable'):
    source, staged = root / 'source', root / 'staged'
    source.mkdir()
    staged.mkdir()
    for name in names:
        (source / name).write_bytes(content)
        (source / name).chmod(0o755)
        shutil.copy2(source / name, staged / name)
    return source, staged, root / 'diagnostics/symbols.tar.gz'


class SymbolArchiveTests(unittest.TestCase):
    def test_archive_maps_both_hashes_and_preserves_originals(self):
        with tempfile.TemporaryDirectory() as folder:
            source, staged, archive = fixture(Path(folder))

            def strip(path, target):
                path.write_bytes(b'published executable')
                return 'test-strip'

            with patch.object(symbols, '_strip', side_effect=strip):
                record = symbols.separate_symbols(source, staged, ['astra-one', 'astra-two'],
                                                  target='test-target', archive_path=archive,
                                                  build_identity={'revision': 'abc', 'components': {'astra': '1'}})
            self.assertEqual(record['build_identity']['revision'], 'abc')
            with tarfile.open(archive) as archived:
                self.assertEqual(json.load(archived.extractfile('symbols.json')), record)
                for row in record['binaries']:
                    original = archived.extractfile(row['original_path']).read()
                    published = (staged / row['path']).read_bytes()
                    self.assertEqual(original, (source / row['path']).read_bytes())
                    self.assertEqual(row['original_sha256'], hashlib.sha256(original).hexdigest())
                    self.assertEqual(row['published_sha256'], hashlib.sha256(published).hexdigest())
                    self.assertNotEqual(original, published)
                    self.assertTrue((staged / row['path']).stat().st_mode & 0o111)
            self.assertEqual(sorted(path.name for path in staged.iterdir()), ['astra-one', 'astra-two'])

    def test_failed_strip_keeps_entire_staged_set_and_previous_archive(self):
        with tempfile.TemporaryDirectory() as folder:
            source, staged, archive = fixture(Path(folder))
            archive.parent.mkdir()
            archive.write_bytes(b'previous archive')

            def strip(path, target):
                path.write_bytes(b'partially modified')
                if path.name == 'astra-two':
                    raise RuntimeError('strip failed')
                return 'test-strip'

            with patch.object(symbols, '_strip', side_effect=strip), self.assertRaisesRegex(RuntimeError, 'strip failed'):
                symbols.separate_symbols(source, staged, ['astra-one', 'astra-two'],
                                         target='test-target', archive_path=archive)
            self.assertEqual(archive.read_bytes(), b'previous archive')
            for directory in (source, staged):
                for path in directory.iterdir():
                    self.assertEqual(path.read_bytes(), b'original executable')

    def test_rejects_original_hard_link_and_archive_inside_payload(self):
        with tempfile.TemporaryDirectory() as folder:
            source, staged, archive = fixture(Path(folder), ('astra-one',))
            (staged / 'astra-one').unlink()
            os.link(source / 'astra-one', staged / 'astra-one')
            with self.assertRaisesRegex(ValueError, 'original executable or hard link'):
                symbols.separate_symbols(source, staged, ['astra-one'], target='test', archive_path=archive)
            with self.assertRaisesRegex(ValueError, 'outside'):
                symbols.separate_symbols(source, staged, ['astra-one'], target='test',
                                         archive_path=staged / 'symbols.tar.gz')

    def test_rejects_unsafe_names_different_contents_and_unknown_format(self):
        with tempfile.TemporaryDirectory() as folder:
            source, staged, archive = fixture(Path(folder), ('astra-one',))
            for name in ('../astra-one', '..', 'folder\\astra-one'):
                with self.assertRaisesRegex(ValueError, 'Unsafe'):
                    symbols.separate_symbols(source, staged, [name], target='test', archive_path=archive)
            with self.assertRaisesRegex(ValueError, 'Unsupported executable format'):
                symbols.separate_symbols(source, staged, ['astra-one'], target='x86_64-unknown-linux-gnu',
                                         archive_path=archive)
            (staged / 'astra-one').write_bytes(b'other build')
            with self.assertRaisesRegex(ValueError, 'differs'):
                symbols.separate_symbols(source, staged, ['astra-one'], target='test', archive_path=archive)

    def test_msvc_preserves_pe_and_archives_available_pdb(self):
        with tempfile.TemporaryDirectory() as folder:
            pe = bytearray(128)
            pe[:2] = b'MZ'
            struct.pack_into('<I', pe, 60, 64)
            pe[64:68] = b'PE\0\0'
            source, staged, archive = fixture(Path(folder), ('astra-one.exe',), bytes(pe))
            (source / 'astra-one.pdb').write_bytes(b'PDB fixture')
            with patch.object(symbols, '_tool', side_effect=AssertionError('MSVC must not invoke strip')):
                record = symbols.separate_symbols(source, staged, ['astra-one.exe'],
                                                  target='x86_64-pc-windows-msvc', archive_path=archive)
            row = record['binaries'][0]
            self.assertEqual(row['strategy'], 'msvc-preserved')
            self.assertEqual(row['original_sha256'], row['published_sha256'])
            with tarfile.open(archive) as archived:
                self.assertEqual(archived.extractfile('unstripped/astra-one.pdb').read(), b'PDB fixture')
                self.assertEqual(record['debug_files'][0]['sha256'], hashlib.sha256(b'PDB fixture').hexdigest())

    def test_macos_identity_signature_is_never_replaced(self):
        signed = Mock(returncode=0, stdout='', stderr='Authority=Developer ID Application\nSignature size=123\n')
        with patch.object(symbols.subprocess, 'run', return_value=signed) as run:
            with self.assertRaisesRegex(RuntimeError, 'before identity signing'):
                symbols._macho_signature(Path('program'), 'codesign')
        self.assertEqual(run.call_count, 1)

    def test_macos_repairs_only_invalid_adhoc_signature(self):
        with tempfile.TemporaryDirectory() as folder:
            path = Path(folder) / 'program'
            path.write_bytes(b'\xcf\xfa\xed\xfe' + b'\0' * 60)
            ok = Mock(returncode=0, stdout='', stderr='')
            signature = Mock(returncode=0, stdout='', stderr='Signature=adhoc\n')
            invalid = Mock(returncode=1, stdout='', stderr='invalid signature')
            for repair, responses in ((True, [signature, ok, ok, invalid, ok, ok]),
                                      (False, [signature, ok, ok, ok])):
                with self.subTest(repair=repair), \
                     patch.object(symbols.platform, 'system', return_value='Darwin'), \
                     patch.object(symbols, '_tool', side_effect=lambda name: name), \
                     patch.object(symbols.subprocess, 'run', side_effect=responses) as run:
                    self.assertEqual(symbols._strip(path, 'aarch64-apple-darwin'), 'mach-o-strip')
                    commands = [call.args[0] for call in run.call_args_list]
                    self.assertEqual(['codesign', '--force', '--sign', '-', str(path)] in commands, repair)

    @unittest.skipUnless(platform.system() in ('Darwin', 'Linux') and shutil.which('cc') and shutil.which('strip'),
                         'native C compiler and strip required')
    def test_native_executable_runs_identically_and_archive_recovers_original(self):
        if platform.system() == 'Darwin' and not shutil.which('codesign'):
            self.skipTest('codesign required')
        with tempfile.TemporaryDirectory() as folder:
            root = Path(folder)
            source, staged, archive = fixture(root, names=())
            c_file = root / 'program.c'
            c_file.write_text('#include <stdio.h>\nint main(void) { puts("astra symbol check"); return 0; }\n')
            subprocess.run(['cc', '-g', str(c_file), '-o', str(source / 'program')], check=True, capture_output=True)
            shutil.copy2(source / 'program', staged / 'program')
            original = (source / 'program').read_bytes()
            expected = subprocess.run([str(source / 'program')], capture_output=True, check=True)
            target = 'aarch64-apple-darwin' if platform.system() == 'Darwin' else 'x86_64-unknown-linux-gnu'
            record = symbols.separate_symbols(source, staged, ['program'], target=target, archive_path=archive)
            actual = subprocess.run([str(staged / 'program')], capture_output=True, check=True)
            self.assertEqual((actual.stdout, actual.stderr), (expected.stdout, expected.stderr))
            self.assertEqual((source / 'program').read_bytes(), original)
            self.assertLess(record['binaries'][0]['published_size'], len(original))
            with tarfile.open(archive) as archived:
                self.assertEqual(archived.extractfile('unstripped/program').read(), original)


if __name__ == '__main__':
    unittest.main()
