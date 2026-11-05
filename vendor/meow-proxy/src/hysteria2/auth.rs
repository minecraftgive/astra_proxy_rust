//! The single HTTP/3 authentication request, without HTTP/3 datagrams.
//!
//! Hysteria relays UDP as raw QUIC DATAGRAMs. Advertising SETTINGS_H3_DATAGRAM
//! makes quic-go start an HTTP/3 receiver that steals those datagrams. Quiche's
//! high-level h3 connection always advertises it when QUIC datagrams are on,
//! so send the small auth exchange directly, reusing quiche's QPACK codec.
//! Empty SETTINGS also leave the QPACK dynamic table disabled.

use super::{proto, Error, Result};
use quiche::h3::{qpack, Header, NameValue};

const MAX_AUTH_BYTES: usize = 64 * 1024;
const AUTH_STREAM: u64 = 0;
// Client-initiated unidirectional control stream: type 0, SETTINGS (4), length 0.
const SETTINGS: &[u8] = &[0, 4, 0];

pub(super) struct Auth {
    request: Vec<u8>,
    request_sent: usize,
    settings_sent: usize,
    response: Vec<u8>,
}

impl Auth {
    pub(super) fn new(password: &str, rx_bps: u64) -> Result<Self> {
        let padding = proto::auth_request_padding();
        let rx = rx_bps.to_string();
        let headers = [
            Header::new(b":method", b"POST"),
            Header::new(b":scheme", b"https"),
            Header::new(b":authority", b"hysteria"),
            Header::new(b":path", b"/auth"),
            Header::new(b"hysteria-auth", password.as_bytes()),
            Header::new(b"hysteria-cc-rx", rx.as_bytes()),
            Header::new(b"hysteria-padding", padding.as_bytes()),
        ];
        let mut block = vec![0; MAX_AUTH_BYTES];
        let n = qpack::Encoder::new()
            .encode(&headers, &mut block)
            .map_err(|e| Error::Http3(format!("auth headers: {e}")))?;
        let mut request = vec![1]; // HEADERS frame
        proto::put_varint(n as u64, &mut request)?;
        request.extend_from_slice(&block[..n]);
        Ok(Self {
            request,
            request_sent: 0,
            settings_sent: 0,
            response: Vec::new(),
        })
    }

    pub(super) fn poll(&mut self, conn: &mut quiche::Connection) -> Result<Option<bool>> {
        if !send(conn, 2, SETTINGS, &mut self.settings_sent, false)?
            || !send(
                conn,
                AUTH_STREAM,
                &self.request,
                &mut self.request_sent,
                true,
            )?
        {
            return Ok(None);
        }
        let mut buf = [0; 4096];
        loop {
            match conn.stream_recv(AUTH_STREAM, &mut buf) {
                Ok((n, fin)) => {
                    if self.response.len() + n > MAX_AUTH_BYTES {
                        return Err(Error::Auth("authentication response too large".into()));
                    }
                    self.response.extend_from_slice(&buf[..n]);
                    if fin {
                        return decode_response(&self.response).map(Some);
                    }
                }
                Err(quiche::Error::Done) => return Ok(None),
                Err(e) => return Err(Error::Http3(format!("auth receive: {e}"))),
            }
        }
    }
}

fn send(
    conn: &mut quiche::Connection,
    id: u64,
    bytes: &[u8],
    offset: &mut usize,
    fin: bool,
) -> Result<bool> {
    if *offset < bytes.len() {
        match conn.stream_send(id, &bytes[*offset..], fin) {
            Ok(n) => *offset += n,
            Err(quiche::Error::Done | quiche::Error::StreamLimit) => return Ok(false),
            Err(e) => return Err(Error::Http3(format!("auth send: {e}"))),
        }
    }
    Ok(*offset == bytes.len())
}

fn decode_response(bytes: &[u8]) -> Result<bool> {
    let mut pos = 0;
    let mut status_seen = false;
    let mut udp_enabled = false;
    while pos < bytes.len() {
        let kind = proto::read_varint(bytes, &mut pos)?;
        let len = proto::read_varint(bytes, &mut pos)?;
        let len = usize::try_from(len)
            .ok()
            .filter(|&n| n <= bytes.len() - pos)
            .ok_or_else(|| Error::Http3("truncated auth frame".into()))?;
        let payload = &bytes[pos..pos + len];
        pos += len;
        match kind {
            1 => {
                // No dynamic table was negotiated. Reject dependent blocks.
                if !payload.starts_with(&[0, 0]) {
                    return Err(Error::Http3("dynamic QPACK auth block".into()));
                }
                let headers = qpack::Decoder::new()
                    .decode(payload, MAX_AUTH_BYTES as u64)
                    .map_err(|e| Error::Http3(format!("auth response headers: {e}")))?;
                let mut status = None;
                for h in &headers {
                    if h.name() == b":status" {
                        if status.replace(h.value()).is_some() || status_seen {
                            return Err(Error::Auth("duplicate authentication status".into()));
                        }
                    } else if h.name() == b"hysteria-udp" && !status_seen {
                        let value = std::str::from_utf8(h.value()).unwrap_or("").trim();
                        udp_enabled = value.eq_ignore_ascii_case("true")
                            || value == "1"
                            || value.eq_ignore_ascii_case("yes");
                    }
                }
                if !status_seen {
                    if status != Some(b"233".as_slice()) {
                        return Err(Error::Auth("authentication status is not 233".into()));
                    }
                    status_seen = true;
                }
            }
            0 if status_seen => {} // DATA body, ignored but bounded
            0 | 3..=5 | 7 | 0xd => {
                return Err(Error::Http3("unexpected auth frame".into()));
            }
            _ => {} // Unknown extension frames must be ignored.
        }
    }
    if !status_seen {
        return Err(Error::Auth(
            "missing authentication response headers".into(),
        ));
    }
    Ok(udp_enabled)
}

#[cfg(test)]
#[path = "../test/hysteria2_auth_test.rs"]
mod tests;
