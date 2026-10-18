#!/usr/bin/env python3
"""Run Astra; standalone configurations only start the proxy core."""
import argparse
import os
import pathlib
import signal
import shutil
import subprocess
import tempfile
import time
from platform_support import executable, process_flags

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
    proxy_command = [str(binary_dir / executable('astra-proxy')), str(args.proxy.resolve())]
    if not pathlib.Path(proxy_command[0]).is_file():
        raise SystemExit('Build first: cargo build' + ('' if args.debug else ' --release'))
    # Ask the Rust parser instead of duplicating TOML parsing in Python.
    mode = subprocess.check_output([proxy_command[0], '--print-routing-mode', proxy_command[1]], cwd=ROOT, env=env, text=True).strip()
    if mode not in ('standalone', 'rules'):
        raise SystemExit('Unexpected routing mode from astra-proxy: ' + mode)
    commands = ([proxy_command] if mode == 'standalone' else
                [[str(binary_dir / executable('astra-rule')), str(args.rules.resolve())], proxy_command])
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
    if os.name == 'nt':
        signal.signal(signal.SIGBREAK, stop)
    children = []
    controls = []
    status = 0
    state_root = None
    if os.name == 'nt':
        state_root = pathlib.Path(os.environ.get('LOCALAPPDATA') or os.environ['USERPROFILE'])/'.astra/launchers'
        state_root.mkdir(parents=True, exist_ok=True)
    state = pathlib.Path(tempfile.mkdtemp(prefix='astra-launch-', dir=state_root))
    try:
        for index, command in enumerate(commands):
            child_env = dict(env)
            control = state / f'{index}.ctl'
            if os.name == 'nt':
                child_env['ASTRA_CONTROL_SOCKET'] = str(control)
            children.append(subprocess.Popen(command, cwd=ROOT, env=child_env, **process_flags()))
            controls.append(control)
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
        for child, command, control in reversed(list(zip(children, commands, controls))):
            if child.poll() is None:
                if os.name == 'nt':
                    try:
                        subprocess.run([command[0], '--stop', str(control)], check=True,
                                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=5)
                    except (OSError, subprocess.SubprocessError):
                        child.terminate()
                else:
                    child.terminate()
                try:
                    child.wait(timeout=30)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait()
        # A failed system-proxy restore must retain its durable recovery snapshot.
        if any(state.glob('*.system-proxy-state.toml')):
            print(f'System proxy recovery is pending in {state}; use --restore-system-proxy before restarting.')
        else:
            shutil.rmtree(state)
    raise SystemExit(status)

if __name__ == '__main__':
    main()
