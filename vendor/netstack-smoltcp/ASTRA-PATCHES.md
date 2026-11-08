# Astra netstack patch

Based on the netstack-smoltcp 0.2.4 source used by this workspace, under its
included MIT license. Kept locally through Cargo's crates.io patch mechanism.
Existing window sizing, half-close and cooperative scheduling fixes are retained.

Memory lifetime fixes:

- Deduplicate initial SYNs by source/destination socket address; retransmissions
  reach the existing socket instead of allocating another set of TCP buffers.
- Limit live TCP controls before allocating windows (default 256). Pending
  accepts count too; excess SYNs are dropped and can be retried by the sender.
- Abort and reclaim the stack socket when the application stream is dropped;
  clear saved task wakers. Explicit write shutdown still uses the existing
  graceful half-close path while the stream remains alive.
- Bound virtual-device ingress to 256 packets with backpressure.
- Add `tcp_staging_buffer_size` to size both application staging rings independently
  of the smoltcp send/receive windows. The default preserves the historical
  per-direction window-sized staging; explicit zero is clamped to one byte.

Astra configures two 256 KiB TCP windows and two 64 KiB staging rings per socket,
with a 256-socket ceiling: 640 KiB per socket and 160 MiB maximum for these four
buffers, down from 1 MiB per socket and 256 MiB. This is not a limit on the
entire process's memory; TCP window capacity and half-close behavior are unchanged.
Regression coverage lives in astra-proxy's platform/tun_ingress.rs and exercises
retransmissions, capacity overflow and repeated incomplete-connection teardown
without a privileged TUN adapter.
Vendor unit tests also accept a real SYN and check that custom staging capacities
leave the smoltcp windows unchanged, including the default per-direction sizing.
