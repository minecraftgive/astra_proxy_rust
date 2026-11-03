"""Release integrity checks shared by archive scenarios, independent of the builder."""
import hashlib
import json
from pathlib import Path


def verify_manifest(root, role):
    root = Path(root)
    manifest = json.loads((root / 'manifest.json').read_text(encoding='utf-8'))
    assert manifest['schema_version'] == 1 and manifest['release_id']
    assert manifest['role'] == role
    entries = manifest['files']
    names = [entry['path'] for entry in entries]
    assert len(names) == len(set(names)), 'Duplicate manifest file'
    actual = {p.name for p in root.iterdir() if p.name != 'manifest.json'}
    assert set(names) == actual, 'Manifest does not exactly cover archive'
    for entry in entries:
        assert entry['path'] == Path(entry['path']).name and entry['path'] not in ('.', '..')
        path = root / entry['path']
        assert path.is_file() and not path.is_symlink()
        digest = hashlib.sha256()
        with path.open('rb') as stream:
            for block in iter(lambda: stream.read(1024 * 1024), b''):
                digest.update(block)
        assert path.stat().st_size == entry['size'] and digest.hexdigest() == entry['sha256']
    return manifest
