# Native physical path diagnostics

`network_diagnostics_path_v1` identifies the new trace/echo event format. It is
checked on the actual diagnostic executable before launching a probe. Runtime
socket permission is reported separately; declaring this format does not grant
raw-socket permission or promise a route.

Trace requires an explicitly selected physical interface or a fully resolved,
interface-bound DIRECT leaf. System routing and proxy leaves are refused. The
physical guard obtains native hardware classification, link state, interface
identity and source addresses, binds both source and device, reads that binding
back, and performs a constrained kernel route lookup without UDP payload. It
rechecks before and after each sample. An unavailable, changed or unclassifiable
interface stops probing without fallback. Host-local destinations (including
addresses on a different TUN interface) cannot produce physical-path evidence.
No helper process, route mutation, subscription refresh or elevation is used.

Linux uses UDP TTL/Hop-Limit and its private socket's error queue. macOS uses UDP
and raw ICMP receive; lack of raw-socket permission stops before resolution or
sending. Windows uses native ICMP APIs with TTL/Hop-Limit and an explicit source.
Because these APIs do not expose a device-binding argument, the selected source's
unconstrained native route is also checked before/after the call; a route choosing
another device, including TUN, is refused. The ordinary socket binding and source
checks remain required. These checks describe the sampled OS path, not a claim
about devices or networks beyond the selected interface.

ICMP errors must correlate with the probe's destination, local socket and packet
quotation/nonce. Silence is only `no_response`; it is never a missing router,
broken link or monitoring conclusion. Defaults are 30 hops, 3 probes each and a
1-second timeout, which are also hard upper bounds for a trace request. Hop RTT
is not merged into end-to-end ping statistics. Windows status values and native
I/O errors remain distinct from timeout.

For system ICMP with known active TUN, no echo is sent and the observation says
TUN-local handling can occur; it contains no RTT or reachability claim. With an
otherwise unconfirmed system path, an observed echo is likewise excluded from
RTT/reachability conclusions. A selected physical path permits correlated echo
RTT only after successful path checks. IPv6 reachability is independently tested
by the constrained TCP probe, never inferred from TUN echo.

## Reproducible checks

- `cargo test -p astra-platform diagnostic_path`: native permission inspection,
  loopback echo correlation, rejection of loopback/unknown physical choices,
  unconnected UDP binding, invalidation and raw ICMP quotation fixtures.
- `cargo test -p astra-proxy --lib path_probe`: TUN echo emits no resolution or
  network attempt, system trace refuses, proxies/unbound DIRECT refuse, and
  trace budgets are bounded before platform work.
- `cargo check -p astra-platform --tests --target x86_64-unknown-linux-gnu`
- `cargo check -p astra-platform --tests --target x86_64-pc-windows-msvc`

On Linux the platform suite additionally exercises real localhost IPv4 and IPv6
closed UDP ports through `MSG_ERRQUEUE`. The suite passed natively in the isolated
`astra-network-test` Linux VM as well as on the macOS host. Windows native checks
also observed IPv6 loopback echo and IPv4/IPv6 physical adapter binding readback.
IPv4 localhost did not respond to either the native adapter or independent system
ping on that host; only the explicit test setting `ASTRA_TEST_ICMP_V4_BLOCKED=1`
permits that known environmental no-response result, and still requires no RTT or
source evidence. This is not a successful IPv4 echo verification. Offline IPv4/IPv6
quotation fixtures are macOS adapter tests. Windows native execution has occurred:
4 path tests, 8 network-snapshot tests, physical binding readback and a real remote
Client → Controller → Agent → Core task flow are recorded in
[Windows native validation](network-windows-validation.md). Its virtual path to the
Mac is not counted as a Windows physical NAT success. Windows positive IPv4
ICMP, service-account and physical hot-removal scenarios remain unverified.
Cross-compilation alone is not runtime evidence. NAT's isolated namespace matrix is
documented in `network-nat-testing.md`.

## Physical path with a real Linux TUN

On 2026-11-09, `scripts/test_network_tun_vm.py` ran against the actual compiled
proxy in the disposable `astra-network-test` ARM64 Linux VM. A host-loopback STUN
fixture served the guest through its Ethernet device. With a real TUN and a
UDP-port-specific policy route installed inside the VM, an unbound control packet
entered TUN. The manually selected `eth0` NAT session received both basic and
repeat responses with binding/route evidence true, while the TUN received no NAT
packets. Known-TUN system ICMP emitted no RTT/reachability claim; system trace and
NAT selecting the virtual interface were refused. The script removed its rule,
route table and TUN, and cleanup was independently checked.

```sh
python3 scripts/test_network_tun_vm.py \
  --profile astra-network-test \
  --binary /tmp/astra-network-target/aarch64-unknown-linux-gnu/debug/astra-proxy
```

This is an opt-in disposable-VM acceptance script, not a product operation. It
requires `sudo -n` inside that VM and refuses pre-existing test names/rules. It
never changes host routes or deploys a public service. The guest's Ethernet is
virtualized hardware positively identified by that guest OS; this does not
replace the full Astra TUN-stack acceptance matrix on macOS or Windows.


