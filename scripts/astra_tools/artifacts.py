"""One definition of build roles, release payloads and build-run discovery."""
from dataclasses import dataclass, field
from datetime import datetime, timezone
import hashlib
import json
import os
from pathlib import Path
import shutil
import tarfile
import tempfile
from .platform import executable
from .workspace import ROOT, cargo_profile as resolve_cargo_profile


@dataclass(frozen=True)
class Role:
    binaries: tuple[str, ...]
    example: str | None = None
    windows_binaries: tuple[str, ...] = ()


ROLES = {
    'client': Role(('astra-rule', 'astra-proxy', 'astra-tui', 'astra-host', 'astra-manager', 'astra-convert'),
                   'client.example.toml', ('astra-tui-startup', 'astra-service')),
    'server': Role(('astra-server-core', 'astra-server-tui', 'astra-manager', 'astra-host'),
                   'server.example.toml', ('astra-tui-startup',)),
    'controller': Role(('astra-manager',)),
    'web': Role(('astra-web',)),
}
COMPONENT_PACKAGES = {
    'astra-host': 'astra-manager',
    'astra-server-tui': 'astra-tui',
    'astra-tui-startup': 'astra-tui',
    'astra-service': 'astra-manager',
}


def role_binaries(roles, *, target=None):
    windows = os.name == 'nt' if target is None else 'windows' in target
    return sorted({binary for role in roles
                   for binary in ROLES[role].binaries + (ROLES[role].windows_binaries if windows else ())})


def role_packages(roles, *, target=None):
    return sorted({COMPONENT_PACKAGES.get(binary, binary) for binary in role_binaries(roles, target=target)})


def _build_metadata(profile, cargo_profile=None):
    """Build fields recorded with an artifact; the Cargo profile is derived from debug/release."""
    if profile is None:
        if cargo_profile is not None:
            raise ValueError('Cargo profile metadata requires an artifact profile')
        return {}
    expected = resolve_cargo_profile(profile)
    if cargo_profile is not None and cargo_profile != expected:
        raise ValueError(f'Cargo profile {cargo_profile} is not supported for {profile}: expected {expected}')
    return {'cargo_profile': expected}


def sha256(path):
    digest = hashlib.sha256()
    with Path(path).open('rb') as source:
        for block in iter(lambda: source.read(1024 * 1024), b''):
            digest.update(block)
    return digest.hexdigest()


def write_release_manifest(directory, label, target, versions, *, role='client', profile=None):
    build = _build_metadata(profile)
    if profile is not None:
        build['profile'] = profile
    directory = Path(directory)
    files = []
    for path in sorted(directory.iterdir()):
        if path.name == 'manifest.json':
            continue
        if not path.is_file() or path.is_symlink():
            raise ValueError(f'Unexpected release entry: {path}')
        files.append({'path': path.name, 'size': path.stat().st_size, 'sha256': sha256(path)})
    components = {name: versions[COMPONENT_PACKAGES.get(name, name)]
                  for name in role_binaries((role,), target=target)}
    identity = json.dumps({'target': target, 'role': role, 'components': components, 'files': files, **build},
                          sort_keys=True, separators=(',', ':')).encode()
    digest = hashlib.sha256(identity).hexdigest()[:16]
    manifest = {'schema_version': 1, 'role': role, 'release_id': f'{label[:64]}-{target}-{digest}',
                'target': target, 'host_protocol': {'min': 1, 'max': 1},
                'components': components, 'files': files, **build}
    (directory / 'manifest.json').write_text(json.dumps(manifest, indent=2) + '\n', encoding='utf-8')
    return manifest


def stage(role, bin_dir, destination, *, target, versions, label, profile=None):
    _build_metadata(profile)
    destination = Path(destination)
    destination.mkdir(parents=True, exist_ok=False)
    for binary in role_binaries((role,), target=target):
        shutil.copy2(Path(bin_dir) / executable(binary), destination / executable(binary))
    if ROLES[role].example:
        shutil.copy2(ROOT / 'configs' / ROLES[role].example, destination / ROLES[role].example)
    if role == 'client' and os.name == 'nt' and (Path(bin_dir) / 'wintun.dll').is_file():
        shutil.copy2(Path(bin_dir) / 'wintun.dll', destination / 'wintun.dll')
    write_release_manifest(destination, label, target, versions, role=role, profile=profile)
    return destination


