#!/usr/bin/env python3
"""Real TLS Controller + two Agents + independent Web lifecycle regression.

All listeners and traffic are loopback. No system proxy, TUN or elevation is used.
--artifact-dir boots the four extracted release roles, verifying their checksums.
Process logs, SQLite state, drafts and a JSON result remain in the printed work dir.
"""
import argparse
import base64
import contextlib
import hashlib
import json
import os
from pathlib import Path
import secrets
import subprocess
import tarfile
import tempfile
import threading
import time

from astra_tools.artifacts import ArtifactIndex, ROLES, role_binaries, sha256
from astra_tools.fixtures.protocols import tls_material
from astra_tools.management import ManagementHttp, eventually, expect_error
from astra_tools.net import Echo, EchoServer, exact, free_port, socks_connect
from astra_tools.package_checks import verify_manifest
from astra_tools.platform import executable
from astra_tools.processes import StopPolicy, process, stop_process, wait_port, wait_socket, child_processes, kill_owned_child
from astra_tools.workspace import ROOT, resolve_binary_dir


def quoted(value):
    return json.dumps(str(value).replace('\\', '/'))


def write_config(path, text):
    path.write_text(text, encoding='utf-8')
    return path


def storage(directory, bins):
    return ('[storage]\n' + '\n'.join(f'{key}={quoted(value)}' for key, value in (
        ('state_dir', directory/'state'), ('config_dir', directory/'config'), ('bin_dir', bins))) + '\n')


def binaries(args, work):
    if args.artifact_dir:
        index = ArtifactIndex.load(args.artifact_dir)
        archives = dict(index.archives(profile=args.profile))
        assert set(archives) == set(ROLES), 'Management package gate requires all four role archives'
        result = {}
        for role, archive in archives.items():
            destination = work/'packages'/role
            destination.mkdir(parents=True)
            with tarfile.open(archive, 'r:gz') as source:
                source.extractall(destination, filter='data')
            roots = [p for p in destination.iterdir() if p.is_dir()]
            assert len(roots) == 1 and len(list(destination.iterdir())) == 1
            manifest = verify_manifest(roots[0], role)
            expected = {executable(name) for name in role_binaries((role,), target=index.target)}
            actual = {entry['path'] for entry in manifest['files']}
            assert expected <= actual, f'{role} is missing a role executable'
            result[role] = roots[0]
        return result, archives['client']
    bins = resolve_binary_dir(args.bin_dir, profile=args.profile)
    return {role: bins for role in ROLES}, None


