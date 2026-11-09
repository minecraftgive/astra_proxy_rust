#!/usr/bin/env python3
"""Package desktop frontends with their existing client/server runtime binaries."""
import argparse
import hashlib
import json
from pathlib import Path
import plistlib
import shutil
import tarfile
import tempfile

from astra_tools.artifacts import ROLES, COMPONENT_PACKAGES, role_binaries
from astra_tools.symbols import separate_symbols
from astra_tools.workspace import ROOT, BuildLayout, build_environment

# Match the deployment target used by the desktop CI build and GPUI's shaders.
MACOS_MINIMUM_VERSION = '12.0'
ICONS = ROOT / 'crates' / 'astra-gui' / 'assets'
FRONTENDS = (('client', 'astra-gui', 'Astra'), ('server', 'astra-server-gui', 'Astra Server'))


def sha256_file(path):
    digest = hashlib.sha256()
    with path.open('rb') as source:
        for block in iter(lambda: source.read(1024 * 1024), b''):
            digest.update(block)
    return digest.hexdigest()


def package_frontend(source, output_dir, target, role, binary, title, version, versions=None, *, profile='debug'):
    if profile not in ('debug', 'release'):
        raise ValueError(f'Unsupported desktop packaging profile: {profile}')
    source, output_dir = Path(source), Path(output_dir)
    windows = 'windows' in target
    macos = 'apple-darwin' in target
    names = [name + ('.exe' if windows else '')
             for name in [binary, *role_binaries((role,), target=target)]]
    missing = [str(source / name) for name in names if not (source / name).is_file()]
    if missing:
        raise FileNotFoundError('Build the desktop frontend and its runtime role first: ' + ', '.join(missing))
    output_dir.mkdir(parents=True, exist_ok=True)
    name = f'{binary}-{target}'
    with tempfile.TemporaryDirectory(prefix='astra-gui-package-') as temp:
        root = Path(temp) / name
        if macos:
            contents = root / f'{title}.app' / 'Contents'
            destination = contents / 'MacOS'
            resources = contents / 'Resources'
            destination.mkdir(parents=True)
            resources.mkdir()
            with (contents / 'Info.plist').open('wb') as output:
                plistlib.dump({
                    'CFBundleName': title,
                    'CFBundleDisplayName': title,
                    'CFBundleIdentifier': f'org.astra.{binary}',
                    'CFBundleExecutable': binary,
                    'CFBundlePackageType': 'APPL',
                    'CFBundleShortVersionString': version,
                    'CFBundleVersion': version,
                    'NSHighResolutionCapable': True,
                    'LSUIElement': False,
                    'CFBundleIconFile': 'Astra.icns',
                    'LSMinimumSystemVersion': MACOS_MINIMUM_VERSION,
                }, output)
        else:
            destination = resources = root
            destination.mkdir(parents=True)
        if macos:
            shutil.copy2(ICONS / 'icon.icns', resources / 'Astra.icns')
        elif windows:
            shutil.copy2(ICONS / 'icon.ico', resources / f'{binary}.ico')
        else:
            shutil.copy2(ICONS / 'icon.png', resources / f'{binary}.png')
            shutil.copy2(ICONS / 'icon.svg', resources / f'{binary}.svg')
            (resources / f'{binary}.desktop').write_text(
                f'[Desktop Entry]\nType=Application\nName={title}\nComment=Astra network management\n'
                f'Exec={binary}\nIcon={binary}\nTerminal=false\nCategories=Network;Utility;\n'
                f'StartupWMClass={binary}\n', encoding='utf-8')
        for component in names:
            shutil.copy2(source / component, destination / component)
        if ROLES[role].example:
            shutil.copy2(ROOT / 'configs' / ROLES[role].example, resources / ROLES[role].example)
        if windows and role == 'client' and (source / 'wintun.dll').is_file():
            shutil.copy2(source / 'wintun.dll', destination / 'wintun.dll')
        # A flat binary payload remains directly importable and installable by the
        # existing manifest verifier. Never relax its archive traversal rules for .app.
        components = {}
        for component in [binary, *role_binaries((role,), target=target)]:
            package = 'astra-gui' if component in ('astra-gui', 'astra-server-gui') else COMPONENT_PACKAGES.get(component, component)
            components[component] = (versions or {}).get(package, version)
        if profile == 'release':
            separate_symbols(source, destination, names, target=target,
                             archive_path=output_dir / f'{name}-symbols.tar.gz',
                             build_identity={'frontend': binary, 'version': version,
                                             'role': role, 'components': components})
        files = [{'path': file.name, 'size': file.stat().st_size,
                  'sha256': sha256_file(file)}
                 for file in sorted(destination.iterdir()) if file.is_file()]
        identity = hashlib.sha256(json.dumps({'target': target, 'components': components, 'files': files}, sort_keys=True).encode()).hexdigest()[:16]
        manifest = {'schema_version': 1, 'role': role, 'release_id': f'{binary}-{version}-{identity}',
                    'target': target, 'host_protocol': {'min': 1, 'max': 1},
                    'components': components, 'files': files}
        (destination / 'manifest.json').write_text(json.dumps(manifest, indent=2) + '\n', encoding='utf-8')
        if role == 'client':
            with tarfile.open(output_dir / f'{binary}-update-{target}.tar.gz', 'w:gz') as update:
                update.add(destination, arcname='client')
        archive = output_dir / f'{name}.tar.gz'
        with tarfile.open(archive, 'w:gz') as output:
            output.add(root, arcname=name)
    return archive


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--profile', choices=('debug', 'release'), default='release')
    parser.add_argument('--output-dir', type=Path, default=Path('dist/gui'))
    args = parser.parse_args()
    layout, metadata = BuildLayout.discover(build_environment())
    source = layout.binary_dir(args.profile)
    version = next(package['version'] for package in metadata['packages'] if package['name'] == 'astra-gui')
    versions = {package['name']: package['version'] for package in metadata['packages']}
    for role, binary, title in FRONTENDS:
        print(package_frontend(source, args.output_dir, layout.target, role, binary, title, version, versions,
                               profile=args.profile))
        if args.profile == 'release':
            print(args.output_dir / f'{binary}-{layout.target}-symbols.tar.gz')
    print(args.output_dir / f'astra-gui-update-{layout.target}.tar.gz')


if __name__ == '__main__':
    main()
