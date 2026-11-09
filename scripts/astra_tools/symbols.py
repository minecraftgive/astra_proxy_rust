"""Separate release symbols without modifying Cargo's original executables.

The archive is a debugging artifact, never an install/update payload. It keeps
the exact original binary and a mapping to the published bytes. No debug info
is invented when the build profile did not generate it.
"""
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import struct
import subprocess
import tarfile
import tempfile


def _sha256(path):
    digest = hashlib.sha256()
    with Path(path).open('rb') as stream:
        for block in iter(lambda: stream.read(1024 * 1024), b''):
            digest.update(block)
    return digest.hexdigest()


def _tool(name):
    path = shutil.which(name)
    if not path:
        raise RuntimeError(f'Release symbol separation requires {name}')
    return path


def _run(command):
    result = subprocess.run(command, capture_output=True, text=True)
    if result.returncode:
        raise RuntimeError(f'{Path(command[0]).name} failed: {result.stderr.strip()}')
    return result


def _format(path, target):
    with Path(path).open('rb') as stream:
        header = stream.read(64)
        if 'windows-msvc' in target and len(header) >= 64 and header[:2] == b'MZ':
            stream.seek(struct.unpack_from('<I', header, 60)[0])
            if stream.read(4) == b'PE\0\0':
                return 'pe-msvc'
        elif 'apple-darwin' in target and len(header) >= 28 and header[:4] in (
                b'\xce\xfa\xed\xfe', b'\xcf\xfa\xed\xfe',
                b'\xfe\xed\xfa\xce', b'\xfe\xed\xfa\xcf',
                b'\xca\xfe\xba\xbe', b'\xbe\xba\xfe\xca'):
            return 'mach-o'
        elif 'linux' in target and len(header) >= 52 and header[:4] == b'\x7fELF':
            return 'elf'
    raise ValueError(f'Unsupported executable format/target for symbol separation: {path.name}: {target}')


def _macho_signature(path, codesign):
    details = subprocess.run([codesign, '--display', '--verbose=2', str(path)],
                             capture_output=True, text=True)
    description = details.stderr + details.stdout
    if details.returncode:
        if 'not signed at all' in description:
            return 'unsigned'
        raise RuntimeError(f'Cannot inspect Mach-O signature: {description.strip()}')
    if 'Signature=adhoc' not in description:
        raise RuntimeError('Release symbol separation must run before identity signing; '
                           f'refusing to replace the signing identity of {path.name}')
    _run([codesign, '--verify', str(path)])
    return 'adhoc'


def _strip(path, target):
    kind = _format(path, target)
    if kind == 'pe-msvc':
        # MSVC places debug info in a PDB, not an ELF-style symbol table. GNU
        # strip is not a safe substitute for a PE-aware publishing tool.
        return 'msvc-preserved'
    host = platform.system()
    if kind == 'mach-o':
        if host != 'Darwin':
            raise RuntimeError('Mach-O release symbols must be separated on macOS')
        codesign = _tool('codesign')
        signature = _macho_signature(path, codesign)
        _run([_tool('strip'), str(path)])
        if signature == 'adhoc' or target.startswith('aarch64-'):
            verified = subprocess.run([codesign, '--verify', str(path)],
                                      capture_output=True, text=True)
            if verified.returncode:
                _run([codesign, '--force', '--sign', '-', str(path)])
                _run([codesign, '--verify', str(path)])
        return 'mach-o-strip'
    if host != 'Linux':
        raise RuntimeError('ELF release symbols must be separated on Linux')
    _run([_tool('strip'), '--strip-unneeded', str(path)])
    return 'elf-strip-unneeded'


def separate_symbols(source, staged, names, *, target, archive_path, build_identity=None):
    """Archive originals and publish stripped copies, before manifest hashing.

    Every staged file must initially equal its original and be a distinct file.
    Strip/sign failures leave the staged files and originals unchanged. The
    completed archive is installed before any staged executable is replaced.
    Unsupported executables fail explicitly; tests/fixtures should use debug
    packaging instead of silently skipping release processing.
    """
    source, staged, archive_path = Path(source), Path(staged), Path(archive_path)
    names = sorted(set(names))
    if archive_path.resolve().is_relative_to(staged.resolve()):
        raise ValueError('Symbol archives must be outside the install/update payload')
    expected = {}
    for name in names:
        if not name or Path(name).name != name or name in ('.', '..') or '\\' in name:
            raise ValueError(f'Unsafe executable name: {name!r}')
        original, published = source / name, staged / name
        if original.is_symlink() or published.is_symlink():
            raise ValueError('Symbol separation requires regular executable copies')
        if not original.is_file() or not published.is_file():
            raise FileNotFoundError(f'Missing executable for symbol separation: {name}')
        if os.path.samefile(original, published):
            raise ValueError('Refusing to strip an original executable or hard link')
        expected[name] = _sha256(original)
        if expected[name] != _sha256(published):
            raise ValueError(f'Staged executable differs from its original: {name}')
    archive_path.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='.astra-symbols-', dir=archive_path.parent) as temporary:
        work = Path(temporary)
        originals, prepared = work / 'unstripped', work / 'prepared'
        originals.mkdir()
        prepared.mkdir()
        # Snapshot the whole set before invoking any tool, even a failing one.
        for name in names:
            shutil.copy2(source / name, originals / name)
            if _sha256(originals / name) != expected[name]:
                raise RuntimeError(f'Executable changed while taking symbol snapshot: {name}')
            shutil.copy2(originals / name, prepared / name)
        debug_files = []
        for name in names:
            sidecar = (source / name).with_suffix('.pdb')
            if sidecar.is_file() and not sidecar.is_symlink() and 'windows-msvc' in target:
                snapshot = originals / sidecar.name
                shutil.copy2(sidecar, snapshot)
                debug_files.append({'path': f'unstripped/{sidecar.name}', 'sha256': _sha256(snapshot)})
        records = []
        for name in names:
            original, published = originals / name, prepared / name
            strategy = _strip(published, target)
            records.append({'path': name, 'original_path': f'unstripped/{name}',
                            'original_sha256': expected[name], 'original_size': original.stat().st_size,
                            'published_sha256': _sha256(published), 'published_size': published.stat().st_size,
                            'strategy': strategy})
        record = {'schema_version': 1, 'target': target, 'profile': 'release',
                  'build_identity': build_identity or {}, 'binaries': records,
                  'debug_files': debug_files}
        (work / 'symbols.json').write_text(json.dumps(record, indent=2) + '\n', encoding='utf-8')
        temporary_archive = work / 'symbols.tar.gz'
        with tarfile.open(temporary_archive, 'w:gz') as archive:
            archive.add(originals, arcname='unstripped')
            archive.add(work / 'symbols.json', arcname='symbols.json')
        os.replace(temporary_archive, archive_path)
        # Stage via the destination filesystem, so --output-dir may be mounted
        # separately from the temporary workspace and os.replace stays atomic.
        for name in names:
            with tempfile.NamedTemporaryFile(prefix='.symbols-', dir=staged, delete=False) as output:
                replacement = Path(output.name)
            try:
                shutil.copy2(prepared / name, replacement)
                os.replace(replacement, staged / name)
            finally:
                replacement.unlink(missing_ok=True)
    return record