def run(args, work, report):
    bins, client_archive = binaries(args, work)
    upload_limit = 256 * 1024 * 1024
    if client_archive:
        archive_bytes = client_archive.stat().st_size
        # Debug symbols can make a genuine CI release exceed the production
        # default. Explicitly provision this test agent for its exact archive;
        # neither the production default nor the 256 KiB wire chunks change.
        if not 0 < archive_bytes <= 4 * 1024 * 1024 * 1024:
            raise ValueError('Client test archive must fit the configured 4 GiB upload ceiling; rebuild without debug symbols')
        upload_limit = max(upload_limit, archive_bytes)
        report['artifact'] = {
            'archive': client_archive.name, 'archive_bytes': archive_bytes,
            'upload_limit_bytes': upload_limit, 'chunk_bytes': 256 * 1024,
            'chunks': (archive_bytes + 256 * 1024 - 1) // (256 * 1024),
        }
        print(f'Artifact fixture: {archive_bytes} bytes, {report["artifact"]["chunks"]} chunks; agent-a upload limit {upload_limit} bytes', flush=True)
    manager = bins['controller']/executable('astra-manager')
    web = bins['web']/executable('astra-web')
    assert manager.is_file() and web.is_file(), 'Build astra-manager and astra-web first'
    has_client = all((bins['client']/executable(name)).is_file() for name in ('astra-rule', 'astra-proxy'))
    has_server = (bins['server']/executable('astra-server-core')).is_file()
    if args.require_cores or args.artifact_dir:
        assert has_client and has_server, 'This gate requires real client and server executables'
    report['cores'] = {'client': 'available' if has_client else 'skipped: binaries absent',
                       'server': 'available' if has_server else 'skipped: binary absent'}
    tls = work/'tls'
    tls.mkdir()
    cert, key = tls_material(tls)
    ca = tls/'ca.pem'
    ctl_dir = work/'controller'
    ctl_dir.mkdir()
    controller_port = free_port()
    controller_config = write_config(ctl_dir/'manager.toml', 'role="controller"\n' + storage(ctl_dir, bins['controller']) +
        f'[controller]\nlisten="127.0.0.1:{controller_port}"\ntls_cert={quoted(cert)}\ntls_key={quoted(key)}\n')
    agent_configs = {}
    for name, role in [('agent-a', 'client'), ('agent-b', 'server')]:
        directory = work/name
        directory.mkdir()
        credential = directory/'credential'
        subprocess.run([manager, 'provision-agent', '--config', controller_config, '--agent-id', name,
                        '--token-out', credential], check=True, capture_output=True, timeout=15)
        agent_configs[name] = write_config(directory/'manager.toml', 'role="agent"\n' + storage(directory, bins[role]) +
            f'[upstream]\nenabled=true\nagent_id="{name}"\nname="{name}"\ncontroller_url="wss://localhost:{controller_port}/agents/v2"\n'
            f'credential_file={quoted(credential)}\ntrust_root={quoted(ca)}\n' +
            (f'[limits]\nupload_bytes={upload_limit}\n' if name == 'agent-a' and client_archive else ''))
    password = secrets.token_urlsafe(24)
    web_configs, clients = {}, {}
    for name, owner in [('remote', ctl_dir), ('local', work/'agent-a')]:
        directory = work/f'web-{name}'
        directory.mkdir()
        port = free_port()
        origin = f'https://localhost:{port}'
        web_configs[name] = (write_config(directory/'web.toml',
            f'manager_socket={quoted(owner/"state/management.sock")}\nstate_dir={quoted(directory/"state")}\n'
            f'listen="127.0.0.1:{port}"\npublic_origin="{origin}"\n[tls]\n'
            f'certificate={quoted(cert)}\nprivate_key={quoted(key)}\n'), port)
        subprocess.run([web, '--config', web_configs[name][0], '--set-password', '--password-stdin'],
                       input=password+'\n', text=True, check=True, capture_output=True, timeout=30)
        clients[name] = ManagementHttp(origin, ca)
        if name == 'local':
            config = agent_configs['agent-a']
            config.write_text(config.read_text(encoding='utf-8') +
                f'\n[web]\nenabled=true\nlisten="127.0.0.1:{port}"\npublic_origin="{origin}"\n'
                f'state_dir={quoted(directory/"state")}\nbinary={quoted(web)}\n[web.tls]\n'
                f'certificate={quoted(cert)}\nprivate_key={quoted(key)}\n', encoding='utf-8')

    def passed(name):
        report['checks'].append(name)
        print(f'PASS {name}', flush=True)

    with contextlib.ExitStack() as stack:
        controller, controller_log = stack.enter_context(process([manager, '--config', controller_config], work, 'controller', stop=StopPolicy(mode='terminate')))
        wait_port(controller_port, controller, controller_log)
        children = {}
        for name, config in agent_configs.items():
            agent_binary = bins['client' if name == 'agent-a' else 'server']/executable('astra-manager')
            assert agent_binary.is_file(), f'{name} role has no manager executable'
            child, log = stack.enter_context(process([agent_binary, '--config', config], work, name,
                stop=StopPolicy(mode='control', timeout=20, command=(str(agent_binary), '--config', str(config), '--shutdown'))))
            children[name] = (child, log)
            wait_socket(work/name/'state/management.sock', child, log, timeout=20)
        for name, (config, port) in web_configs.items():
            if name == 'local':
                wait_port(port, children['agent-a'][0], work/'web-local/state/gateway.log')
                continue
            child, log = stack.enter_context(process([web, '--config', config], work, 'web-'+name, stop=StopPolicy(mode='terminate')))
            wait_port(port, child, log)
        remote, local = clients['remote'], clients['local']
        assert remote.http('GET', '/api/v2/nodes', token=False)[0] == 401
        assert remote.http('GET', '/api/v2/events?agent_id=agent-a', token=False)[0] == 401
        assert remote.http('POST', '/api/v2/login', {'password': 'incorrect'}, token=False)[0] == 401
        remote.login(password)
        local.login(password)
        assert remote.http('GET', '/api/v2/nodes', headers={'Origin': 'https://other.invalid'})[0] == 403
        assert remote.http('GET', '/api/v2/nodes', headers={'Host': 'other.invalid'})[0] == 403
        agent_credential = (work/'agent-a/credential').read_text()
        assert remote.http('GET', '/api/v2/nodes', headers={'Authorization': 'Bearer '+agent_credential})[0] == 401
        passed('HTTPS authentication, session isolation, origin and host checks')
        agents = eventually(lambda: (rows if len(rows := [a for a in remote.query('agents') if a['online']]) == 2 else None), description='two authenticated WSS agents')
        assert {agent['id'] for agent in agents} == {'agent-a', 'agent-b'}
        assert all(agent['boot_id'] and agent['last_seen_ms'].isdigit() for agent in agents)
        assert remote.query('instances', 'agent-a') == []
        assert remote.query('instances', 'agent-b') == []
        assert all(not (work/name/'state/manager.sock').exists() for name in agent_configs), 'Legacy manager actor IPC must not exist in production'
        expect_error('offline', lambda: remote.query('instances', 'missing-agent'))
        passed('real WSS registration, node selection and isolated instance lists')

        # An unprovisioned identity with a valid TLS trust root cannot register.
        invalid_dir = work/'invalid-agent'
        invalid_dir.mkdir()
        (invalid_dir/'credential').write_text(remote.token, encoding='utf-8')
        invalid_config = write_config(invalid_dir/'manager.toml', 'role="agent"\n' + storage(invalid_dir, bins['client']) +
            f'[upstream]\nenabled=true\nagent_id="unprovisioned"\nname="invalid"\ncontroller_url="wss://localhost:{controller_port}/agents/v2"\n'
            f'credential_file={quoted(invalid_dir/"credential")}\ntrust_root={quoted(ca)}\n')
        invalid, invalid_log = stack.enter_context(process([manager, '--config', invalid_config], work, 'invalid-agent',
            stop=StopPolicy(mode='control', command=(str(manager), '--config', str(invalid_config), '--shutdown'))))
        wait_socket(invalid_dir/'state/management.sock', invalid, invalid_log, timeout=20)
        time.sleep(1)
        assert {agent['id'] for agent in remote.query('agents')} == {'agent-a', 'agent-b'}
        passed('browser session cannot authenticate an agent')

        active = []
        if has_client:
            rule_socket = work/'agent-a/rule.sock'
            rule_text = f'socket={quoted(rule_socket)}\ndefault_action="direct"\n'
            proxy_port = free_port()
            proxy_text = f'rule_socket={quoted(rule_socket)}\nsocks5_listen="127.0.0.1:{proxy_port}"\nshutdown_timeout_secs=0\n'
            for kind, text in [('rule', rule_text), ('proxy', proxy_text)]:
                operation = f'create-{kind}'
                original = remote.execute('agent-a', 'create', operation=operation, name=kind, kind=kind)
                assert remote.execute('agent-a', 'create', operation=operation, name=kind, kind=kind) == original
                expect_error('conflict', lambda: remote.execute('agent-a', 'create', operation=operation, name='changed', kind=kind))
                assert remote.query('task', 'agent-a', operation_id=operation)['state'] == 'complete'
                edit_source(remote, 'agent-a', kind, text)
                remote.execute('agent-a', 'start', instance=kind)
                active.append(('agent-a', kind))
            assert remote.query('instances', 'agent-b') == []
            passed('client create/start, per-node isolation and operation-id deduplication')
            draft_conflict(remote, local, 'agent-a', 'proxy')
            passed('two frontends reject stale draft and source revisions without losing edits')
        if has_server:
            remote.execute('agent-b', 'create', name='server', kind='server')
            server_port = free_port()
            server_text = f'allow_private_targets=true\nshutdown_timeout_secs=0\n[[listeners]]\ntag="test"\nprotocol="socks5"\nlisten="127.0.0.1:{server_port}"\n[[listeners.users]]\nname="test"\npassword="local-test-password"\n'
            edit_source(remote, 'agent-b', 'server', server_text)
            remote.execute('agent-b', 'start', instance='server')
            active.append(('agent-b', 'server'))
            status = remote.query('runtime', 'agent-b', instance='server')
            assert status['kind'] == 'server' and status['snapshot']['pid'] > 0
            passed('server create/start and typed runtime status')

        if args.browser:
            assert has_client and has_server, '--browser requires running client and server fixtures'
            browser_checks(remote.origin, password, work, report)
            passed('real browser login, node selection and client/server management')

        if client_archive:
            artifact_started = time.monotonic()
            try:
                artifact_checks(remote, client_archive, bins['client'], report['artifact'])
            finally:
                report['artifact']['total_seconds'] = round(time.monotonic() - artifact_started, 3)
            passed('resumable artifact upload verifies chunks and package digest, then stages exact release without activation')

        before = {(agent, name): instance(remote, agent, name)['pid'] for agent, name in active}
        assert all(before.values())
        if active:
            batch = remote.events(active[0][0])
            assert batch['events'] and batch['cursor'].isdigit()
            resumed = remote.events(active[0][0], batch['cursor'])
            assert all(int(event['sequence']) > int(batch['cursor']) for event in resumed['events'])
            passed('real event delivery and SSE cursor resume')

        with EchoServer(('127.0.0.1', 0), Echo) as echo:
            threading.Thread(target=echo.serve_forever, daemon=True).start()
            try:
                ports = ([(proxy_port, False)] if has_client else []) + ([(server_port, True)] if has_server else [])
                for port, authenticated in ports:
                    traffic(port, echo.server_address[1], authenticated)
                supervisor_checks(children, local, password, web_configs['local'][1], work, ports, echo.server_address[1])
                passed('manager supervises optional Web restart while cores continue; disabled Web creates no child')
                credentials = credential_checks(manager, controller_config, controller, remote, local,
                    work/'agent-a/credential', children['agent-a'][0], ports, echo.server_address[1], passed)
                # Kill only the remote controller. Neither local manager owns this process.
                stop_process(controller, StopPolicy(mode='terminate'))
                assert controller.poll() is not None
                assert remote.http('GET', '/api/v2/nodes')[0] == 502
                assert local.query('agents')[0]['id'] == 'agent-a'
                for agent, name in active:
                    if agent == 'agent-a':
                        assert instance(local, agent, name)['pid'] == before[(agent, name)]
                for port, authenticated in ports:
                    traffic(port, echo.server_address[1], authenticated)
                if has_client:
                    local.execute('agent-a', 'stop', instance='proxy')
                    eventually(lambda: instance(local, 'agent-a', 'proxy')['pid'] is None, description='local proxy stop')
                    local.execute('agent-a', 'start', instance='proxy')
                    traffic(proxy_port, echo.server_address[1])
                passed('controller loss preserves core traffic and independent local management')
            finally:
                echo.shutdown()

        # A fresh Controller recovers node routing without replaying completed commands.
        # Abrupt termination skips the endpoint guard. Only this known-dead test
        # process's private endpoint is removed; no unknown service is replaced.
        assert controller.poll() is not None
        (ctl_dir/'state/management.sock').unlink(missing_ok=True)
        controller2, log2 = stack.enter_context(process([manager, '--config', controller_config], work, 'controller-restarted', stop=StopPolicy(mode='terminate')))
        wait_port(controller_port, controller2, log2)
        eventually(lambda: len([a for a in remote.query('agents') if a['online']]) == 2, description='agents reconnect after controller restart')
        if has_client:
            assert remote.query('task', 'agent-a', operation_id='create-proxy')['state'] == 'complete'
            original = remote.execute('agent-a', 'create', operation='create-proxy', name='proxy', kind='proxy')
            assert original['type'] == 'done'
            assert sum(row['name'] == 'proxy' for row in remote.query('instances', 'agent-a')) == 1
        for agent, name in reversed(active):
            remote.execute(agent, 'stop', instance=name)
            eventually(lambda: instance(remote, agent, name)['pid'] is None, description=f'{name} stop')
            if name == 'server':
                eventually(lambda: not (work/'agent-b/state/server-server.sock').exists(), description='server endpoint cleanup')
            remote.execute(agent, 'remove', instance=name)
        passed('reconnect, persistent operation results, stop and remove')
        if has_client:
            old_boot = next(a['boot_id'] for a in remote.query('agents') if a['id'] == 'agent-a')
            agent_binary = bins['client']/executable('astra-manager')
            config = agent_configs['agent-a']
            stop = StopPolicy(mode='control', command=(str(agent_binary), '--config', str(config), '--shutdown'))
            stop_process(children['agent-a'][0], stop)
            restarted, restarted_log = stack.enter_context(process([agent_binary, '--config', config], work, 'agent-a-restarted', stop=stop))
            wait_socket(work/'agent-a/state/management.sock', restarted, restarted_log, timeout=20)
            eventually(lambda: any(a['id'] == 'agent-a' and a['online'] and a['boot_id'] != old_boot for a in remote.query('agents')), description='agent process restart')
            assert remote.query('task', 'agent-a', operation_id='create-proxy')['state'] == 'complete'
            remote.execute('agent-a', 'create', operation='create-proxy', name='proxy', kind='proxy')
            assert remote.query('instances', 'agent-a') == [], 'Persisted request was replayed after agent restart'
            passed('SQLite task deduplication survives an actual agent process restart')
        # Explicitly shut down one agent and ensure commands are not queued while offline.
        child, _ = children['agent-b']
        stop_process(child, StopPolicy(mode='control', command=(str(bins['server']/executable('astra-manager')), '--config', str(agent_configs['agent-b']), '--shutdown')))
        eventually(lambda: any(a['id'] == 'agent-b' and not a['online'] for a in remote.query('agents')), description='agent offline transition')
        expect_error('offline', lambda: remote.execute('agent-b', 'create', name='must-not-queue', kind='server'))
        passed('offline mutation is rejected without queueing')
        old_token = remote.token
        assert remote.http('POST', '/api/v2/logout')[0] == 204
        assert remote.http('GET', '/api/v2/nodes')[0] == 401
        for path in work.glob('web-*/state/audit.jsonl'):
            audit = path.read_text(encoding='utf-8')
            assert all(secret not in audit for secret in [password, old_token, agent_credential, *credentials])
        passed('logout revokes sessions and audit logs exclude credentials')


