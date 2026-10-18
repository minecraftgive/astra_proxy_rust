#!/usr/bin/env python3
"""Verify a local Astra archive, then boot its native client cores and relay an echo."""
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
from platform_support import executable, interrupt


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('archive', type=pathlib.Path)
    parser.add_argument('--report', type=pathlib.Path)
    args = parser.parse_args()
    archive = args.archive.resolve()
    if args.report:
        args.report.parent.mkdir(parents=True, exist_ok=True)
        args.report.write_text(json.dumps({'archive':archive.name,'passed':False})+'\n')
    expected = hashlib.sha256(archive.read_bytes()).hexdigest()
    with tempfile.TemporaryDirectory(prefix='astra-unpacked-') as td, EchoServer(('127.0.0.1',0), Echo) as echo:
        directory = pathlib.Path(td)
        with tarfile.open(archive) as tar:
            for member in tar.getmembers():
                path = pathlib.PurePosixPath(member.name)
                assert not path.is_absolute() and '..' not in path.parts
                assert member.isfile() or member.isdir()
            tar.extractall(directory, filter="data")
        root, = directory.iterdir()
        expected_files = {executable(n) for n in ('astra-rule','astra-proxy','astra-tui','astra-convert')} | {'client.example.toml'}
        if os.name == 'nt' and (root/'wintun.dll').exists():expected_files.add('wintun.dll')
        actual = {p.relative_to(root).as_posix() for p in root.rglob('*') if p.is_file()}
        assert root.name == 'client' and actual == expected_files and {p.name for p in root.iterdir()} == expected_files, actual
        subprocess.run([str(root/executable('astra-tui')), '--help'],check=True,capture_output=True,timeout=10)
        subprocess.run([str(root/executable('astra-proxy')), '--check', str(root/'client.example.toml')],check=True,timeout=15)
        # The single client example contains a commented complete Rule Core template.
        text = (root/'client.example.toml').read_text(encoding='utf-8')
        appendix = text.split('# BEGIN RULE CONFIG\n',1)[1].split('# END RULE CONFIG',1)[0]
        template = directory/'rule-template.toml'
        template.write_text('\n'.join(line[2:] for line in appendix.splitlines()),encoding='utf-8')
        subprocess.run([str(root/executable('astra-rule')),'--check',str(template)],check=True,timeout=10)
        tui = str(root/executable('astra-tui'))
        # A short state path fits macOS UDS limits even when extraction uses /var/folders.
        with tempfile.TemporaryDirectory(prefix='astra-tui-package-', dir=None if os.name == 'nt' else '/tmp') as state:
            state = pathlib.Path(state)
            with process([tui, '--manager', '--state-dir', str(state)], directory, 'tui-manager') as (child, log):
                wait_socket(state/'manager.sock', child, log)
                subprocess.run([tui, '--shutdown', '--state-dir', str(state)], check=True, cwd=directory, timeout=10)
                assert child.wait(timeout=5) == 0, log.read_text()
                assert not (state/'manager.sock').exists(), 'TUI manager left a stale socket'
        port = free_port()
        sock = directory/'rules.sock'
        rules = directory/'rules.toml'
        proxy = directory/'proxy.toml'
        rules.write_text(f'socket={json.dumps(sock.as_posix())}\ndefault_action="direct"\n', encoding='utf-8')
        proxy.write_text(f'rule_socket={json.dumps(sock.as_posix())}\nsocks5_listen="127.0.0.1:{port}"\nshutdown_timeout_secs=1\n', encoding='utf-8')
        threading.Thread(target=echo.serve_forever, daemon=True).start()
        with process([str(root/executable('astra-rule')),str(rules)], directory, 'rule') as (rule_child, rule_log), process([str(root/executable('astra-proxy')),str(proxy)], directory, 'proxy') as (child, log):
            wait_socket(sock, rule_child, rule_log)
            wait_port(port, child, log)
            client, status = socks(port, echo.server_address[1])
            with client:
                assert status == 0
                payload = bytes(range(256))*256
                client.sendall(payload)
                assert exact(client, len(payload)) == payload
                client.shutdown(socket.SHUT_WR)
                assert client.recv(1) == b''
            interrupt(child)
            assert child.wait(timeout=5) == 0, log.read_text()
            interrupt(rule_child)
            assert rule_child.wait(timeout=5) == 0, rule_log.read_text()
        assert not sock.exists(), 'packaged shutdown left a stale rule socket'
        echo.shutdown()
    if args.report:
        args.report.write_text(json.dumps({'archive':archive.name,'archive_sha256':expected,'passed':True,'checks':['archive checksum','four executable binaries','strict minimal client file list','TUI help and manager IPC shutdown','full proxy example and embedded full rule template checks','native cores from a separate directory','65536-byte SOCKS echo and EOF','graceful exit and UDS cleanup']},indent=2)+'\n')
    print('PASS minimal client archive, native executables, TUI IPC, full configuration, 64 KiB SOCKS echo and cleanup')


if __name__ == '__main__':
    main()
