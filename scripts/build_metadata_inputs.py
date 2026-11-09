#!/usr/bin/env python3
"""Regenerate/check local dependency watches after changing Cargo manifests."""
import argparse
from astra_tools.build_inputs import render_inputs
from astra_tools.workspace import ROOT


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--check', action='store_true')
    args = parser.parse_args()
    path = ROOT / 'crates/astra-platform/build_metadata_inputs.rs'
    content = render_inputs(ROOT)
    if args.check:
        if not path.is_file() or path.read_text() != content:
            parser.error('Metadata inputs are stale; run python scripts/build_metadata_inputs.py')
    elif not path.is_file() or path.read_text() != content:
        path.write_text(content)


if __name__ == '__main__':
    main()
