Source: crates.io meow-common 0.23.1 (MIT, as declared in Cargo.toml).

Astra adds a task-local outbound-interface scope to src/outbound_iface.rs.
It overrides the process default without changing other concurrent connections.
The original process-wide TUN interface ownership remains the fallback.
