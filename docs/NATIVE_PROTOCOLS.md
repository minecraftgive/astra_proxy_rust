# Native protocol configuration — 0.1.0

Astra runs two Rust executables. Protocol codecs are linked Rust libraries (`meow-proxy`, `meow-transport`, `shadowsocks`); there is no external proxy-core process or FFI proxy core. TLS uses rustls/AWS-LC and BoringSSL crypto libraries. Building the latter requires CMake, Clang/C++ and libclang. This is not a claim that every cryptographic dependency is written in Rust.

`configs/native.toml` shows a complete VLESS + XHTTP `stream-one` example. Replace the server, UUID, SNI and path with your node's values. Rules select the configured tag with `action = "proxy:edge"`.

```
cargo build --release
python3 scripts/astra.py --check --proxy configs/native.toml --rules configs/proxy-all-rule.toml
python3 scripts/astra.py --proxy configs/native.toml --rules configs/proxy-all-rule.toml
```

The launcher manages only `astra-rule` and `astra-proxy`. Either binary also runs independently. Native TLS CA paths are relative to the proxy configuration file. System trust is the default; custom `ca_file` adds trust, not a verification bypass.

Each native outbound has `kind = "native"`, a `tag`, and `[outbounds.config]`:

| protocol | Required node fields | Optional fields |
|---|---|---|
| `vless` | `server`, `port`, `uuid` | `flow = "xtls-rprx-vision"`, `mux` |
| `vmess` | `server`, `port`, `uuid` | `cipher`: auto / aes-128-gcm / chacha20-poly1305; `mux` |
| `trojan` | `server`, `port`, `password`, TLS section | transport section |
| `shadowsocks` | `server`, `port`, `cipher`, `password` | AEAD and SS2022 methods validated by the codec |
| `tuic` | `server`, `port`, `uuid`, `password`, TLS section | `tuic_udp_mode`: native / quic; `congestion_control`: cubic / bbr / new_reno; `tuic_idle_timeout_secs` |
| `hysteria2` | `server`, `port`, `password` | `pin_sha256` (server certificate SHA-256 hex), `obfs_password` (Salamander), TLS server name |

`[outbounds.config.tls]`: `server_name`, optional `ca_file`, `fingerprint`, `reality_public_key` (base64url), `reality_short_id` (hex). REALITY TCP/gRPC and all three XHTTP modes have passed local Xray interoperability; a wrong public key was rejected. Vision requires TLS/REALITY; HTTP transports additionally require native/xorpub VLESS Encryption. Hysteria2 uses public roots or explicit certificate pinning; its custom CA-file option is not implemented.

`[outbounds.config.transport]`: `kind` is `tcp`, `ws`, `grpc`, `http_upgrade`, or `xhttp`. Optional `path`, `host`, `service_name` and `headers` apply to the relevant transport. XHTTP accepts `mode = "stream-one"`, `"stream-up"`, `"packet-up"` or `"auto"` over HTTP/2. All three modes passed an 8 MiB local payload test. Packet-up uses numbered POSTs, bounded to four in flight with 64 KiB chunks, and validates upload acknowledgments. Packet-up cannot signal independent upload EOF: shutdown flushes queued POSTs and returns Unsupported so the outer relay closes the tunnel; response-after-upload-EOF applications must not assume half-close on this mode. Stream-up owns and cancels its HTTP/2 tasks with the connection and checks upload response status. `mux` accepts `muxcool`, `smux`, `yamux`, or `h2mux` for VLESS/VMess; peer support is required. All four modes passed concurrent VLESS/VMess TCP and UDP tests: smux/yamux/h2mux against sing-box, muxcool against Xray. Mux.Cool IP UDP shares the pool; domain UDP uses a separate native connection and retains remote DNS.

## Reference-server tests

Reference executables are test fixtures only. The installer pins official release versions and SHA-256 digests. They are not downloaded or started by Astra.

```
python3 scripts/install_test_servers.py
cargo build
python3 scripts/protocol_matrix.py --case vless-xhttp:stream-one
```

The full default TCP matrix now passes all 24 configurations in the macOS arm64 release build with 64 KiB and close after echo. UDP and strict half-close diagnostics include the confirmed peer/mode limitations listed in COMPATIBILITY.md. See V1_ACCEPTANCE.md for the release gate. A successful individual test proves only that named configuration, not every server option or public deployment.

