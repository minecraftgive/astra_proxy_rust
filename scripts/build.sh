#!/usr/bin/env bash
# Build and distribute all three native Astra executables on the current host.
set -euo pipefail

ROOT="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)"
profile=release
build_only=false
skip_tests=false
output_dir="$ROOT/dist"
label=""

usage() {
    cat <<'HELP'
Usage: scripts/build.sh [options]

Build astra-proxy, astra-rule and astra-tui together, then create and verify
one distribution archive containing all three executables.

  --debug             Use the debug profile (default: release)
  --build-only        Build and test without creating an archive
  --skip-tests        Skip workspace, live IPC and archive tests
  --output-dir DIR    Archive directory (default: project/dist)
  --label VERSION     Archive label (default: workspace package version)
  -h, --help          Show this help

Requires Rust, CMake, Clang/C++, libclang and Python 3 at build time.
The three distributed Rust executables do not require Python at runtime.
CARGO_TARGET_DIR and PYTHON may override build output and Python interpreter.
HELP
}

while (($#)); do
    case "$1" in
        --debug) profile=debug; shift ;;
        --build-only) build_only=true; shift ;;
        --skip-tests) skip_tests=true; shift ;;
        --output-dir|--label)
            if (($# < 2)) || [[ -z "$2" ]]; then
                printf 'Missing value for %s\n' "$1" >&2
                exit 2
            fi
            if [[ "$1" == --output-dir ]]; then output_dir="$2"; else label="$2"; fi
            shift 2 ;;
        -h|--help) usage; exit 0 ;;
        *) printf 'Unknown option: %s\n' "$1" >&2; usage >&2; exit 2 ;;
    esac
done

# Resolve relative output paths against the caller's working directory.
if [[ "$output_dir" != /* ]]; then output_dir="$PWD/$output_dir"; fi
cd "$ROOT"
python="${PYTHON:-python3}"
for tool in cargo rustc "$python"; do
    if ! command -v "$tool" >/dev/null 2>&1; then
        printf 'Required build tool is missing: %s\n' "$tool" >&2
        exit 1
    fi
done
"$python" -c 'import sys; assert sys.version_info >= (3, 8), "Python 3.8+ required"'
# Reuse the repository's optional local CMake installation when available.
if ! command -v cmake >/dev/null 2>&1 && [[ -x "$ROOT/.build-tools/bin/cmake" ]]; then
    export PATH="$ROOT/.build-tools/bin:$PATH"
fi
if ! command -v cmake >/dev/null 2>&1; then
    printf 'CMake is required to build the native TLS dependencies.\n' >&2
    exit 1
fi

build_info="$(cargo metadata --locked --no-deps --format-version 1 | "$python" -c '
import json, sys
m = json.load(sys.stdin)
p = [p for p in m["packages"] if p["name"] in ("astra-proxy", "astra-rule", "astra-tui")]
assert len(p) == 3, "All three Astra packages must be workspace members"
assert len({p["version"] for p in p}) == 1, "Astra package versions must agree"
print(p[0]["version"])
print(m["target_directory"])
')"
if [[ -z "$label" ]]; then label="${build_info%%$'\n'*}"; fi
if [[ ! "$label" =~ ^[a-zA-Z0-9.-]+$ ]]; then
    printf 'Label must contain only letters, numbers, dots and hyphens.\n' >&2
    exit 2
fi
target_dir="${build_info#*$'\n'}"
# This script creates native host packages; it does not label cross builds as host builds.
build_target="${CARGO_BUILD_TARGET:-$(rustc -vV | sed -n 's/^host: //p')}"
host_target="$(rustc -vV | sed -n 's/^host: //p')"
if [[ "$build_target" != "$host_target" ]]; then
    printf 'This distribution script requires a native build; unset CARGO_BUILD_TARGET.\n' >&2
    exit 2
fi
cargo_profile=(--profile dev)
if [[ "$profile" == release ]]; then cargo_profile=(--release); fi
# Explicit --target avoids a .cargo/config.toml default silently changing the platform.
bin_dir="$target_dir/$host_target/$profile"
printf 'Building astra-proxy, astra-rule and astra-tui (%s, %s)…\n' "$profile" "$host_target"
cargo build --locked --workspace --bins --target "$host_target" "${cargo_profile[@]}"
for binary in astra-proxy astra-rule astra-tui; do
    if [[ ! -x "$bin_dir/$binary" ]]; then
        printf 'Expected executable is missing: %s\n' "$bin_dir/$binary" >&2
        exit 1
    fi
done
if [[ "$skip_tests" == false ]]; then
    cargo test --locked --workspace --target "$host_target" "${cargo_profile[@]}"
    cargo test --locked -p astra-tui --target "$host_target" "${cargo_profile[@]}" live_ -- --ignored
fi
if [[ "$build_only" == true ]]; then
    printf 'Built all three executables in %s\n' "$bin_dir"
    exit 0
fi
"$python" "$ROOT/scripts/package_release.py" --bin-dir "$bin_dir" --output-dir "$output_dir" --label "$label"
archive="$output_dir/astra-$label-$("$python" -c 'import platform; print(platform.system().lower() + "-" + platform.machine())').tar.gz"
if [[ "$skip_tests" == false ]]; then
    "$python" "$ROOT/scripts/package_regression.py" "$archive" --report "$archive.verification.json"
fi
printf '\nDistribution: %s\nChecksum:     %s.sha256\n' "$archive" "$archive"
