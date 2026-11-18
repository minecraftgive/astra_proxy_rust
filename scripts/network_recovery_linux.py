#!/usr/bin/env python3
"""Exercise the real core in disposable Linux net, mount and PID namespaces.

Run as root in a test VM with iproute2, unshare and /dev/net/tun. Pass
--resolved and --resolvectl to exercise real per-link DNS on a private bus.
"""
import argparse
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import sys
import tempfile
import time


def run(*args, **kwargs):
    return subprocess.run(args, capture_output=True, check=True, text=True, timeout=20, **kwargs).stdout


def wait(check, message):
    end = time.monotonic() + 10
    while time.monotonic() < end:
        if check():
            return
        time.sleep(.04)
    raise RuntimeError(message)


def network():
    links = {row['ifname'] for row in json.loads(run('ip', '-j', 'link'))}
    routes = set(run('ip', '-o', 'route').splitlines())
    return links, routes


def private_resolver(args):
    root = Path('/run')
    (root / 'systemd/resolve').mkdir(parents=True)
    run('chown', 'systemd-resolve:systemd-resolve', root / 'systemd/resolve')
    (root / 'dbus').mkdir()
    conf = root / 'dbus/test.conf'
    conf.write_text('<busconfig><type>system</type><listen>unix:path=/run/dbus/system_bus_socket</listen>'
                    '<auth>EXTERNAL</auth><policy context="default"><allow user="*"/><allow own="*"/>'
                    '<allow send_destination="*"/><allow receive_sender="*"/></policy></busconfig>')
    run('dbus-daemon', '--config-file=' + str(conf), '--fork')
    os.environ['DBUS_SYSTEM_BUS_ADDRESS'] = 'unix:path=/run/dbus/system_bus_socket'
    os.environ['PATH'] = str(args.resolvectl.parent) + os.pathsep + os.environ['PATH']
    subprocess.Popen([args.resolved], stdout=subprocess.DEVNULL, stderr=sys.stderr,
                     env=dict(os.environ, SYSTEMD_LOG_LEVEL='warning'))
    wait(lambda: subprocess.run([args.resolvectl, 'dns', 'lo'], capture_output=True).returncode == 0,
         'private systemd-resolved failed to start')


