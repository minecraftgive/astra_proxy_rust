"""Workspace paths and native build environment shared by all command entrypoints."""
from dataclasses import dataclass
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess

ROOT = Path(__file__).resolve().parents[2]
# The workspace defines exactly these two Cargo profiles (see Cargo.toml).
CARGO_PROFILES = {'debug': 'dev', 'release': 'release'}


def cargo_profile(profile='release'):
    """Map a logical artifact profile (debug/release) to its Cargo profile (dev/release)."""
    try:
        return CARGO_PROFILES[profile]
    except KeyError:
        raise ValueError(f'Unsupported build profile: {profile}') from None


def reference_engine_dir(explicit=None, *, env=None):
    """Resolve one test-only engine cache without replacing another platform's files."""
    env = os.environ if env is None else env
    selected = explicit or env.get('ASTRA_TEST_ENGINES_DIR')
    return Path(selected or ROOT / '.test-tools' / ('windows-reference-bin' if os.name == 'nt' else 'bin')).resolve()


def reference_engine(name, directory=None):
    from .platform import executable
    if name not in ('xray', 'sing-box'):
        raise ValueError(f'Unknown reference engine: {name}')
    path = reference_engine_dir(directory) / executable(name)
    if not path.is_file():
        raise FileNotFoundError(f'Test engine missing: {path}; run scripts/install_test_servers.py --destination {path.parent}')
    with path.open('rb') as stream:
        magic = stream.read(4)
    if os.name == 'nt' and magic[:2] != b'MZ':
        raise RuntimeError(f'Test engine is not a Windows executable: {path} (magic {magic.hex()}); preserve it and install a Windows cache separately')
    return path


def _windows_toolchain(env):
    """Select an installed compiler, rather than a removed VS preview instance."""
    installer = Path(env.get('ProgramFiles(x86)', 'C:/Program Files (x86)')) / 'Microsoft Visual Studio/Installer/vswhere.exe'
    if not installer.is_file():
        return
    result = subprocess.run([str(installer), '-all', '-products', '*', '-format', 'json'],
                            capture_output=True, text=True, check=True)
    arch = 'arm64' if platform.machine().lower() in ('arm64', 'aarch64') else 'x64'
    instances = sorted(json.loads(result.stdout), key=lambda item: tuple(map(int, item['installationVersion'].split('.'))), reverse=True)
    for instance in instances:
        installation = Path(instance['installationPath'])
        compilers = sorted((installation / 'VC/Tools/MSVC').glob(f'*/bin/Hostx64/{arch}/cl.exe'), reverse=True)
        setup = installation / 'Common7/Tools/VsDevCmd.bat'
        if not compilers or not setup.is_file():
            continue
        # The path comes from vswhere, not a user command; reject cmd metacharacters.
        if any(c in str(setup) for c in '\r\n"&|<>^%'):
            raise ValueError(f'Unsupported Visual Studio installation path: {installation}')
        command = f'call "{setup}" -no_logo -arch={arch} -host_arch=x64 >nul && set'
        loaded = subprocess.run(f'"{env.get("COMSPEC", "cmd.exe")}" /d /s /c "{command}"',
                                env=env, capture_output=True, text=True, check=True)
        allowed = {'path', 'include', 'lib', 'libpath', 'vsinstalldir', 'vcinstalldir',
                   'vctoolsinstalldir', 'vctoolsversion', 'windowssdkdir', 'windowssdkversion',
                   'universalcrtsdkdir', 'ucrtversion', 'vscmd_arg_tgt_arch', 'vscmd_arg_host_arch'}
        for line in loaded.stdout.splitlines():
            key, separator, value = line.partition('=')
            if separator and key.lower() in allowed:
                for previous in list(env):
                    if previous.lower() == key.lower():
                        del env[previous]
                env['PATH' if key.lower() == 'path' else key] = value
        for key in ('CC', 'CXX'):
            if not env.get(key):
                env[key] = str(compilers[0])
        if not env.get('CMAKE_GENERATOR') or env['CMAKE_GENERATOR'].startswith('Visual Studio'):
            major = int(instance['installationVersion'].split('.')[0])
            generator = {16: 'Visual Studio 16 2019', 17: 'Visual Studio 17 2022', 18: 'Visual Studio 18 2026'}.get(major)
            if generator:
                env.setdefault('CMAKE_GENERATOR', generator)
            env.setdefault('CMAKE_GENERATOR_INSTANCE', str(installation))
            # cmake-rs passes -G, which can bypass CMake's instance environment
            # default. This documented VS hint also selects the discovered
            # installation when the generator is explicit.
            env[f'VS{major}0COMNTOOLS'] = str(installation / 'Common7/Tools')
        return
    raise RuntimeError('No installed Visual Studio instance has a usable C++ compiler')


def build_environment():
    env = dict(os.environ)
    local = ROOT / '.build-tools/windows'
    if os.name == 'nt':
        _windows_toolchain(env)
        paths = [p.parent for p in local.glob('nasm-*/nasm.exe')]
        paths += [local / 'python/bin']
        env['PATH'] = os.pathsep.join(map(str, paths)) + os.pathsep + env.get('PATH', '')
        if not env.get('LIBCLANG_PATH'):
            for candidate in (local / 'python/clang/native', Path('C:/Program Files/LLVM/bin')):
                if (candidate / 'libclang.dll').is_file():
                    env['LIBCLANG_PATH'] = str(candidate)
                    break
    elif not shutil.which('cmake') and (ROOT / '.build-tools/bin/cmake').is_file():
        env['PATH'] = str(ROOT / '.build-tools/bin') + os.pathsep + env.get('PATH', '')
    return env


def host_target(env):
    info = subprocess.check_output(['rustc', '-vV'], cwd=ROOT, env=env, text=True)
    return next(line[6:] for line in info.splitlines() if line.startswith('host: '))


@dataclass(frozen=True)
class BuildLayout:
    target_directory: Path
    target: str

    @classmethod
    def discover(cls, env):
        metadata = json.loads(subprocess.check_output(
            ['cargo', 'metadata', '--locked', '--no-deps', '--format-version', '1'],
            cwd=ROOT, env=env, text=True))
        return cls(Path(metadata['target_directory']), host_target(env)), metadata

    def binary_dir(self, profile='release'):
        cargo_profile(profile)
        return self.target_directory / self.target / profile


def binary_dir(env, debug):
    layout, metadata = BuildLayout.discover(env)
    return layout.binary_dir('debug' if debug else 'release'), metadata


def resolve_binary_dir(explicit=None, *, artifact_dir=None, profile='release', role=None, env=None):
    cargo_profile(profile)
    if artifact_dir is not None:
        from .artifacts import ArtifactIndex
        entries = ArtifactIndex.load(artifact_dir).select(profile=profile, role=role)
        if explicit is not None:
            return Path(explicit).resolve()
        paths = {Path(entry['binary_dir']).resolve() for entry in entries}
        if len(paths) != 1:
            raise ValueError('Artifact selection must identify one binary directory; choose --role and --debug')
        return paths.pop()
    if explicit is not None:
        return Path(explicit).resolve()
    layout, _ = BuildLayout.discover(env or build_environment())
    return layout.binary_dir(profile)
