#!/usr/bin/env python3
"""Isolated reuse of an externally created Wintun adapter; no capture routes."""
import argparse
import ctypes as C
from ctypes import wintypes as W
import json
import os
from pathlib import Path
import subprocess
import time
import traceback
from windows_modes_regression import Core, base, upstream, free_port, powershell


def run(args, directory):
    assert C.windll.shell32.IsUserAnAdmin(), 'requires an elevated test process'
    dll = C.WinDLL(str(args.bin_dir / 'wintun.dll'), use_last_error=True)
    dll.WintunCreateAdapter.argtypes = [W.LPCWSTR, W.LPCWSTR, C.c_void_p]
    dll.WintunCreateAdapter.restype = C.c_void_p
    dll.WintunCloseAdapter.argtypes = [C.c_void_p]
    dll.WintunStartSession.argtypes = [C.c_void_p, W.DWORD]
    dll.WintunStartSession.restype = C.c_void_p
    dll.WintunEndSession.argtypes = [C.c_void_p]
    name = f'AstraReuseTest{os.getpid()}'
    handle = dll.WintunCreateAdapter(name, 'Astra reuse regression', None)
    if not handle:
        raise C.WinError(C.get_last_error())
    checks = []
    try:
        index = powershell(f"(Get-NetAdapter -Name '{name}').ifIndex")
        with upstream(True) as server:
            config = base(server, free_port()) + f'''\n[tun]
name="{name}"
address="198.18.252.1"
prefix=30
ipv6_address="fd00:198:18:252::1"
ipv6_prefix=126
mtu=1500
routes=[]
'''
            core = Core(args, directory, config, 'reuse')
            try:
                core.ready()
                assert powershell(f"(Get-NetAdapter -Name '{name}').ifIndex") == index
                log = core.log_path.read_text(encoding='utf-8')
                assert 'Windows TUN session opened' in log and 'reused=true' in log, log
                assert powershell(f"@(Get-NetIPAddress -InterfaceIndex {index} -IPAddress '198.18.252.1').Count") == '1'
                assert powershell(f"@(Get-NetIPAddress -InterfaceIndex {index} -IPAddress 'fd00:198:18:252::1').Count") == '1'
                checks.append('reuse existing Wintun with unchanged interface index and configured IPv4/IPv6')
                assert core.command('--reload').returncode == 0
                checks.append('same-device reload reuses its active session')
                contender_config = base(server, free_port()) + '\n[tun]' + config.split('[tun]', 1)[1]
                contender = Core(args, directory, contender_config.replace('mtu=1500', 'mtu=1400').replace('198.18.252.1', '198.18.252.2'), 'busy')
                try:
                    contender.child.wait(timeout=15)
                    assert contender.child.returncode != 0
                    log = contender.log_path.read_text(encoding='utf-8')
                    assert 'open existing Windows TUN' in log, log
                    assert powershell(f"(Get-NetIPInterface -InterfaceIndex {index} -AddressFamily IPv4).NlMtu") == '1500'
                    assert powershell(f"@(Get-NetIPAddress -InterfaceIndex {index} -IPAddress '198.18.252.1').Count") == '1'
                    checks.append('busy-session failure preserves active adapter address and MTU')
                finally:
                    contender.close()
            finally:
                core.close()
            assert powershell(f"(Get-NetAdapter -Name '{name}').ifIndex") == index
            session = dll.WintunStartSession(handle, 0x200000)
            if not session:
                raise C.WinError(C.get_last_error())
            dll.WintunEndSession(session)
            checks.append('stop releases the session and preserves the external adapter')
    finally:
        dll.WintunCloseAdapter(handle)
    deadline = time.monotonic() + 10
    while powershell(f"@(Get-NetAdapter -Name '{name}' -ErrorAction SilentlyContinue).Count") != '0':
        assert time.monotonic() < deadline, 'owned test adapter leaked'
        time.sleep(.1)
    checks.append('external owner close removes only its test adapter')
    return checks


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bin-dir', type=Path, required=True)
    parser.add_argument('--output-dir', type=Path, required=True)
    args = parser.parse_args()
    args.bin_dir = args.bin_dir.resolve()
    directory = args.output_dir.resolve()
    directory.mkdir(parents=True, exist_ok=True)
    report = {'passed': False}
    try:
        report['checks'] = run(args, directory)
        report['passed'] = True
    except Exception:
        report['error'] = traceback.format_exc()
        raise
    finally:
        (directory / 'result.json').write_text(json.dumps(report, indent=2) + '\n', encoding='utf-8')
    print(json.dumps(report, indent=2))


if __name__ == '__main__':
    main()
