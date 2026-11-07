#!/bin/sh
# Linux/macOS/Unix build entry point; options are those of build.py, the same
# as scripts/build.ps1. Example: ./scripts/build.sh --debug
set -eu
script_dir=$(CDPATH= cd -- "$(dirname -- "$0")" && pwd)
supported='import sys; sys.exit(sys.version_info < (3, 11))'
if [ -n "${PYTHON:-}" ]; then
    interpreter=$PYTHON
else
    interpreter=
    for candidate in python3 python3.14 python3.13 python3.12 python3.11 python; do
        if command -v "$candidate" >/dev/null 2>&1 && "$candidate" -c "$supported" 2>/dev/null; then
            interpreter=$candidate
            break
        fi
    done
    if [ -z "$interpreter" ]; then
        echo 'Python 3.11+ is required to build; install it or set PYTHON. See docs/INSTALL.md' >&2
        exit 127
    fi
fi
exec "$interpreter" "$script_dir/build.py" "$@"
