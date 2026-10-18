# Astra patches to meow-proxy 0.23.1

Source: crates.io meow-proxy 0.23.1; upstream https://github.com/meow-rs/meow-rs (MIT, see LICENSE).

- Shadowsocks raw UDP and Hysteria2: accept a reply carrying the same domain and port as the association's bound target. Return an unspecified IP to the caller, which retains the original domain. Unknown/unbound domain sources remain rejected. No local DNS resolution is introduced. Required for real sing-box domain-target UDP interoperability.

Astra integration coverage: `python3 scripts/protocol_matrix.py --udp --case sing-shadowsocks --case sing-hysteria2`.

- SOCKS5 UDP: likewise accept only a matching bound-domain source in replies, preserving remote DNS. Verified against Xray with and without username/password authentication.
- Expose `TrojanPacketConn::new` so Astra can reuse the frame codec after its custom-CA/composable-transport UDP handshake. The framing implementation is unchanged.

- VMess native UDP: one authenticated body record per datagram, lazy response header, independently locked read/write halves, bounded record lengths, and poisoned state after a canceled/incomplete frame. Plain UDP is now advertised when enabled; TCP-only mux falls back to this path. IPv4/IPv6/domain 1..4096-byte packets pass Xray; sing-box additionally passes empty and 8192-byte datagrams. Xray still times out at 8192 bytes.

VMess codec tests (the vendor package is excluded from the parent workspace so its own dev-dependencies can be tested):

```sh
cargo test --manifest-path vendor/meow-proxy/Cargo.toml --no-default-features --features vmess,mux --lib vmess::
```

Tests cover client-first UDP, AES-GCM/ChaCha20-Poly1305, record boundaries, empty packets, oversized receive-buffer draining, and cancellation of partially read headers or partially written frames. Reference interoperability: `python3 scripts/protocol_matrix.py --udp --case vmess-tls` and `python3 scripts/protocol_matrix.py --udp-edge --case sing-vmess`.

- Hysteria2 UDP: raise the native payload cap to 65507 bytes and bound reassembly to eight packets / 65507 total retained bytes / ten seconds. Reject conflicting addresses/counts and avoid charging duplicates twice. Native QUIC tests pass empty through 32768 bytes; sing-box still truncates the return of an 8192-byte echo to 4096, despite delivery of all 8192 bytes to the target. Test with `cargo test --manifest-path vendor/meow-proxy/Cargo.toml --no-default-features --features hysteria2 --lib hysteria2::`.

- VLESS Encryption over buffered transports flushes every encrypted record. The relay now exposes a Vision-aware stream that switches at exact plaintext write offsets and waits for acknowledged record consumption before reading ahead. DIRECT bypasses the VLESS Encryption record layer while preserving the underlying HTTP/TLS transport. This avoids decrypting already-raw TLS bytes and losing buffered data at the transition. Native raw TCP/TLS Vision behavior remains unchanged. Integration: scripts/vless_node_regression.py --inner-tls (TLS 1.3-capable Python).
