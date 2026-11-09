#!/usr/bin/env python3
"""One bounded physical trace to an explicitly supplied local test peer.

Run manually with sudo on macOS to verify the privileged adapter. This does not
install a service, alter routes/TUN/firewall, or use a resolver. Only one UDP TTL
probe is allowed; a timeout remains no_response, never inferred reachability.
"""
import argparse
import ipaddress
import json
import os
import platform
import subprocess
import tempfile
from pathlib import Path

parser = argparse.ArgumentParser(description=__doc__)
parser.add_argument('--binary', required=True)
parser.add_argument('--interface', required=True)
parser.add_argument('--target', required=True)
args = parser.parse_args()
if platform.system() != 'Darwin' or os.geteuid() != 0:
    parser.error('This acceptance scenario requires a manually authenticated macOS root session')
address = ipaddress.ip_address(args.target)
if not address.is_private or address.is_loopback or address.is_unspecified:
    parser.error('Use the explicitly selected private-network test peer, not a public service')
request = {'egress': {'kind': 'interface', 'name': args.interface},
           'probe': {'kind': 'trace', 'host': str(address), 'max_hops': 1, 'probes_per_hop': 1},
           'count': 1, 'interval_ms': 0, 'timeout_ms': 500,
           'family': 'ipv4' if address.version == 4 else 'ipv6'}
result = subprocess.run([str(Path(args.binary).resolve()), '--network-diagnostics-stream', '/tmp/unused-config'],
                        input=json.dumps(request), text=True, capture_output=True, timeout=10)
events = [json.loads(line) for line in result.stdout.splitlines() if line.startswith('{')]
report = {'scenario': 'macos_root_physical_trace', 'uid': os.geteuid(), 'exit_code': result.returncode,
          'events': events, 'stderr': result.stderr}
fd, output = tempfile.mkstemp(prefix='astra-macos-root-path-', suffix='.json')
with os.fdopen(fd, 'w') as stream:
    json.dump(report, stream, ensure_ascii=False, indent=2)
if 'SUDO_UID' in os.environ:
    os.chown(output, int(os.environ['SUDO_UID']), int(os.environ['SUDO_GID']))
print('Evidence:', output)
summary = next((event for event in events if event.get('event') == 'trace_summary'), None)
assert summary and summary['capability']['supported'], 'Privileged trace capability unavailable; inspect evidence'
assert summary['probes_sent'] == 1 and summary['physical']['binding_verified'] and summary['physical']['route_verified'], 'Physical path not confirmed; inspect evidence'
assert events[-1]['event'] == 'done', 'Diagnostic stream did not complete'
print('PASS: root adapter sent one physical-interface probe; response outcome is retained without inference')
