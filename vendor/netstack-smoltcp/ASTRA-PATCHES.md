# Astra netstack patch

Based on the netstack-smoltcp 0.2.4 source used by this workspace, under its
included MIT license. Kept locally through Cargo's crates.io patch mechanism.
Existing window sizing, half-close and cooperative scheduling fixes are retained.

Memory lifetime fixes:

- Deduplicate initial SYNs by source/destination socket address; retransmissions
  reach the existing socket instead of allocating four more TCP windows.
- Limit live TCP controls before allocating windows (default 256). Pending
  accepts count too; excess SYNs are dropped and can be retried by the sender.
- Abort and reclaim the stack socket when the application stream is dropped;
  clear saved task wakers. Explicit write shutdown still uses the existing
  graceful half-close path while the stream remains alive.
- Bound virtual-device ingress to 256 packets with backpressure.

Astra configures four 256 KiB windows per socket and a 256-socket ceiling:
256 MiB maximum for these windows, not a limit on the entire process's memory.
Regression coverage lives in astra-proxy's platform/tun_ingress.rs and exercises
retransmissions, capacity overflow and repeated incomplete-connection teardown
without a privileged TUN adapter.
