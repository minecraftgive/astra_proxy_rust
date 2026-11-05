//! sing `Socksaddr` stream addressing (client side of sing-mux streams).
//!
//! The full per-stream request prefix is
//! `[flags u16 BE][type u8][address][port u16 BE]` — flags 0 for TCP —
//! matching sing-mux's `EncodeStreamRequest` (protocol.go).  Socksaddr type
//! bytes: 0x01=IPv4, 0x04=IPv6, 0x03=FQDN (u8 length-prefixed).  Matches
//! `M.SocksaddrSerializer` in sagernet/sing.

use bytes::{BufMut, BytesMut};
use std::io;
use std::net::IpAddr;

const TYPE_IPV4: u8 = 0x01;
const TYPE_IPV6: u8 = 0x04;
const TYPE_FQDN: u8 = 0x03;

/// Encode the full TCP stream request: `[flags u16 = 0][socksaddr]`.
pub fn encode_stream_request(host: &str, port: u16) -> io::Result<Vec<u8>> {
    encode_stream_request_with_flags(host, port, 0)
}

/// Encode a stream request with explicit flags (sing-mux flagUDP = 1).
pub fn encode_stream_request_with_flags(host: &str, port: u16, flags: u16) -> io::Result<Vec<u8>> {
    let mut out = BytesMut::new();
    out.put_u16(flags);
    out.put_slice(&encode_address(host, port)?);
    Ok(out.to_vec())
}

/// Encode a destination Socksaddr (without the stream-request flags).
///
/// FQDN payloads use a 1-byte length.  Hosts longer than 255 bytes are
/// invalid as DNS names and cannot be encoded; they are rejected instead
/// of silently truncating the frame (the server would dial a wrong
/// address).
pub fn encode_address(host: &str, port: u16) -> io::Result<Vec<u8>> {
    let mut out = BytesMut::new();
    match host.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => {
            out.put_u8(TYPE_IPV4);
            out.put_slice(&ip.octets());
        }
        Ok(IpAddr::V6(ip)) => {
            out.put_u8(TYPE_IPV6);
            out.put_slice(&ip.octets());
        }
        Err(_) => {
            if host.len() > u8::MAX as usize {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("mux address fqdn exceeds 255 bytes: {host:?}"),
                ));
            }
            let len = host.len() as u8;
            out.put_u8(TYPE_FQDN);
            out.put_u8(len);
            out.put_slice(host.as_bytes());
        }
    }
    out.put_u16(port);
    Ok(out.to_vec())
}

#[cfg(test)]
#[path = "../test/mux_address_test.rs"]
mod tests;