def package(role, staged, output_dir, label, *, target):
    archive = Path(output_dir) / f'astra-{role}-{label}-{target}.tar.gz'
    archive.parent.mkdir(parents=True, exist_ok=True)
    with tarfile.open(archive, 'w:gz') as tar:
        tar.add(staged, arcname=role)
    print(f'Archive: {archive}\nSHA-256: {sha256(archive)}')
    return archive


@dataclass
class ArtifactIndex:
    directory: Path
    target: str
    source_revision: str = ''
    entries: list[dict] = field(default_factory=list)
    created_at: str = field(default_factory=lambda: datetime.now(timezone.utc).isoformat())

    def add(self, *, role, profile, binary_dir, staged, archive=None):
        if role not in ROLES or profile not in ('debug', 'release'):
            raise ValueError('Unknown artifact role or profile')
        build = _build_metadata(profile)
        if any(e['role'] == role and e['profile'] == profile for e in self.entries):
            raise ValueError(f'Duplicate artifact entry: {role}/{profile}')
        self.entries.append({'role': role, 'profile': profile, 'binary_dir': str(Path(binary_dir).resolve()),
                             'staged': Path(staged).resolve().relative_to(self.directory.resolve()).as_posix(),
                             'archive': Path(archive).resolve().relative_to(self.directory.resolve()).as_posix() if archive else None,
                             'archive_sha256': sha256(archive) if archive else None,
                             'binaries': role_binaries((role,), target=self.target), **build})

    def save(self):
        self.directory.mkdir(parents=True, exist_ok=True)
        data = {'schema_version': 1, 'target': self.target, 'source_revision': self.source_revision,
                'created_at': self.created_at, 'entries': self.entries}
        with tempfile.NamedTemporaryFile('w', encoding='utf-8', dir=self.directory, delete=False) as output:
            temporary = Path(output.name)
            json.dump(data, output, indent=2)
            output.write('\n')
        try:
            temporary.replace(self.directory / 'artifacts.json')
        finally:
            temporary.unlink(missing_ok=True)

    @classmethod
    def load(cls, directory):
        directory = Path(directory).resolve()
        path = directory if directory.is_file() else directory / 'artifacts.json'
        data = json.loads(path.read_text(encoding='utf-8'))
        if data.get('schema_version') != 1 or not isinstance(data.get('entries'), list):
            raise ValueError(f'Unsupported artifact index: {path}')
        index = cls(path.parent, data['target'], data.get('source_revision', ''), data['entries'], data['created_at'])
        seen = set()
        for entry in index.entries:
            pair = (entry['role'], entry['profile'])
            if pair in seen or pair[0] not in ROLES or pair[1] not in ('debug', 'release'):
                raise ValueError(f'Invalid or duplicate artifact selection: {pair}')
            if entry.get('cargo_profile', resolve_cargo_profile(pair[1])) != resolve_cargo_profile(pair[1]):
                raise ValueError(f'Artifact {pair[0]}/{pair[1]} was built with a removed Cargo profile '
                                 f'({entry.get("cargo_profile")}); rebuild with scripts/build.py')
            seen.add(pair)
            for name in ('staged', 'archive'):
                if entry.get(name) is not None:
                    resolved = (index.directory / entry[name]).resolve()
                    if not resolved.is_relative_to(index.directory):
                        raise ValueError(f'Artifact {name} escapes build directory')
        return index

    def select(self, *, profile=None, role=None):
        entries = [e for e in self.entries if (profile is None or e['profile'] == profile)
                   and (role is None or e['role'] == role)]
        if not entries:
            raise ValueError(f'No artifacts for profile={profile}, role={role}')
        return entries

    def archives(self, *, profile=None, role=None):
        result = []
        for entry in self.select(profile=profile, role=role):
            if entry['archive']:
                path = self.directory / entry['archive']
                if sha256(path) != entry['archive_sha256']:
                    raise ValueError(f'Archive checksum changed since build: {path}')
                result.append((entry['role'], path))
        if not result:
            raise ValueError('Selected build has no archives; build with --package')
        return result


def discover_index(directory=ROOT / 'dist'):
    directory = Path(directory)
    if (directory / 'artifacts.json').is_file():
        return ArtifactIndex.load(directory)
    candidates = sorted(directory.glob('*/artifacts.json'))
    if len(candidates) != 1:
        raise ValueError(f'Found {len(candidates)} build indexes; select one with --artifact-dir')
    return ArtifactIndex.load(candidates[0])
