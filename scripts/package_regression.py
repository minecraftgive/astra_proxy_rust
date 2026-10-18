#!/usr/bin/env python3
"""Verify a local Astra archive, then boot its launcher and relay an echo."""
import argparse
import hashlib
import json
import os
import pathlib
import socket
import subprocess
import sys
import tarfile
import tempfile
import threading
from protocol_matrix import EchoServer, Echo, free_port, exact, process, wait_port
from core_regression import socks, wait_socket


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('archive', type=pathlib.Path)
    parser.add_argument('--report', type=pathlib.Path)
    args = parser.parse_args()
    archive = args.archive.resolve()
    if args.report:
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_text(json.dumps({'archive':archive.name,'passed':False})+'\n')
    expected = archive.with_suffix(archive.suffix+'.sha256').read_text().split()[0]
    assert hashlib.sha256(archive.read_bytes()).hexdigest() == expected
    with tempfile.TemporaryDirectory(prefix='astra-unpacked-') as td, EchoServer(('127.0.0.1',0), Echo) as echo:
        directory = pathlib.Path(td)
        with tarfile.open(archive) as tar:
            for member in tar.getmembers():
                path = pathlib.PurePosixPath(member.name)
                assert not path.is_absolute() and '..' not in path.parts
                assert member.isfile() or member.isdir()
            tar.extractall(directory)
        root, = directory.iterdir()
        manifest = json.loads((root/'manifest.json').read_text())
        for binary in ('astra-rule', 'astra-proxy', 'astra-tui'):
            assert f'bin/{binary}' in manifest['files'], f'{binary} missing from distribution'
            assert os.access(root/'bin'/binary, os.X_OK), f'{binary} is not executable'
        for name, digest in manifest['files'].items():
            path = root / name
            assert path.is_file() and hashlib.sha256(path.read_bytes()).hexdigest() == digest, name
        tui = str(root/'bin/astra-tui')
        subprocess.run([tui, '--help'], check=True, cwd=directory, timeout=10)
        # A short state path fits macOS UDS limits even when extraction uses /var/folders.
        with tempfile.TemporaryDirectory(prefix='astra-tui-package-', dir='/tmp') as state:
            state = pathlib.Path(state)
            with process([tui, '--manager', '--state-dir', str(state)], directory, 'tui-manager') as (child, log):
                wait_socket(state/'manager.sock', child, log)
                subprocess.run([tui, '--shutdown', '--state-dir', str(state)], check=True, cwd=directory, timeout=10)
                assert child.wait(timeout=5) == 0, log.read_text()
                assert not (state/'manager.sock').exists(), 'TUI manager left a stale socket'
        launcher = [sys.executable, str(root/'scripts/astra.py')]
        subprocess.run(launcher+['--check'], check=True, cwd=directory)
        for example in ('native.toml','tuic.toml'):
            subprocess.run(launcher+['--check','--proxy',str(root/'configs'/example),'--rules',str(root/'configs/proxy-all-rule.toml')],check=True,cwd=directory)
        port = free_port()
        sock = directory/'rules.sock'
        rules = directory/'rules.toml'
        proxy = directory/'proxy.toml'
        rules.write_text(f'socket="{sock}"\ndefault_action="direct"\n')
        proxy.write_text(f'rule_socket="{sock}"\nsocks5_listen="127.0.0.1:{port}"\nshutdown_timeout_secs=1\n')
        threading.Thread(target=echo.serve_forever, daemon=True).start()
        with process(launcher+['--proxy',str(proxy),'--rules',str(rules)], directory, 'launcher') as (child, log):
            wait_port(port, child, log)
            client, status = socks(port, echo.server_address[1])
            with client:
                assert status == 0
                payload = bytes(range(256))*256
                client.sendall(payload)
                assert exact(client, len(payload)) == payload
                client.shutdown(socket.SHUT_WR)
                assert client.recv(1) == b''
            child.terminate()
            assert child.wait(timeout=5) == 0, log.read_text()
        assert not sock.exists(), 'packaged shutdown left a stale rule socket'
        echo.shutdown()
    if args.report:
        args.report.write_text(json.dumps({'archive':archive.name,'archive_sha256':expected,'passed':True,'checks':['archive checksum','three executable binaries','manifest file hashes','TUI help and manager IPC shutdown','default/native/TUIC example configuration checks','packaged launcher from a separate directory','65536-byte SOCKS echo and EOF','graceful exit and UDS cleanup']},indent=2)+'\n')
    print('PASS three binaries, archive checksums, TUI manager IPC, configuration checks, launcher, 64 KiB SOCKS echo and graceful cleanup')


if __name__ == '__main__':
    main()