def instance(client, agent, name):
    return next(row for row in client.query('instances', agent) if row['name'] == name)


def artifact_checks(client, archive, directory, stats):
    identity = 'client-package'
    def command(kind, **data):
        return client.request({'type': 'artifacts', 'data': {'type': kind, 'data': data}}, 'agent-a')
    size = archive.stat().st_size
    stats['phase'] = 'hash'
    started = time.monotonic()
    digest = sha256(archive)
    stats['hash_seconds'] = round(time.monotonic() - started, 3)
    stats['phase'] = 'upload'
    started = last_progress = time.monotonic()
    state = command('begin', id=identity, name=archive.name, size=str(size), sha256=digest)['data']
    assert state['received'] == '0' and not state['complete']
    with archive.open('rb') as source:
        offset = 0
        while chunk := source.read(256 * 1024):
            data = dict(id=identity, offset=str(offset), base64=base64.b64encode(chunk).decode(), sha256=hashlib.sha256(chunk).hexdigest())
            if offset == 0:
                # A corrupt chunk cannot advance the persisted cursor.
                expect_error('internal', lambda: command('write', **dict(data, sha256='0' * 64)))
            state = command('write', **data)['data']
            offset += len(chunk)
            assert state['received'] == str(offset)
            stats['received_bytes'] = offset
            stats['upload_seconds'] = round(time.monotonic() - started, 3)
            if time.monotonic() - last_progress >= 10 or offset == size:
                print(f'Artifact upload: {offset}/{size} bytes in {stats["upload_seconds"]:.1f}s', flush=True)
                last_progress = time.monotonic()
            if data['offset'] == '0':
                assert command('write', **data)['data']['received'] == str(offset), 'Retried chunk duplicated bytes'
    state = command('finish', id=identity)['data']
    assert state['complete'] and state['sha256'] == digest and state['received'] == str(size)
    stats['upload_seconds'] = round(time.monotonic() - started, 3)
    stats['phase'] = 'stage'
    started = time.monotonic()
    expected = json.loads((directory/'manifest.json').read_text(encoding='utf-8'))['release_id']
    before = client.query('update_status', 'agent-a')
    response = client.execute('agent-a', 'update_artifact', operation='stage-client-artifact', artifact_id=identity)
    assert response['type'] == 'update_status'
    status = response['data']
    assert status['staged_release_id'] == expected, f"Staging did not select the verified release: {status.get('operation')}"
    assert status['runtime_directory'] == before['runtime_directory'], 'Upload/stage must not activate the package'
    assert status['next_launch'] == before['next_launch'], 'Upload/stage must not change the next launch'
    assert client.execute('agent-a', 'update_artifact', operation='stage-client-artifact', artifact_id=identity) == response
    stats['staging_seconds'] = round(time.monotonic() - started, 3)
    stats['phase'] = 'complete'


