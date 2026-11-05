//! Bounded, read-only HTTP Host / TLS ClientHello SNI inspection.
//! Callers must replay every inspected byte; this module never changes payloads.
pub const LIMIT: usize = 16 * 1024;
#[derive(Debug, PartialEq, Eq)]
pub enum Result {
    NeedMore,
    Host(String),
    Unknown,
}
pub fn hostname(raw: &[u8]) -> Option<String> {
    let host = std::str::from_utf8(raw).ok()?.trim_end_matches('.');
    if host.is_empty() || host.len() > 253 || host.parse::<std::net::IpAddr>().is_ok() {
        return None;
    }
    if !host.split('.').all(|label| {
        !label.is_empty()
            && label.len() <= 63
            && label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    }) {
        return None;
    }
    Some(host.to_ascii_lowercase())
}
pub fn inspect(bytes: &[u8]) -> Result {
    if bytes.is_empty() {
        return Result::NeedMore;
    }
    if bytes[0] == 22 {
        return tls(bytes);
    }
    let methods: [&[u8]; 9] = [
        b"GET ",
        b"POST ",
        b"HEAD ",
        b"PUT ",
        b"DELETE ",
        b"OPTIONS ",
        b"PATCH ",
        b"CONNECT ",
        b"TRACE ",
    ];
    if !methods
        .iter()
        .any(|m| bytes.starts_with(m) || m.starts_with(bytes))
    {
        return Result::Unknown;
    }
    let Some(end) = bytes.windows(4).position(|w| w == b"\r\n\r\n") else {
        return if bytes.len() < LIMIT {
            Result::NeedMore
        } else {
            Result::Unknown
        };
    };
    let Ok(header) = std::str::from_utf8(&bytes[..end]) else {
        return Result::Unknown;
    };
    let mut found = None;
    for line in header.split("\r\n").skip(1) {
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        if key.eq_ignore_ascii_case("host") {
            if found.is_some() {
                return Result::Unknown;
            }
            let value = value.trim();
            let host = if let Some((host, port)) = value.rsplit_once(':') {
                if port.parse::<u16>().is_ok() {
                    host
                } else {
                    return Result::Unknown;
                }
            } else {
                value
            };
            found = hostname(host.as_bytes());
            if found.is_none() {
                return Result::Unknown;
            }
        }
    }
    found.map(Result::Host).unwrap_or(Result::Unknown)
}
struct Cursor<'a>(&'a [u8]);
impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let out = self.0.get(..n)?;
        self.0 = &self.0[n..];
        Some(out)
    }
    fn byte(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn number(&mut self) -> Option<usize> {
        let b = self.take(2)?;
        Some(u16::from_be_bytes([b[0], b[1]]) as usize)
    }
    fn vector8(&mut self) -> Option<&'a [u8]> {
        let n = self.byte()? as usize;
        self.take(n)
    }
    fn vector16(&mut self) -> Option<&'a [u8]> {
        let n = self.number()?;
        self.take(n)
    }
}
fn hello(bytes: &[u8]) -> Option<String> {
    let mut c = Cursor(bytes);
    c.take(34)?;
    c.vector8()?;
    c.vector16()?;
    c.vector8()?;
    let mut extensions = Cursor(c.vector16()?);
    while !extensions.0.is_empty() {
        let kind = extensions.number()?;
        let value = extensions.vector16()?;
        if kind != 0 {
            continue;
        }
        let mut names = Cursor(value);
        let mut names = Cursor(names.vector16()?);
        while !names.0.is_empty() {
            let kind = names.byte()?;
            let name = names.vector16()?;
            if kind == 0 {
                return hostname(name);
            }
        }
    }
    None
}
fn tls(bytes: &[u8]) -> Result {
    let mut offset = 0;
    let mut handshake = Vec::new();
    loop {
        let Some(header) = bytes.get(offset..offset + 5) else {
            return Result::NeedMore;
        };
        if header[0] != 22 || header[1] != 3 || !(1..=4).contains(&header[2]) {
            return Result::Unknown;
        }
        let size = u16::from_be_bytes([header[3], header[4]]) as usize;
        if size == 0 || size > LIMIT || handshake.len() + size > LIMIT {
            return Result::Unknown;
        }
        let Some(record) = bytes.get(offset + 5..offset + 5 + size) else {
            return Result::NeedMore;
        };
        handshake.extend_from_slice(record);
        offset += 5 + size;
        if handshake.first() != Some(&1) {
            return Result::Unknown;
        }
        if handshake.len() >= 4 {
            let size = ((handshake[1] as usize) << 16)
                | ((handshake[2] as usize) << 8)
                | handshake[3] as usize;
            if size > LIMIT - 4 {
                return Result::Unknown;
            }
            if handshake.len() >= 4 + size {
                return hello(&handshake[4..4 + size])
                    .map(Result::Host)
                    .unwrap_or(Result::Unknown);
            }
        }
        if offset >= bytes.len() {
            return Result::NeedMore;
        }
    }
}

#[cfg(test)]
#[path = "test/sniff_test.rs"]
mod tests;
