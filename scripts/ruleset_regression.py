#!/usr/bin/env python3
"""Mihomo -> native rule providers, MMDB, updates and failed-update retention."""
import argparse
import contextlib
import http.server
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
import time
from platform_support import executable, process_flags
from protocol_matrix import Echo, EchoServer, exact, free_port, wait_port
from smoke_test import socks_connect
from rule_test_data import mmdb, geosite, geoip


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--bin-dir', type=Path, required=True)
    binaries = parser.parse_args().bin_dir.resolve()
    state = {'payload': b'DST-PORT,1\n', 'requests': 0}

    class Provider(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            state['requests'] += 1
            body = state['geosite'] if self.path == '/geosite' else state['geoip'] if self.path == '/geoip' else state['payload']
            self.send_response(200)
            self.send_header('Content-Length', str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *args):
            pass

    with tempfile.TemporaryDirectory(prefix='astra-rule-sources-') as temporary, contextlib.ExitStack() as stack:
        root = Path(temporary)
        source = root / 'source'
        source.mkdir()
        output = root / 'converted'
        echo = stack.enter_context(EchoServer(('127.0.0.1', 0), Echo))
        provider_server = stack.enter_context(http.server.ThreadingHTTPServer(('127.0.0.1', 0), Provider))
        for server in [echo, provider_server]:
            threading.Thread(target=server.serve_forever, daemon=True).start()
            stack.callback(server.shutdown)
        port = echo.server_address[1]
        socks = free_port()
        route = root / 'route.sock'
        state['payload'] = f'DST-PORT,{port}\n'.encode()
        local = source / 'local.txt'
        local.write_text('127.0.0.1/32\n', encoding='utf-8')
        database = source / 'Country.mmdb'
        mmdb(database)
        yaml = source / 'mihomo.yaml'
        yaml.write_text(f'''socks-port: {socks}
rule-providers:
  domains:
    type: inline
    behavior: domain
    payload: ['+.blocked.test']
  local:
    type: file
    behavior: ipcidr
    format: text
    path: local.txt
  remote:
    type: http
    behavior: classical
    format: text
    url: http://127.0.0.1:{provider_server.server_address[1]}/rules
    interval: 1
rules:
  - RULE-SET,domains,REJECT
  - RULE-SET,remote,REJECT
  - RULE-SET,local,DIRECT,no-resolve
  - GEOIP,CN,REJECT,no-resolve
  - MATCH,REJECT
''', encoding='utf-8')
        subprocess.run([str(binaries / executable('astra-convert')), str(yaml), '--strict', '--rule-socket', str(route), '-o', str(output)], check=True, capture_output=True, timeout=10)
        assert state['requests'] == 0, 'converter must remain offline'
        rules = output / 'rule.toml'
        rules.write_text('watch_config=true\n' + rules.read_text(encoding='utf-8'), encoding='utf-8')
        proxy = output / 'proxy.toml'
        proxy.write_text('shutdown_timeout_secs=1\n' + proxy.read_text(encoding='utf-8'), encoding='utf-8')
        children = []
        for name, config in [('astra-rule', rules), ('astra-proxy', proxy)]:
            control = root / f'{name}.ctl'
            log = root / f'{name}.log'
            output_log = stack.enter_context(log.open('w', encoding='utf-8'))
            binary = str(binaries / executable(name))
            child = subprocess.Popen([binary, str(config)], stdout=output_log, stderr=subprocess.STDOUT, env=dict(os.environ, ASTRA_CONTROL_SOCKET=str(control)), **process_flags())
            children.append((child, control, log))

            def cleanup(child=child, binary=binary, control=control):
                if child.poll() is None:
                    result = subprocess.run([binary, '--stop', str(control)], capture_output=True, timeout=10)
                    if result.returncode:
                        child.kill()
                child.wait(timeout=15)

            stack.callback(cleanup)
            if name == 'astra-rule':
                deadline = time.monotonic() + 10
                while not route.exists():
                    assert child.poll() is None and time.monotonic() < deadline, log.read_text(encoding='utf-8')
                    time.sleep(.02)
        wait_port(socks, children[1][0], children[1][2])

        def status(host='127.0.0.1'):
            client, result = socks_connect(socks, host, port)
            with client:
                if result == 0:
                    client.sendall(b'echo')
                    assert exact(client, 4) == b'echo'
            return result

        def until(expected, host='127.0.0.1'):
            deadline = time.monotonic() + 8
            while True:
                result = status(host)
                if result == expected:
                    return
                assert time.monotonic() < deadline, (expected, result, children[0][2].read_text(encoding='utf-8'))
                time.sleep(.1)

        assert status('a.blocked.test') == 2
        assert status() == 2, 'remote provider must block before local allow'
        state['payload'] = b'DOMAIN,unused.test\n'
        until(0)
        cache = next((output / 'rule-cache').iterdir())
        cached = cache.read_bytes()
        requests = state['requests']
        state['payload'] = b'SRC-PORT,80\n'
        deadline = time.monotonic() + 5
        while state['requests'] == requests:
            assert time.monotonic() < deadline
            time.sleep(.1)
        time.sleep(.1)
        assert status() == 0 and cache.read_bytes() == cached, 'invalid remote update replaced valid content'
        requests = state['requests']
        state['payload'] = b'DOMAIN-REGEX,[\n'
        deadline = time.monotonic() + 5
        while state['requests'] == requests:
            assert time.monotonic() < deadline
            time.sleep(.1)
        time.sleep(.1)
        assert status() == 0 and cache.read_bytes() == cached, 'invalid regular expression replaced valid provider cache'
        local.write_text('192.0.2.0/24\n', encoding='utf-8')
        until(2)
        local.write_text('invalid-cidr\n', encoding='utf-8')
        time.sleep(1.3)
        assert status() == 2, 'failed local update changed active rules'
        local.write_text('127.0.0.1/32\n', encoding='utf-8')
        until(0)
        alternate = root / 'geoip.toml'
        alternate.write_text(f'socket={json.dumps(str(route))}\ndefault_action="direct"\ngeoip_database={json.dumps(database.as_posix())}\n[[rules]]\nkind="geoip"\nvalue="CN"\naction="block"\n', encoding='utf-8')
        subprocess.run([str(binaries / executable('astra-rule')), '--switch', str(children[0][1]), str(alternate)], check=True, capture_output=True, timeout=10)
        until(2)
        database.write_bytes(b'invalid-mmdb')
        result = subprocess.run([str(binaries / executable('astra-rule')), '--reload', str(children[0][1])], capture_output=True, timeout=10)
        assert result.returncode != 0 and status() == 2, 'invalid MMDB reload changed active rules'
        mmdb(database)
        ip_source = source / 'geoip.dat'
        geoip(ip_source)
        state['geoip'] = ip_source.read_bytes()
        ip_yaml = source / 'ip.yaml'
        ip_yaml.write_text(f"geodata-mode: true\ngeox-url: {{geoip: 'http://127.0.0.1:{provider_server.server_address[1]}/geoip'}}\nrule-providers: {{ips: {{type: inline, behavior: classical, payload: ['GEOIP,telegram,no-resolve']}}}}\nrules: ['RULE-SET,ips,REJECT,no-resolve','MATCH,DIRECT']\n", encoding='utf-8')
        ip_output = root / 'ip-converted'
        before = state['requests']
        subprocess.run([str(binaries / executable('astra-convert')), str(ip_yaml), '--strict', '--rule-socket', str(route), '-o', str(ip_output)], check=True, capture_output=True, timeout=10)
        assert state['requests'] == before, 'GeoIP conversion must remain offline'
        ip_config = ip_output / 'rule.toml'
        ip_config.write_text('geoip_interval=1\n' + ip_config.read_text(encoding='utf-8'), encoding='utf-8')
        result = subprocess.run([str(binaries / executable('astra-rule')), '--switch', str(children[0][1]), str(ip_config)], capture_output=True, timeout=10)
        assert result.returncode == 0, result.stderr.decode(errors='replace')
        until(2)
        cached_ip = (ip_output / 'geoip/geoip.dat').read_bytes()
        geoip(ip_source, category='missing')
        state['geoip'] = ip_source.read_bytes()
        time.sleep(1.3)
        assert status() == 2 and (ip_output / 'geoip/geoip.dat').read_bytes() == cached_ip, 'missing GeoIP category replaced cache'
        state['geoip'] = b'invalid-geoip-dat'
        time.sleep(1.3)
        assert status() == 2 and (ip_output / 'geoip/geoip.dat').read_bytes() == cached_ip, 'invalid GeoIP update replaced cache'
        geoip(ip_source, networks=('192.0.2.0/24',))
        state['geoip'] = ip_source.read_bytes()
        until(0)
        # A failed explicit local switch keeps the currently applied generation.
        broken_ip = root / 'missing-category.toml'
        broken_ip.write_text(f'socket={json.dumps(str(route))}\ndefault_action="direct"\ngeoip_database={json.dumps(ip_source.as_posix())}\n[[rules]]\nkind="geoip"\nvalue="missing"\naction="block"\n', encoding='utf-8')
        result = subprocess.run([str(binaries / executable('astra-rule')), '--switch', str(children[0][1]), str(broken_ip)], capture_output=True, timeout=10)
        assert result.returncode != 0 and status() == 0
        site_source = source / 'geosite.dat'
        geosite(site_source, include_localhost=True)
        state['geosite'] = site_source.read_bytes()
        site_config = root / 'geosite.toml'
        def select_site(selector):
            site_config.write_text(f'socket={json.dumps(str(route))}\ndefault_action="direct"\ngeosite_database="site-cache.dat"\ngeosite_url="http://127.0.0.1:{provider_server.server_address[1]}/geosite"\ngeosite_interval=1\n[[rules]]\nkind="geosite"\nvalue="{selector}"\naction="block"\n', encoding='utf-8')
            result = subprocess.run([str(binaries / executable('astra-rule')), '--switch', str(children[0][1]), str(site_config)], capture_output=True, timeout=10)
            assert result.returncode == 0, result.stderr.decode(errors='replace')
        select_site('TEST')
        assert status('localhost') == 2
        for host in ['exact.test', 'suffix.test', 'rx42.test', 'a.keyword.test']:
            assert status(host) == 2
        select_site('test@cn')
        until(0, 'localhost')
        assert status('suffix.test') == 2
        select_site('test@!cn')
        until(2, 'localhost')
        cached_site = (root / 'site-cache.dat').read_bytes()
        state['geosite'] = cached_site.replace(b'TEST', b'NONE')
        time.sleep(1.3)
        assert status('localhost') == 2 and (root / 'site-cache.dat').read_bytes() == cached_site
        state['geosite'] = b'invalid-database'
        time.sleep(1.3)
        assert status('localhost') == 2 and (root / 'site-cache.dat').read_bytes() == cached_site
        # Check the converter's local-database override and classical GEOSITE provider.
        site_yaml = source / 'site.yaml'
        site_yaml.write_text("rule-providers: {sites: {type: inline, behavior: classical, payload: ['GEOSITE,test@!cn']}}\nrules: ['RULE-SET,sites,REJECT','MATCH,DIRECT']\n", encoding='utf-8')
        site_output = root / 'site-converted'
        subprocess.run([str(binaries / executable('astra-convert')), str(site_yaml), '--strict', '--geosite-db', str(site_source), '--rule-socket', str(route), '-o', str(site_output)], check=True, capture_output=True, timeout=10)
        result = subprocess.run([str(binaries / executable('astra-rule')), '--switch', str(children[0][1]), str(site_output / 'rule.toml')], capture_output=True, timeout=10)
        assert result.returncode == 0, result.stderr.decode(errors='replace')
        assert status('localhost') == 2
        provider_server.shutdown()
        provider_server.server_close()
        result = subprocess.run([str(binaries / executable('astra-rule')), '--check', str(rules)], capture_output=True, timeout=20)
        assert result.returncode == 0, result.stderr.decode(errors='replace')
        missing = output / 'missing-cache.toml'
        missing.write_text(rules.read_text(encoding='utf-8').replace(cache.relative_to(output).as_posix(), 'rule-cache/missing.text'), encoding='utf-8')
        result = subprocess.run([str(binaries / executable('astra-rule')), '--check', str(missing)], capture_output=True, timeout=20)
        assert result.returncode != 0, 'uncached remote failure must reject startup'
        assert cache.read_bytes() == cached
    print('PASS offline conversion, inline/file/HTTP providers, reference order, live updates, invalid-download cache retention, MMDB/GeoIP DAT/GEOSITE lookup, missing-category rejection, failed reload retention, offline cached startup')


if __name__ == '__main__':
    main()
