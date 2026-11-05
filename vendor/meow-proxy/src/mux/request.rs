//! Mux request header: version / protocol / padding (sing-mux protocol.go).

use bytes::{BufMut, BytesMut};
use std::io;

pub const VERSION_0: u8 = 0;
pub const VERSION_1: u8 = 1;

/// Encoded mux request header written right after the proxy handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub version: u8,
    pub protocol: u8,
    pub padding: bool,
}

impl Request {
    pub fn new(version: u8, protocol: u8, padding: bool) -> Self {
        Self {
            version,
            protocol,
            padding,
        }
    }

    /// Serialise the header.  When `padding` is set the payload includes
    /// random padding (length 256 + rand(512), matching sing-mux).
    pub fn encode(&self) -> Vec<u8> {
        let mut out = BytesMut::with_capacity(3 + 2 + 768);
        out.put_u8(self.version);
        out.put_u8(self.protocol);
        if self.version == VERSION_1 {
            out.put_u8(u8::from(self.padding));
            if self.padding {
                let len: u16 = 256 + (rand::random::<u16>() % 512);
                out.put_u16(len);
                let mut pad = vec![0u8; len as usize];
                for b in &mut pad {
                    *b = rand::random();
                }
                out.put_slice(&pad);
            }
        }
        out.to_vec()
    }

    /// Parse a request header from the stream prefix.  Returns the parsed
    /// header and the number of bytes consumed (including padding).
    pub fn decode(buf: &[u8]) -> io::Result<(Self, usize)> {
        if buf.len() < 2 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "mux request too short",
            ));
        }
        let version = buf[0];
        let protocol = buf[1];
        let mut consumed = 2;
        if !(VERSION_0..=VERSION_1).contains(&version) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unsupported mux version: {version}"),
            ));
        }
        // Protocols are smux=0 / yamux=1 / h2mux=2 (sing-mux Protocol* iota).
        if protocol > 2 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("unknown mux protocol: {protocol}"),
            ));
        }
        let mut padding = false;
        if version == VERSION_1 {
            if buf.len() < consumed + 1 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "mux padding flag missing",
                ));
            }
            padding = buf[consumed] != 0;
            consumed += 1;
            if padding {
                if buf.len() < consumed + 2 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "mux padding length missing",
                    ));
                }
                let len = u16::from_be_bytes([buf[consumed], buf[consumed + 1]]) as usize;
                consumed += 2;
                if buf.len() < consumed + len {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "mux padding truncated",
                    ));
                }
                consumed += len;
            }
        }
        Ok((
            Request {
                version,
                protocol,
                padding,
            },
            consumed,
        ))
    }
}

#[cfg(test)]
#[path = "../test/mux_request_test.rs"]
mod tests;