def supervisor_checks(children, local, password, port, work, ports, destination):
    owner = children['agent-a'][0]
    image = executable('astra-web')
    def gateway():
        return next((row['pid'] for row in child_processes(owner.pid) if row['name'] == image), None)
    before = {row['name']: row['pid'] for row in local.query('instances', 'agent-a')}
    assert not any(row['name'] == image for row in child_processes(children['agent-b'][0].pid))
    original_pid = eventually(gateway, description='supervised Web child')
    kill_owned_child(owner, original_pid, image)
    eventually(lambda: (pid if (pid := gateway()) and pid != original_pid else None), timeout=20, description='replacement Web process')
    wait_port(port, owner, work/'web-local/state/gateway.log')
    assert local.http('GET', '/api/v2/nodes')[0] == 401, 'Restarted gateway retained an old in-memory session'
    local.login(password)
    assert {row['name']: row['pid'] for row in local.query('instances', 'agent-a')} == before
    for core_port, authenticated in ports:
        traffic(core_port, destination, authenticated)
    assert not any(row['name'] == image for row in child_processes(children['agent-b'][0].pid))


def browser_checks(origin, password, work, report):
    directory = work/'browser'
    directory.mkdir()
    env = dict(os.environ, ASTRA_E2E_URL=origin, ASTRA_E2E_PASSWORD=password,
               ASTRA_E2E_REPORT_DIR=str(directory))
    # Credentials are inherited only by the child, never rendered as arguments.
    npx = 'npx.cmd' if os.name == 'nt' else 'npx'
    try:
        result = subprocess.run([npx, '--no-install', 'playwright', 'test'], cwd=ROOT/'apps/web',
                                env=env, capture_output=True, text=True, encoding='utf-8', errors='replace', timeout=300)
        code, stdout, stderr = result.returncode, result.stdout, result.stderr
    except subprocess.TimeoutExpired as error:
        code = 124
        stdout = error.stdout or b''
        stderr = error.stderr or b''
    def decoded(value):
        return value.decode('utf-8', errors='replace') if isinstance(value, bytes) else value
    output = (decoded(stdout) + '\n' + decoded(stderr)).replace(password, '[REDACTED]')
    (directory/'runner.log').write_text(output, encoding='utf-8')
    report['browser'] = {'exit_code': code, 'report_directory': str(directory)}
    if code:
        raise RuntimeError(f'Browser verification failed; report: {directory}\n{output[-5000:]}')