The basic matrix closes the upload only after receiving the echo. `--trailer` is a stricter diagnostic: the destination replies after upload EOF. This diagnostic fails with the Xray VLESS/TLS and XHTTP fixtures; do not assume full end-to-end TCP half-close support. A minimal VLESS client bypassing Astra reproduced the missing tail response on Xray; sing-box preserved it. Native VLESS and VMess through sing-box passed 8 MiB with the post-EOF trailer. The isolated Rust stream-up transport does pass a response-after-upload-EOF test.

## SOCKS5 UDP

SOCKS5 supports UDP ASSOCIATE with IPv4, IPv6 and domain destinations. Rules receive `network = "udp"`; TCP and UDP decisions have separate cache keys. UDP proxy targets retain their domain for remote resolution. The TCP control connection owns the association; closing it closes all flows. FRAG != 0 is discarded (no SOCKS fragment reassembly). An association accepts only its TCP peer IP and declared UDP port, or pins the first valid packet's source port when the client declared zero.

Optional top-level limits (defaults):

```toml
max_udp_flows = 512
udp_targets_per_association = 32
udp_idle_timeout_secs = 120
```

Each flow has a four-datagram outgoing queue. Capacity exhaustion drops UDP packets; it does not block unrelated targets. Associations count toward `max_connections`. Both per-target flows and associations expire when idle. Existing UDP flows keep their selected route until close/expiry; new targets must obtain a rule decision.

Verified UDP outbounds: direct, SOCKS5 (including username/password), native VMess/TLS, Trojan/TLS, TUIC v5, Shadowsocks/SS2022, Hysteria2, VLESS over TLS/REALITY/WS/gRPC/HTTPUpgrade/XHTTP stream-one, stream-up and packet-up. Proxy matrix tests cover 1, 64, 1200 and 4096 bytes with IPv4, IPv6 and domain destinations. Direct also passes empty and 8192-byte datagrams. Run `scripts/udp_regression.py` for source isolation, malformed packets, flow limits and cleanup; use `scripts/protocol_matrix.py --udp --case ...` for reference-server interoperability.

UDP size/empty-packet behavior depends on the reference server; verified boundaries are explicit below. REALITY UDP now passes 1..4096-byte probes: the original five-second probe timeout was shorter than the configured ten-second handshake deadline; a fifteen-second probe passed repeated runs. VMess/TLS passes 1..4096 bytes against Xray and empty through 8192-byte datagrams against sing-box; the Xray 8192-byte probe still times out. Legacy `trojan_tls`/`vless_tls` entries remain TCP-only; use `kind = "native"` for their UDP-capable variants. `--udp-edge` additionally tests empty and 8192-byte packets; several reference paths still fail. Hysteria2's native codec now allows 65507-byte payloads and passed up to 32768 bytes against a local QUIC peer. With sing-box, 8192 bytes reach the target intact but its reply returns only 4096 bytes; this end-to-end limitation remains. Empty packets pass. These reference-path failures remain documented limitations and are not counted as successful validation.

The linked `meow-proxy` 0.23.1 source is pinned under `vendor/meow-proxy` with its MIT notice and `ASTRA_PATCHES.md`: SS/Hysteria2/SOCKS5 accept matching bound-domain replies without resolving them locally. Other domain replies remain rejected.

SOCKS5 upstream examples use the existing outbound format and now support both TCP and UDP:

```toml
[[outbounds]]
kind = "socks5"
tag = "edge"
server = "127.0.0.1:1081"
username = "your-user"
password = "your-password"
```

Omit both credentials for an unauthenticated server. `kind = "http"` uses the same fields for TCP CONNECT (HTTP Basic authentication); HTTP CONNECT does not carry UDP. Both upstream types have verified positive and invalid-credential TCP tests. SOCKS5 UDP close explicitly releases its upstream control connection.

## TUIC v5

`configs/tuic.toml` is a complete template. Replace its node values, then run:

```sh
python3 scripts/astra.py --check --proxy configs/tuic.toml --rules configs/proxy-all-rule.toml
python3 scripts/astra.py --proxy configs/tuic.toml --rules configs/proxy-all-rule.toml
```

The supplied rule file sends traffic through `edge`; the original `configs/rule.toml` defaults to direct routing. TUIC uses the native Rust Quinn/rustls QUIC implementation, verifies server certificates, and authenticates using the TUIC v5 TLS exporter token. ALPN is currently `h3`. System trust is the default and `ca_file` adds a private CA. REALITY/fingerprint/HTTP transport/mux settings are rejected for TUIC. TCP and UDP share a QUIC connection, with periodic heartbeats only while relay sessions are active and reconnect on a later request after connection loss. `tuic_idle_timeout_secs` defaults to 15 (range 3..3600); heartbeats run every one-third of that period. This also bounds detection of a vanished server that sends no QUIC close notification. TUIC has no authentication or target-connect acknowledgment; a failure may surface on the first payload I/O after SOCKS CONNECT succeeds.

