use smol_str::SmolStr;

/// Parse the HTTP `Host:` header from the first request bytes.
///
/// Returns `None` if the buffer does not look like HTTP/1.x or has no Host header.
/// `httparse::Status::Partial` is treated as success — as long as we've seen
/// the Host line we don't need the full request.
///
/// Returns `SmolStr` so typical hostnames (≤ 23 bytes) are inlined without
/// a heap allocation per sniffed request.
pub fn sniff_http(buf: &[u8]) -> Option<SmolStr> {
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut req = httparse::Request::new(&mut headers);
    // Both Complete and Partial are fine; an Err means the buffer isn't HTTP.
    req.parse(buf).ok()?;
    for h in req.headers.iter() {
        if h.name.eq_ignore_ascii_case("host") {
            let s = std::str::from_utf8(h.value).ok()?.trim();
            // Strip optional `:port` suffix while preserving bracketed IPv6.
            // RFC 7230 §5.4: `Host = uri-host [":" port]` where uri-host may
            // be an `IP-literal` in `[...]`. Naive split(':') mangles `[::1]:8080`.
            let host = if let Some(rest) = s.strip_prefix('[') {
                let end = rest.find(']')?;
                &rest[..end]
            } else {
                s.split(':').next()?
            };
            if host.is_empty() {
                return None;
            }
            // Hostnames are usually already lowercase — only pay for a
            // char-mapped rebuild when one actually contains uppercase.
            return Some(if host.bytes().any(|b| b.is_ascii_uppercase()) {
                host.chars().map(|c| c.to_ascii_lowercase()).collect()
            } else {
                SmolStr::new(host)
            });
        }
    }
    None
}

#[cfg(test)]
#[path = "../test/sniffer_http_test.rs"]
mod tests;