def credential_checks(manager, config, controller, remote, local, credential, agent_process, ports, destination, passed):
    def online():
        return any(a['id'] == 'agent-a' and a['online'] for a in remote.query('agents'))

    def unchanged():
        assert controller.poll() is None and agent_process.poll() is None
        identity = local.query('agents')[0]
        assert identity['id'] == 'agent-a' and identity['boot_id'] == original_boot
        assert remote.query('hello')['epoch'] == original_epoch, 'Controller restarted during live credential change'
        for port, authenticated in ports:
            traffic(port, destination, authenticated)

    original_epoch = remote.query('hello')['epoch']
    original_boot = local.query('agents')[0]['boot_id']
    secrets_used = []
    for command, label in [('rotate-agent', 'rotation'), ('revoke-agent', 'revocation')]:
        next_credential = credential.with_name('credential-'+label)
        args = [manager, command, '--config', config, '--agent-id', 'agent-a']
        if command == 'rotate-agent':
            args += ['--token-out', next_credential]
        subprocess.run(args, check=True, capture_output=True, timeout=15)
        eventually(lambda: not online(), timeout=15, description=f'live credential {label}')
        unchanged()
        expect_error('offline', lambda: remote.execute('agent-a', 'create', name='must-not-queue', kind='proxy'))
        # The old token is still installed: verify retry handshakes cannot restore access.
        deadline = time.monotonic() + 3
        while time.monotonic() < deadline:
            assert not online(), f'Old credential reconnected after {label}'
            time.sleep(.2)
        if command == 'revoke-agent':
            subprocess.run([manager, 'provision-agent', '--config', config, '--agent-id', 'agent-a',
                            '--token-out', next_credential], check=True, capture_output=True, timeout=15)
        secrets_used.append(next_credential.read_text(encoding='utf-8').strip())
        # Provisioned files already carry private permissions. Rename installs the
        # new credential atomically; the reconnecting Agent must read it afresh.
        next_credential.replace(credential)
        eventually(online, timeout=30, description=f'agent reconnect with replacement credential after {label}')
        unchanged()
        passed(f'live credential {label} disconnects old sessions and preserves core traffic')
    return secrets_used


