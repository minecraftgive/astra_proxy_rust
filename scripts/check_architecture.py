#!/usr/bin/env python3
"""Check dependency direction and platform boundaries without a Rust toolchain."""
from pathlib import Path
import ast
import re
import tomllib

ROOT = Path(__file__).resolve().parents[1]
errors = []
workspace = tomllib.loads((ROOT / 'Cargo.toml').read_text(encoding='utf-8'))
packages = {}
for member in workspace['workspace']['members']:
    path = ROOT / member / 'Cargo.toml'
    data = tomllib.loads(path.read_text(encoding='utf-8'))
    packages[data['package']['name']] = (path.parent, data)


def dependencies(data):
    sections = [data.get('dependencies', {}), data.get('build-dependencies', {})]
    for target in data.get('target', {}).values():
        sections.extend([target.get('dependencies', {}), target.get('build-dependencies', {})])
    return {spec.get('package', name) if isinstance(spec, dict) else name
            for section in sections for name, spec in section.items()}


graph = {name: dependencies(data) for name, (_, data) in packages.items()}


def reachable(name, visiting=()):
    if name in visiting:
        errors.append('Workspace dependency cycle: ' + ' -> '.join((*visiting, name)))
        return set()
    result = set(graph.get(name, ()))
    for dep in tuple(result):
        if dep in graph:
            result.update(reachable(dep, (*visiting, name)))
    return result


frontends = {'astra-tui', 'astra-tui-kit', 'ratatui', 'crossterm'}
application_cores = {'astra-proxy', 'astra-server-core', 'astra-rule', 'astra-manager'}
for name in graph:
    all_deps = reachable(name)
    if name not in ('astra-tui', 'astra-tui-kit') and all_deps & frontends:
        errors.append(f'Non-TUI package depends on terminal rendering: {name}: {sorted(all_deps & frontends)}')
    if name in ('astra-model', 'astra-platform', 'astra-resource', 'astra-protocols', 'astra-management', 'astra-app', 'astra-tui', 'astra-web', 'astra-tui-kit'):
        forbidden = all_deps & application_cores
        if forbidden:
            errors.append(f'Library/frontend depends on application core: {name}: {sorted(forbidden)}')
    if name == 'astra-model' and 'tokio' in all_deps:
        errors.append('astra-model must remain a synchronous data model')

# Executable regression scenarios cannot be used as utility libraries.
scripts = ROOT / 'scripts'
entrypoints = {p.stem for p in scripts.glob('*.py')}
for path in scripts.rglob('*.py'):
    if '__pycache__' in path.parts:
        continue
    for node in ast.walk(ast.parse(path.read_text(encoding='utf-8'))):
        modules = ([node.module] if isinstance(node, ast.ImportFrom) and node.module else
                   [alias.name for alias in node.names] if isinstance(node, ast.Import) else [])
        for module in modules:
            if module.split('.')[0] in entrypoints:
                errors.append(f'Script imports executable scenario: {path.relative_to(ROOT)} -> {module}')

web = ROOT / 'apps/web/src'
for path in web.rglob('*') if web.exists() else []:
    if path.suffix not in ('.ts', '.tsx', '.js', '.jsx') or any(part in ('__tests__', 'fixtures') for part in path.parts):
        continue
    text = path.read_text(encoding='utf-8')
    if re.search(r'from\s+[\"\x27][^\"\x27]*(?:/sim(?:\.|[\"\x27])|/model/data(?:\.|[\"\x27]))', text):
        errors.append(f'Production web imports simulated state: {path.relative_to(ROOT)}')
protocols = ROOT / "crates/astra-protocols"
manifest = tomllib.loads((protocols / "Cargo.toml").read_text(encoding="utf-8"))
for forbidden in ("astra-proxy", "astra-server-core", "astra-server-api"):
    if forbidden in manifest.get("dependencies", {}):
        errors.append(f"protocol library depends on application: {forbidden}")
for name in ("socks5", "http", "trojan", "vless", "vmess", "shadowsocks", "tuic", "hysteria2"):
    if not any(p.exists() for p in (protocols / "src" / f"{name}.rs", protocols / "src" / name / "mod.rs")):
        errors.append(f"missing protocol module: {name}")
os_pattern = re.compile(r"std::os::|windows_sys::|#\[cfg\([^\n]*(?:windows|unix|target_os)|cfg!\([^)]*(?:windows|unix|target_os)")
for package in ("astra-protocols", "astra-proxy", "astra-server-core"):
    source = ROOT / "crates" / package / "src"
    for path in source.rglob("*.rs"):
        if "platform" in path.relative_to(source).parts or path.name == 'tests.rs' or path.stem.endswith('_tests'):
            continue
        text = path.read_text(encoding="utf-8")
        # Unit-test modules may exercise OS-specific behavior. Production code
        # remains subject to the adaptation-layer rule.
        text = re.split(r'#\[cfg\((?:test\b|all\(test\b)', text, maxsplit=1)[0]
        if os_pattern.search(text):
            errors.append(f"OS dependency outside adaptation layer: {path.relative_to(ROOT)}")
    if package != "astra-protocols":
        entry = (source / "main.rs").read_text(encoding="utf-8")
        if len(entry.splitlines()) > 8 or "mod " in entry:
            errors.append(f"entrypoint must only start the application library: {package}")
        deps = tomllib.loads((source.parent / "Cargo.toml").read_text(encoding="utf-8"))["dependencies"]
        if "astra-protocols" not in deps:
            errors.append(f"application does not consume shared protocols: {package}")
if errors:
    raise SystemExit("\n".join(errors))
print("Architecture boundaries OK: dependency direction, shared protocols, isolated OS adaptations, independent scenarios.")