Both UDP modes (`native` datagrams and `quic` streams) passed IPv4, IPv6, domain, empty and 8192-byte payloads against sing-box. Native UDP fragments/reassembles oversized QUIC datagrams; reassembly retains at most eight incomplete packets and 65507 payload bytes per association for five seconds. The connection-wide UDP delivery queue has a 2 MiB byte budget; unidirectional receive tasks are capped at 32 with five-second deadlines. TCP passed 8 MiB and a response arriving after upload EOF. `scripts/tuic_regression.py` validates twenty batches of 32 concurrent TCP streams plus UDP sharing one connection, server restart, invalid password, wrong SNI and an untrusted certificate.

The TUIC restart/concurrency regression uses a TCP echo listener backlog of 128. The original small test-server backlog could reject simultaneous target connections; the revised fixture avoids treating target-server overload as a protocol failure. Initial server-restart tests also exposed the original 60-second stale-connection window, now reduced to the configurable 15-second default.

## Inner TLS / Vision verification

`python3 scripts/protocol_matrix.py --case vless-vision --tls-payload --payload-bytes 8388608` checks a verified TLS 1.3 target and requires logs confirming both Vision read and write raw passthrough. This passed against Xray. Use Python linked with TLS 1.3-capable OpenSSL; the script rejects older SSL runtimes explicitly. `--udp-size N` can be repeated to diagnose individual datagram boundaries independently of the standard edge sequence.

Hysteria2 reassembly now retains at most eight incomplete packets and 65507 bytes total per session, with a ten-second expiry. Duplicates do not consume additional budget; conflicting fragment counts/addresses discard the packet. The packet framing remains the standard [Hysteria2 UDP format](https://v2.hysteria.network/docs/developers/Protocol/#udp).

## Mux regression

`python3 scripts/mux_regression.py --bin-dir target/release` verifies eight VLESS/VMess × mux combinations. Each opens 65 TCP streams (including two concurrent batches), then IPv4/IPv6/domain UDP. A transparent TCP counter proves reuse: sing-mux holds at most four physical connections; Mux.Cool uses the same pool for IP UDP and one additional native connection for the domain target. The test validates this fallback explicitly, rather than treating it as pooled domain UDP.

`python3 scripts/half_close_diagnostic.py` uses a minimal plain VLESS client, without Astra, to compare the two pinned reference servers after TCP write EOF. This diagnostic reproduces the Xray/sing-box difference. `--case sing-vless --trailer --payload-bytes 8388608` verifies the corresponding native Astra behavior with sing-box.

Additional UDP edge coverage passed against sing-box for VLESS, Shadowsocks AES-128-GCM (`--case sing-shadowsocks-aead`) and SS2022 (`--case sing-shadowsocks`), including empty and 8192-byte packets with IPv4/IPv6/domain targets. These diagnostic cases supplement the default 24-case matrix.

Xray Shadowsocks IPv4 boundary probe: 8153-byte payload succeeded, 8154 timed out; with the 7-byte IPv4 destination, 16-byte salt and 16-byte AEAD tag, that is an observed 8192-byte encrypted-packet boundary. This observation does not apply unchanged to domain/IPv6 headers. Sing-box accepted 8192-byte application payloads for all three address forms.

## VLESS Encryption and certificate pinning

`[outbounds.config].encryption` accepts `none` (default) or the full `mlkem768x25519plus` client string (native/xorpub/random and 1rtt/0rtt, parsed by the native codec). Vision over HTTP additionally requires native/xorpub Encryption. `[outbounds.config.tls].cert_pin_sha256` accepts a 64-character certificate SHA-256 hex value; it replaces CA verification with a pin check and cannot be combined with ca_file or REALITY. The existing `fingerprint` field remains the TLS ClientHello profile, not a certificate pin. TUIC/Hysteria2 do not use this new TLS pin field.

Explicit XHTTP `mode="auto"` selects packet-up for ordinary TLS and stream-one for REALITY, matching upstream single-transport selection. Omitting mode retains Astra's previous stream-one default. Private node configurations and live verification records are kept in the local, Git-ignored `Local/` directory.