def edit_source(client, agent, name, text):
    draft = client.draft(agent, 'open', instance=name, draft_id=f'configure-{name}')['data']
    changed = client.draft(agent, 'edit', draft_id=draft['id'], expected_revision=draft['revision'], text=text)['data']
    result = client.draft(agent, 'save', draft_id=changed['id'], expected_revision=changed['revision'],
                          expected_source_revision=changed['source_revision'], operation_id=f'configure-save-{name}')
    saved = result['data']['last_save']
    assert saved['state'] == 'next_start' and saved['disk_saved'] is True and saved['runtime_applied'] is False
    return result


def draft_conflict(remote, local, agent, name):
    one = remote.draft(agent, 'open', instance=name, draft_id='browser-draft')['data']
    two = local.draft(agent, 'open', instance=name, draft_id='local-draft')['data']
    edited = remote.draft(agent, 'edit', draft_id=one['id'], expected_revision=one['revision'], text=one['text']+'\n# browser edit\n')['data']
    expect_error('conflict', lambda: local.draft(agent, 'edit', draft_id=one['id'], expected_revision=one['revision'], text='stale'))
    saved = remote.draft(agent, 'save', draft_id=edited['id'], expected_revision=edited['revision'], expected_source_revision=edited['source_revision'], operation_id='save-browser')['data']['last_save']
    assert saved['state'] == 'applied' and saved['disk_saved'] is True and saved['runtime_applied'] is True
    edited_two = local.draft(agent, 'edit', draft_id=two['id'], expected_revision=two['revision'], text=two['text']+'\n# local edit\n')['data']
    expect_error('conflict', lambda: local.draft(agent, 'save', draft_id=edited_two['id'], expected_revision=edited_two['revision'], expected_source_revision=edited_two['source_revision'], operation_id='save-local-conflict'))
    assert local.draft(agent, 'read', draft_id=two['id'])['data']['text'] == edited_two['text']
    assert remote.query('document', agent, instance=name)['text'] == edited['text']


