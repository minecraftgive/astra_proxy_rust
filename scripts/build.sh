#!/usr/bin/env bash
# Linux/macOS build entry point; options are those of build.py. Example: scripts/build.sh --package
set -euo pipefail
exec "${PYTHON:-python3}" "$(dirname -- "${BASH_SOURCE[0]}")/build.py" "$@"
