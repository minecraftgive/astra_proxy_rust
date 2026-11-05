//! VLESS request/response header encoding and address encoding.
//!
//! # Wire format
//!
//! Request header (upstream: transport/vless/encoding.go::EncodeRequestHeader):
//! ```text
//! version(1)          = 0x00
//! uuid(16)            = binary UUID
//! addon_length(1)     = byte length of addon blob
//! addon(addon_length) = protobuf-encoded addons (see below)
//! command(1)          = 0x01 TCP | 0x02 UDP
//! port(2)             = destination port, big-endian
//! addr_type(1)        = 0x01 IPv4 | 0x02 domain | 0x03 IPv6
//! address             = variable (see below)
//! ```
//!
//! **NOTE:** `port` comes BEFORE `addr_type`. This is the VMess/VLESS convention;
//! human-readable specs often list address first, which is misleading.
//!
//! Response header:
//! ```text
//! version(1)          = 0x00 (echoes client)
//! addon_length(1)     = usually 0; addon bytes follow and are discarded
//! ```
//!
//! Addon encoding (plain protobuf, no prost dep):
//! - flow = ""  → addon_length = 0, no addon bytes
//! - flow = "xtls-rprx-vision" → addon_length = 18,
//!   addon = [0x0A, 0x10, b"xtls-rprx-vision"]
//!   where 0x0A = field 1 wire-type 2, 0x10 = varint 16 (string length)
// upstream SHA: xray-core/xray-core (2024) — pin on first Vision integration

use bytes::{BufMut, BytesMut};
use meow_common::Metadata;

// ─── Address type ─────────────────────────────────────────────────────────────

/// Destination address for a VLESS connection.
#[derive(Debug, Clone)]
pub(crate) enum VlessAddr {
    Ipv4([u8; 4]),
    Ipv6([u8; 16]),
    /// UTF-8 domain name; max 255 bytes enforced at construction.
    /// Uses `SmolStr` — domains ≤23 bytes inline without heap allocation.
    Domain(smol_str::SmolStr),
}

impl VlessAddr {
    /// Construct a domain address, returning `Err` if the name exceeds 255 bytes.
    ///
    /// 256-byte domains cause a protocol error (1 byte for the length field)
    /// with no diagnostic on silent truncation — reject early. Class A per ADR-0002.
    pub(crate) fn domain(s: &str) -> std::result::Result<Self, String> {
        if s.len() > 255 {
            return Err(format!(
                "vless: domain '{}…' is {} bytes; max 255 (would be silently truncated)",
                &s[..s.len().min(20)],
                s.len()
            ));
        }
        Ok(VlessAddr::Domain(smol_str::SmolStr::new(s)))
    }
}

/// Derive a `VlessAddr` from connection metadata.
///
/// Prefers `host` (domain), falls back to `dst_ip`. Returns `Err` when the
/// name exceeds the one-byte domain-length field — callers pick the class:
/// `dial_udp` maps to `NotSupported` so a chained caller can fall back to a
/// locally-resolved `UdpTarget::Addr` (issue #657).
pub(crate) fn addr_from_metadata(m: &Metadata) -> std::result::Result<VlessAddr, String> {
    if !m.host.is_empty() {
        VlessAddr::domain(&m.host)
    } else if let Some(ip) = m.dst_ip {
        Ok(match ip {
            std::net::IpAddr::V4(v4) => VlessAddr::Ipv4(v4.octets()),
            std::net::IpAddr::V6(v6) => VlessAddr::Ipv6(v6.octets()),
        })
    } else {
        Ok(VlessAddr::Domain(smol_str::SmolStr::default()))
    }
}

// ─── Command byte ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Cmd {
    Tcp = 0x01,
    Udp = 0x02,
    /// `0x03` — Mux.Cool session (Xray `CommandMux`): the request carries
    /// no port/address; per-stream targets ride mux frames on the stream.
    #[cfg(feature = "mux")]
    Mux = 0x03,
}

// ─── Request encoder ──────────────────────────────────────────────────────────

/// Encode a VLESS request header into `dst`.
///
/// Caller must have validated domain length (≤ 255 bytes) before calling.
///
/// upstream: transport/vless/encoding.go::EncodeRequestHeader
pub(crate) fn encode_request(
    dst: &mut BytesMut,
    uuid_bytes: &[u8; 16],
    flow: Option<&str>,
    cmd: Cmd,
    dst_port: u16,
    addr: &VlessAddr,
) {
    // version
    dst.put_u8(0x00);

    // uuid (16 bytes, binary form)
    dst.put_slice(uuid_bytes);

    // addon (shared with the Mux request encoder below)
    put_flow_addon(dst, flow);

    // command
    dst.put_u8(cmd as u8);

    // port (big-endian)  ← NOTE: port comes BEFORE addr_type
    dst.put_u16(dst_port);

    // addr_type + address
    match addr {
        VlessAddr::Ipv4(octets) => {
            dst.put_u8(0x01);
            dst.put_slice(octets);
        }
        VlessAddr::Ipv6(octets) => {
            dst.put_u8(0x03);
            dst.put_slice(octets);
        }
        VlessAddr::Domain(name) => {
            dst.put_u8(0x02);
            let b = name.as_bytes();
            dst.put_u8(b.len() as u8); // len(1 byte) — max 255 enforced by VlessAddr::domain()
            dst.put_slice(b);
        }
    }
}

/// Append the flow addon block (length byte + protobuf blob, if any).
///
/// NOT prost — two hardcoded bytes + string copy (spec §Addon encoding).
fn put_flow_addon(dst: &mut BytesMut, flow: Option<&str>) {
    match flow {
        Some("xtls-rprx-vision") => {
            dst.put_u8(18); // addon_length = 18
            dst.put_u8(0x0A); // protobuf field 1, wire type 2
            dst.put_u8(0x10); // varint 16  (len("xtls-rprx-vision") = 16)
            dst.put_slice(b"xtls-rprx-vision");
        }
        _ => {
            dst.put_u8(0x00); // addon_length = 0
        }
    }
}

/// Encode a VLESS Mux.Cool session request: `version + uuid + addons +
/// command 0x03` — **no port/address**.
///
/// upstream: xray proxy/vless/encoding/encoding.go::EncodeRequestHeader —
/// for `RequestCommandMux` the command byte is written and port/address
/// are omitted (the `if command != ...Mux` guard); sing-vmess
/// vless/protocol.go::ReadRequest mirrors it by skipping the address parse
/// for `CommandMux`.
#[cfg(feature = "mux")]
pub(crate) fn encode_mux_request(dst: &mut BytesMut, uuid_bytes: &[u8; 16], flow: Option<&str>) {
    dst.put_u8(0x00); // version
    dst.put_slice(uuid_bytes);
    put_flow_addon(dst, flow);
    dst.put_u8(Cmd::Mux as u8); // command — signals the Mux.Cool session
}

// The VLESS response header `[version][addon_length][addon…]` is consumed
// by `VlessConn::poll_read` (TCP) and `VlessUdpReader::read_packet` (UDP)
// with persistent, cancellation-safe progress in `vless/conn.rs`.

// ─── Unit tests (§A, §B) ─────────────────────────────────────────────────────

#[cfg(test)]
#[path = "../test/vless_header_test.rs"]
mod tests;
