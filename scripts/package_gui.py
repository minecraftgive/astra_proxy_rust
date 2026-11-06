#!/usr/bin/env python3
"""Package desktop frontends with their existing client/server runtime binaries."""
import argparse
from pathlib import Path
import plistlib
import shutil
import tarfile
import tempfile

from astra_tools.artifacts import ROLES, role_binaries
from astra_tools.workspace import ROOT, BuildLayout, build_environment

# Match the deployment target used by the desktop CI build and GPUI's shaders.
MACOS_MINIMUM_VERSION = '12.0'
FRONTENDS = (('client', 'astra-gui', 'Astra'), ('server', 'astra-server-gui', 'Astra Server'))


def package_frontend(source, output_dir, target, role, binary, title, version):
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
                    'LSMinimumSystemVersion': MACOS_MINIMUM_VERSION,
                }, output)
        else:
            destination = resources = root
            destination.mkdir(parents=True)
        for component in names:
            shutil.copy2(source / component, destination / component)
        if ROLES[role].example:
            shutil.copy2(ROOT / 'configs' / ROLES[role].example, resources / ROLES[role].example)
        if windows and role == 'client' and (source / 'wintun.dll').is_file():
            shutil.copy2(source / 'wintun.dll', destination / 'wintun.dll')
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
    for role, binary, title in FRONTENDS:
        print(package_frontend(source, args.output_dir, layout.target, role, binary, title, version))


if __name__ == '__main__':
    main()
