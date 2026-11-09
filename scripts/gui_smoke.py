#!/usr/bin/env python3
"""Open a client/server GUI against an isolated manager and verify graceful exit."""
from __future__ import annotations
import argparse
import json
import os
import plistlib
import shutil
import sys
from pathlib import Path
import signal
import subprocess
import tempfile
import time


def wait_for(predicate, process, timeout, description):
    end = time.monotonic() + timeout
    while time.monotonic() < end:
        if process.poll() is not None:
            raise RuntimeError(f'{description}: process exited {process.returncode}')
        if predicate():
            return
        time.sleep(0.05)
    raise TimeoutError(description)


def smoke(binary_dir, duration=5, termination="signal", frontend="astra-gui"):
    binary_dir = Path(binary_dir).resolve()
    host, gui = binary_dir / 'astra-host', binary_dir / frontend
    for executable in (host, gui):
        if not executable.is_file():
            raise FileNotFoundError(executable)
    if os.name == 'nt':
        raise RuntimeError('The SIGTERM window smoke runs on Unix; Windows uses native GUI tests.')
    with tempfile.TemporaryDirectory(prefix='ag-', dir='/tmp') as temp:
        root = Path(temp)
        state, config, gui_state = root / 'manager', root / 'config', root / 'gui'
        for directory in (state, config, gui_state, gui_state / 'restart-ready'):
            directory.mkdir(mode=0o700)
        marker = gui_state / 'restart-ready' / 'smoke.ready'
        bundle = None
        if termination == 'native':
            if sys.platform != 'darwin':
                raise RuntimeError('Native application quit smoke is macOS-only')
            bundle = root / 'Astra Quit Smoke.app'
            contents = bundle / 'Contents'
            program = contents / 'MacOS' / frontend
            program.parent.mkdir(parents=True)
            shutil.copy2(gui, program)
            with (contents / 'Info.plist').open('wb') as info:
                plistlib.dump({'CFBundleIdentifier': 'org.astra.quit-smoke.' + root.name,
                              'CFBundleExecutable': frontend, 'CFBundleName': 'Astra Quit Smoke',
                              'CFBundlePackageType': 'APPL'}, info)
            gui = program
        host_args = [str(host), '--state-dir', str(state), '--config-dir', str(config), '--bin-dir', str(binary_dir)]
        log_path = root / 'smoke.log'
        app = manager = None
        with log_path.open('w+', encoding='utf-8') as log:
            try:
                manager = subprocess.Popen(host_args, stdout=log, stderr=log)
                socket = state / 'management.sock'
                wait_for(socket.exists, manager, 20, 'Isolated manager did not become ready')
                env = dict(os.environ)
                env.pop('WAYLAND_DISPLAY', None)  # Xvfb is an X11 display, independent of the runner desktop.
                app = subprocess.Popen([str(gui), '--endpoint', str(socket), '--state-dir', str(gui_state),
                                        '--config-dir', str(config), '--bin-dir', str(binary_dir),
                                        '--resume-ui', '--restart-ready', str(marker)],
                                       stdout=log, stderr=log, env=env)
                wait_for(marker.is_file, app, 30, 'Native window did not paint its ready frame')
                if marker.read_bytes() != b'astra-tui-ready-v1\n':
                    raise AssertionError('Invalid native-window readiness marker')
                time.sleep(duration)
                if bundle is None:
                    app.send_signal(signal.SIGTERM)
                else:
                    escaped = str(bundle).replace('\\', '\\\\').replace('"', '\\"')
                    quit_request = subprocess.run(['/usr/bin/osascript', '-e', f'tell application "{escaped}" to quit'],
                                                  capture_output=True, text=True, timeout=10)
                    # The approval callback intentionally returns NSTerminateCancel
                    # while the client saves, then stops the loop itself after ack.
                    if quit_request.returncode and '(-128)' not in quit_request.stderr:
                        raise RuntimeError(quit_request.stderr)
                if app.wait(timeout=20) != 0:
                    raise AssertionError(f'GUI did not exit gracefully: {app.returncode}')
                lifecycle = (gui_state / 'gui.log').read_text(encoding='utf-8')
                if 'native event loop and backend stopped' not in lifecycle:
                    raise AssertionError('Native termination bypassed Rust backend teardown')
                session = json.loads((gui_state / 'ui-session.json').read_text(encoding='utf-8'))
                if not session.get('gui_windows'):
                    raise AssertionError('SIGTERM did not save the open native windows')
                print(f'GUI window painted, saved its session, completed backend teardown, and exited with status 0 after {termination} quit.')
            except BaseException:
                log.flush()
                print(log_path.read_text(encoding='utf-8', errors='replace'))
                print('Isolated state files:', [str(path.relative_to(root)) for path in root.rglob('*') if path.is_file()])
                raise
            finally:
                if app is not None and app.poll() is None:
                    app.kill()
                    app.wait(timeout=10)
                if manager is not None and manager.poll() is None:
                    subprocess.run([*host_args, '--shutdown'], stdout=log, stderr=log, timeout=10, check=False)
                    try:
                        manager.wait(timeout=10)
                    except subprocess.TimeoutExpired:
                        manager.kill()
                        manager.wait(timeout=10)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bin-dir', required=True, type=Path)
    parser.add_argument('--duration', type=float, default=5)
    parser.add_argument('--termination', choices=('signal', 'native'), default='signal')
    parser.add_argument('--frontend', choices=('astra-gui', 'astra-server-gui'), default='astra-gui')
    args = parser.parse_args()
    smoke(args.bin_dir, args.duration, args.termination, args.frontend)

if __name__ == '__main__':
    main()
