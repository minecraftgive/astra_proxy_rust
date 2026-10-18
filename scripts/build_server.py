#!/usr/bin/env python3
"""Build the independent native server core and terminal manager."""
import argparse
import pathlib
import subprocess
from build import build_environment

ROOT=pathlib.Path(__file__).resolve().parents[1]
def main():
    parser=argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--debug',action='store_true')
    parser.add_argument('--offline',action='store_true')
    args=parser.parse_args()
    cmd=['cargo','build','--locked','-p','astra-server-core','-p','astra-server-tui']
    if not args.debug:cmd.append('--release')
    if args.offline:cmd.append('--offline')
    subprocess.run(cmd,cwd=ROOT,env=build_environment(),check=True)
if __name__=='__main__':main()
