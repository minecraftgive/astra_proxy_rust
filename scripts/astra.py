#!/usr/bin/env python3
"""Run Astra; standalone configurations only start the proxy core."""
import argparse
import os
import pathlib
import signal
import subprocess
import time

ROOT = pathlib.Path(__file__).resolve().parents[1]

def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--proxy', type=pathlib.Path, default=ROOT / 'configs/proxy.toml')
    parser.add_argument('--rules', type=pathlib.Path, default=ROOT / 'configs/rule.toml')
    parser.add_argument('--debug', action='store_true')
    parser.add_argument('--bin-dir', type=pathlib.Path, help='directory containing Astra executables (only astra-proxy is needed for standalone)')
    parser.add_argument('--check', action='store_true')
    args = parser.parse_args()
    binary_dir = (args.bin_dir.resolve() if args.bin_dir else
                  ROOT / 'bin' if not args.debug and (ROOT / 'bin').is_dir() else
                  ROOT / 'target' / ('debug' if args.debug else 'release'))
    env = dict(os.environ)
    proxy_command = [str(binary_dir / 'astra-proxy'), str(args.proxy.resolve())]
    if not pathlib.Path(proxy_command[0]).is_file():
        raise SystemExit('Build first: cargo build' + ('' if args.debug else ' --release'))
    # Ask the Rust parser instead of duplicating TOML parsing in Python.
    mode = subprocess.check_output([proxy_command[0], '--print-routing-mode', proxy_command[1]], cwd=ROOT, env=env, text=True).strip()
    if mode not in ('standalone', 'rules'):
        raise SystemExit('Unexpected routing mode from astra-proxy: ' + mode)
    commands = ([proxy_command] if mode == 'standalone' else
                [[str(binary_dir / 'astra-rule'), str(args.rules.resolve())], proxy_command])
    for command in commands:
        if not pathlib.Path(command[0]).is_file():
            raise SystemExit('Build first: cargo build' + ('' if args.debug else ' --release'))
        subprocess.run([command[0], '--check', command[1]], check=True, cwd=ROOT, env=env)
    if args.check:
        return
    stopping = False
    def stop(_signum, _frame):
        nonlocal stopping
        stopping = True
    signal.signal(signal.SIGINT, stop)
    signal.signal(signal.SIGTERM, stop)
    children = []
    status = 0
    try:
        for command in commands:
            children.append(subprocess.Popen(command, cwd=ROOT, env=env))
        while not stopping:
            for child in children:
                result = child.poll()
                if result is not None:
                    status = result or 1
                    stopping = True
                    break
            time.sleep(.1)
    finally:
        # Proxy drains first while the rule core remains available.
        for child in reversed(children):
            if child.poll() is None:
                child.terminate()
                try:
                    child.wait(timeout=30)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait()
    raise SystemExit(status)

if __name__ == '__main__':
    main()