## macOS physical binding and privileged trace

On 2026-11-09, the current proxy explicitly bound physical `en0` (source
`192.168.137.152`) and contacted the task-only Windows STUN fixture
`192.168.137.1:52220`. Both the initial binding and same-target repeat received
validated responses. The report completed with `partial=false`,
`binding_verified=true` and `route_verified=true`; the subprocess emitted `Done`.
The raw evidence is `/tmp/astra-macos-physical-nat-evidence.log`. This confirms
physical binding and basic observations. The fixture did not expose RFC 5780
alternate-IP/port tests, so mapping/filtering remained unknown rather than being
promoted to a full NAT classification.

The user separately authorized and authenticated a one-shot root trace on the Mac.
`/tmp/astra-macos-root-path-fj1abgnf.json` records UID 0 and exit 0, one UDP probe via
`en0` to `192.168.137.1`, and a correlated ICMP destination response.
`probes_sent=1`, `reached=true`, `completed=true`, and both physical binding and
route evidence are true. No host route or TUN setting was changed. This is distinct
from the ordinary-account sample, which correctly refused raw ICMP without
permission; the product does not elevate itself. A single LAN destination response
does not validate a public multihop path or macOS TUN-on behavior.

## Live Linux interface invalidation and restoration

`scripts/test_network_interface_vm.py` was executed against a freshly built proxy
in the dedicated `astra-network-test` Linux VM on 2026-11-09. Other test agents
confirmed the VM was idle first. A host-loopback STUN fixture initially produced
two successful physical `eth0` observations. In a separate session it withheld the
first reply until the guest's own Python process renamed `eth0` to `astraifcheck0`.
The device index, MAC and UP flag remained the same during the rename. The test did
not issue commands to delete addresses, change routes or bring the link down.

The next guard check stopped NAT after about 514 ms with `Selected interface is
unavailable`. The report retained `partial=true`, unknown mapping/filtering,
no traditional classification, and invalidated both binding and route evidence.
No delayed response was accepted. The host independently observed exactly one
STUN request in that session: no repeat, retransmission or fallback escaped after
the invalidation. Raw evidence is `/tmp/astra-linux-interface-evidence.log`; the
current-core build log is `/tmp/astra-linux-interface-build.log`.

The same guest process restored the original name in `finally`, coordinating with
the rename thread so cancellation cannot produce a late rename after cleanup.
The VM's network service automatically withdrew/reacquired DHCP during renaming;
this test therefore covers a real identity change with transient connectivity loss,
not a claim that only a string changed or that addresses stayed present throughout.
After restoration, the script waited boundedly for exact address, route and rule
state equality (ignoring only ticking lease lifetimes and list order); final recovery
took about 112 ms in the recorded run. An independent check confirmed `eth0` UP with
the original MAC, IPv4/IPv6 addresses, default route and rule priorities, and no
remaining proxy process. No host or Windows physical interface was changed.

```sh
python3 scripts/test_network_interface_vm.py \
  --profile astra-network-test \
  --binary /tmp/astra-network-target/aarch64-unknown-linux-gnu/debug/astra-proxy
```

This is an opt-in disposable-VM test. It does not substitute for physical cable
removal, adapter removal/replacement, or macOS/Windows physical-interface failure
matrices. The script never repairs failures by manually adding an IP or route.


## Actual isolated macOS TUN acceptance

`scripts/test_macos_isolated_tun.py` passed after explicit user authentication on
2026-11-09. The final evidence is
`/private/tmp/astra-macos-isolated-tun-oxq6unte.json`: UID 0, temporary Core PID
62474 with Runtime TUN enabled, and both `passed` and `cleanup_verified` true.
The fixture owned only `utun200`, a benchmark /30 address and one benchmark host
route; it did not replace a default route or alter the production instance.

A normal system ping over that TUN received a local echo. The diagnostic emitted
`tun_local=true` without a source, RTT or reachability claim. System trace emitted
one unsupported summary with zero probes, no resolution/trace/connection/attempt
observations, and exit 1 for the expected refusal. All successful probes still
require exit 0 and Done. Selecting the virtual adapter for NAT produced no
observations. Selecting physical `en0` received two validated private-fixture
STUN responses both before and during TUN operation, with binding and constrained
route evidence true. The independent Windows fixture logged the matching four
transactions in `/tmp/astra-windows-stun-session-21080.jsonl`.

The script stopped its own TUN/Core and verified its interface and routes were
gone, default/half-default routes and system proxy were unchanged, and production
PID 99006 retained the same runtime settings. A separate post-run check confirmed
the cleanup. The Windows fixture was then explicitly stopped; no endpoint or
fixture process remained. The authenticated terminal is deliberately kept open
at the user's request. Earlier expired-fixture preflight failures and an overly
strict test assertion are preserved separately and are not counted as passes.

The fixture must be live before invoking the opt-in script. Without `--execute`
it prints its plan. It never invokes sudo or elevates the product; authentication
is performed explicitly by the operator. This verifies the scoped native TUN
scenario, not every third-party full-tunnel setup or physical cable removal.