def traffic(port, destination, authenticated=False):
    credentials = {'username': 'test', 'password': 'local-test-password'} if authenticated else {}
    connection, status = socks_connect(port, '127.0.0.1', destination, **credentials)
    with contextlib.closing(connection) as stream:
        assert status == 0, f'SOCKS connection rejected: {status}'
        payload = b'astra-management-controller-independent-traffic'
        stream.sendall(payload)
        assert exact(stream, len(payload)) == payload


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('--bin-dir', type=Path)
    parser.add_argument('--artifact-dir', type=Path)
    parser.add_argument('--profile', choices=('debug', 'release'), default='release')
    parser.add_argument('--require-cores', action='store_true', help='fail unless client and server cores both exist')
    parser.add_argument('--browser', action='store_true', help='run the real Playwright UI tests against this isolated HTTPS deployment')
    parser.add_argument('--work-dir', type=Path, help='new directory for logs and isolated deployment state')
    parser.add_argument('--report', type=Path)
    args = parser.parse_args()
    if args.bin_dir and args.artifact_dir:
        parser.error('choose --bin-dir or --artifact-dir')
    # macOS Unix-domain sockets have a short path limit; /var/folders is too long.
    work = args.work_dir.resolve() if args.work_dir else Path(tempfile.mkdtemp(prefix='astra-management-', dir=None if os.name == 'nt' else '/tmp'))
    if args.work_dir:
        work.mkdir(parents=True, exist_ok=False)
    print(f'Isolated management test files: {work}', flush=True)
    report = {'passed': False, 'work_dir': str(work), 'checks': []}
    if args.artifact_dir:
        index = args.artifact_dir.resolve()/'artifacts.json'
        report['artifact_index'] = {'path': str(index), 'sha256': sha256(index)}
    started = time.monotonic()
    try:
        run(args, work, report)
        report['passed'] = True
    finally:
        report['duration_seconds'] = round(time.monotonic()-started, 3)
        data = json.dumps(report, indent=2)+'\n'
        (work/'report.json').write_text(data, encoding='utf-8')
        if args.report:
            args.report.parent.mkdir(parents=True, exist_ok=True)
            args.report.write_text(data, encoding='utf-8')


if __name__ == '__main__':
    main()
