#!/usr/bin/env python3
"""Check dependency direction and platform boundaries without a Rust toolchain."""
from pathlib import Path
import re
import tomllib

ROOT = Path(__file__).resolve().parents[1]
errors = []
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
        if "platform" in path.relative_to(source).parts:
            continue
        text = path.read_text(encoding="utf-8")
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
print("Architecture boundaries OK: shared protocols, thin entrypoints, isolated OS adaptations.")