def exercise(args, root):
    scenario = args.scenario
    protected = not scenario.startswith('off-')
    hosted = 'hosted' in scenario
    dns = 'dns' in scenario
    run('ip', 'link', 'set', 'lo', 'up')
    run('ip', 'link', 'add', 'egress0', 'type', 'dummy')
    run('ip', 'addr', 'add', '192.0.2.1/24', 'dev', 'egress0')
    run('ip', 'link', 'set', 'egress0', 'up')
    run('ip', 'route', 'add', 'default', 'via', '192.0.2.254', 'dev', 'egress0')
    if dns:
        private_resolver(args)
        run('ip', 'tuntap', 'add', 'dev', 'audit0', 'mode', 'tun')
        run('ip', 'link', 'set', 'audit0', 'up')
        wait(lambda: subprocess.run(['resolvectl', 'dns', 'audit0'], capture_output=True).returncode == 0,
             'test link absent from resolved')
        run('resolvectl', 'dns', 'audit0', '10.0.0.53', '10.0.0.54')
        run('resolvectl', 'domain', 'audit0', 'corp.test', '~split.test')
        run('resolvectl', 'default-route', 'audit0', 'no')
        run('resolvectl', 'llmnr', 'audit0', 'no')
        run('ip', 'addr', 'add', '198.18.22.1/30', 'dev', 'audit0')
    before = network()
    keys = ('dns', 'domain', 'default-route', 'llmnr')
    def resolver():
        return {k: run('resolvectl', k, 'audit0').strip() for k in keys}
    old_dns = resolver() if dns else {}
    conf = root / 'proxy.toml'
    conf.write_text(f"""standalone=true
standalone_outbound="direct"
mixed_listen="127.0.0.1:37422"
shutdown_timeout_secs=1
[proxy_groups.direct]
members=["DIRECT"]
[tun]
name="audit0"
address="198.18.22.1"
prefix=30
interface="egress0"
auto_interface=false
auto_gateway=false
gateway="192.0.2.254"
routes=["198.51.100.43/32"]
bypass=["203.0.113.71/32"]
[experimental]
network_recovery={str(protected).lower()}
""")
    if dns:
        with conf.open('a') as f:
            f.write('[dns]\nenable=true\nhijack=true\nroute_system_dns=false\nlisten=["127.0.0.1:1053"]\n')
    control = root / 'core.sock'
    runtime = root / 'runtime'
    env = dict(os.environ, ASTRA_CONTROL_SOCKET=str(control), ASTRA_RUNTIME_DIR=str(runtime))
    if hosted:
        env['ASTRA_HOSTED_EXPERIMENTAL'] = '1'
    if 'partial' in scenario:
        # Suspend the caller after resolved changes its first property and
        # before it can acknowledge the write to the journal.
        shim = root / 'resolvectl'
        shim.write_text(f"""#!/usr/bin/python3
import pathlib, subprocess, sys, time
p = subprocess.run([{str(args.resolvectl)!r}, *sys.argv[1:]])
if p.returncode == 0 and sys.argv[1:3] == ['domain', 'audit0'] and '~.' in sys.argv:
    pathlib.Path({str(root / 'applied')!r}).touch()
    while not pathlib.Path({str(root / 'resume')!r}).exists():
        time.sleep(.02)
sys.exit(p.returncode)
""")
        shim.chmod(0o700)
        env['PATH'] = str(root) + os.pathsep + env['PATH']
    logs = root / 'core.log'
    def cli(*params):
        return run(args.binary, *params, env=env)
    def start(profile):
        with logs.open('a') as out:
            core = subprocess.Popen([args.binary, profile], env=env, cwd=root, stdout=out, stderr=out)
        def ready():
            if core.poll() is not None:
                raise RuntimeError(logs.read_text())
            return control.exists() and subprocess.run([args.binary, '--runtime', control, 'snapshot'],
                                                        capture_output=True, env=env).returncode == 0
        wait(ready, 'core not ready')
        return core
    def journals():
        return list((runtime / 'network-recovery').glob('*.json'))
    core = start(conf)
    try:
        assert not journals(), 'local listeners unexpectedly started network recovery'
        if 'partial' in scenario:
            enabling = subprocess.Popen([args.binary, '--runtime', control, 'tun', 'on'], env=env,
                                        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            wait(lambda: (root / 'applied').exists(), 'first DNS mutation did not run')
            assert resolver()['domain'] != old_dns['domain'] and resolver()['dns'] == old_dns['dns']
        else:
            cli('--runtime', control, 'tun', 'on')
        links, routes = network()
        assert 'audit0' in links and all(any(ip in route for route in routes)
                                          for ip in ('198.51.100.43', '203.0.113.71'))
        if dns and 'partial' not in scenario:
            assert resolver()['dns'].endswith('198.18.22.2'), logs.read_text()
        if 'compete' in scenario:
            # Simulate an abandoned journal in another runtime directory. Even
            # matching values/tokens do not authorize changing a live target.
            elsewhere = root / 'other'
            elsewhere.mkdir(mode=0o700)
            record = json.loads(journals()[0].read_text())
            record['owner'] = None
            record['notice'] = None
            record['resources'] = [r for r in record['resources'] if r['resource']['kind'] == 'DnsSetting']
            assert len(record['resources']) == 2
            copy = elsewhere / journals()[0].name
            copy.write_text(json.dumps(record))
            copy.chmod(0o600)
            live = resolver()
            replay = subprocess.run([args.binary, '--recover-network', copy], capture_output=True, env=env)
            assert replay.returncode and resolver() == live and copy.exists()
            assert all('another Astra instance' in r['error'] for r in json.loads(copy.read_text())['resources'])
        if 'reload' in scenario:
            backend = socket.socket()
            backend.bind(('127.0.0.1', 0))
            backend.listen()
            backend.settimeout(4)
            client = socket.create_connection(('127.0.0.1', 37422), timeout=4)
            client.sendall(bytes([5, 1, 0]))
            assert client.recv(2, socket.MSG_WAITALL) == bytes([5, 0])
            client.sendall(bytes([5, 1, 0, 1, 127, 0, 0, 1]) + backend.getsockname()[1].to_bytes(2, 'big'))
            peer, _ = backend.accept()
            peer.settimeout(4)
            assert client.recv(10, socket.MSG_WAITALL)[:2] == bytes([5, 0])
            cli('--reload', control)
            client.sendall(b'reload')
            assert peer.recv(6, socket.MSG_WAITALL) == b'reload'
        if 'conflict' in scenario:
            run('resolvectl', 'dns', 'audit0', '10.10.10.10')
        if 'double' in scenario:
            os.kill(core.pid, signal.SIGSTOP)
            children = {int(pid) for tasks in Path(f'/proc/{core.pid}/task').glob('*/children')
                        for pid in tasks.read_text().split()}
            guardians = [p for p in children if b'--network-guardian' in Path(f'/proc/{p}/cmdline').read_bytes().split(b'\0')]
            assert len(guardians) == 1
            os.kill(guardians[0], signal.SIGKILL)
        if 'kill' in scenario:
            core.kill()
        elif 'term' in scenario:
            core.terminate()
        else:
            cli('--stop', control)
        if 'reload' in scenario:
            peer.sendall(b'drain')
            assert client.recv(5, socket.MSG_WAITALL) == b'drain'
        code = core.wait(15)
        if 'reload' in scenario:
            assert client.recv(1) == b''
            client.close()
            peer.close()
            backend.close()
        if 'partial' in scenario:
            (root / 'resume').touch()
            enabling.wait(5)
        expected = -9 if 'kill' in scenario else int('conflict' in scenario)
        assert code == expected, (code, logs.read_text())
        if 'double' in scenario:
            assert journals() and network() != before
            clean = root / 'new.toml'
            clean.write_text(conf.read_text())
            core = start(clean)
            cli('--stop', control)
            assert core.wait(15) == 0
        if scenario == 'off-kill':
            links, routes = network()
            assert links == before[0] and len(routes - before[1]) == 1
            assert any('203.0.113.71' in r for r in routes - before[1]) and not journals()
        elif 'conflict' in scenario:
            assert network() == before and resolver()['dns'].endswith('10.10.10.10')
            assert all(resolver()[k] == old_dns[k] for k in keys if k != 'dns')
            if protected:
                wait(lambda: journals() and len(json.loads(journals()[0].read_text())['resources']) == 1,
                     'expected one pending DNS conflict')
                run('resolvectl', 'dns', 'audit0', '10.0.0.53', '10.0.0.54')
                cli('--recover-network', journals()[0])
                assert not journals()
        else:
            wait(lambda: network() == before and not journals(), 'network or journal differs after exit')
            if dns:
                assert resolver() == old_dns
        print(scenario, 'passed', flush=True)
    except BaseException:
        print(logs.read_text(), file=sys.stderr)
        raise
    finally:
        if core.poll() is None:
            core.kill()
            core.wait()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--resolvectl', type=Path)
    parser.add_argument('--resolved', type=Path)
    parser.add_argument('--scenario', help=argparse.SUPPRESS)
    args = parser.parse_args()
    args.binary = args.binary.resolve(strict=True)
    if bool(args.resolved) != bool(args.resolvectl):
        parser.error('supply both systemd binaries')
    if args.scenario:
        assert os.getpid() == 1 and network()[0] == {'lo'}, 'requires fresh PID and network namespaces'
        for path in ('/run', '/var/tmp'):
            run('mount', '-t', 'tmpfs', 'tmpfs', path)
        with tempfile.TemporaryDirectory(prefix='astra-exit-', dir='/run') as tmp:
            exercise(args, Path(tmp))
        return
    cases = ['off-stop', 'off-term', 'off-kill', 'on-stop', 'on-term', 'on-kill',
             'on-double-kill', 'off-reload-stop', 'on-hosted-reload-stop']
    if args.resolved:
        cases += ['on-dns-stop', 'on-dns-kill', 'on-dns-partial-kill', 'on-dns-compete-stop',
                  'on-dns-conflict', 'off-dns-conflict']
    for case in cases:
        subprocess.run(['unshare', '--mount', '--net', '--pid', '--fork', '--kill-child', '--mount-proc',
                        sys.executable, __file__, *sys.argv[1:], '--scenario', case], check=True, timeout=60)


if __name__ == '__main__':
    main()
