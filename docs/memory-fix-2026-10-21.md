# 2026-10-21 proxy memory investigation

The running Windows proxy (PID 14632) showed about 7 GiB resident memory and
73 GiB private committed memory during inspection. Its elevated command line
and configuration were unavailable, so its operating mode and the complete
cause of that instance's growth have not been established.

The TUN TCP stack had independently verifiable allocation/lifetime defects:
every initial SYN, including retransmissions, created four windows; admission
occurred before the application's connection limiter; dropped streams could
leave sockets and buffers until the two-hour TCP timeout.

The vendored netstack patch deduplicates SYNs, caps allocation before acceptance,
reclaims sockets when application streams are dropped, and bounds ingress.
Astra's 256-socket ceiling limits these TCP windows to 256 MiB. It also limits
concurrent TUN TCP connections; excess new SYNs wait for client retransmission.
Other process allocations are outside this budget. Explicit write half-close
continues to drain while the stream remains alive.

Validation: workspace tests, including 32 cycles of repeated SYNs, excess distinct
SYNs, and incomplete connection teardown; existing raw TCP handshake/payload and
UDP routing tests. Tests use an isolated in-memory stack, not the user's adapter.
Real-world memory stability must still be checked after restarting with the new
binary under the user's workload. The running instance is not modified in place.
