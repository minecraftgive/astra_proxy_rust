use smol_str::SmolStr;

/// Parse SNI hostname from raw TLS ClientHello bytes.
///
/// Returns `SmolStr` so typical hostnames (≤ 23 bytes) are inlined without
/// a heap allocation per sniffed handshake.
pub fn sniff_tls(buf: &[u8]) -> Option<SmolStr> {
    // TLS record header: content_type(1) + version(2) + length(2)
    if buf.len() < 5 {
        return None;
    }
    // content_type must be Handshake (0x16)
    if buf[0] != 0x16 {
        return None;
    }

    let record_len = u16::from_be_bytes([buf[3], buf[4]]) as usize;
    let record = buf.get(5..5 + record_len)?;

    // Handshake header: type(1) + length(3)
    if record.is_empty() || record[0] != 0x01 {
        return None;
    }
    let handshake_len =
        ((record[1] as usize) << 16) | ((record[2] as usize) << 8) | (record[3] as usize);
    let hello = record.get(4..4 + handshake_len)?;

    // ClientHello: version(2) + random(32) = skip 34 bytes
    if hello.len() < 34 {
        return None;
    }
    let mut pos = 34;

    // Session ID: length(1) + data
    let session_id_len = *hello.get(pos)? as usize;
    pos += 1 + session_id_len;

    // Cipher suites: length(2) + data
    let cs_len = u16::from_be_bytes([*hello.get(pos)?, *hello.get(pos + 1)?]) as usize;
    pos += 2 + cs_len;

    // Compression methods: length(1) + data
    let comp_len = *hello.get(pos)? as usize;
    pos += 1 + comp_len;

    // Extensions: length(2) + data
    if pos + 2 > hello.len() {
        return None;
    }
    let ext_len = u16::from_be_bytes([hello[pos], hello[pos + 1]]) as usize;
    pos += 2;

    let ext_end = pos + ext_len;
    while pos + 4 <= ext_end && pos + 4 <= hello.len() {
        let ext_type = u16::from_be_bytes([hello[pos], hello[pos + 1]]);
        let ext_data_len = u16::from_be_bytes([hello[pos + 2], hello[pos + 3]]) as usize;
        pos += 4;

        if ext_type == 0x0000 {
            // SNI extension
            return parse_sni_extension(hello.get(pos..pos + ext_data_len)?);
        }
        pos += ext_data_len;
    }

    None
}

fn parse_sni_extension(data: &[u8]) -> Option<SmolStr> {
    if data.len() < 2 {
        return None;
    }
    let list_len = u16::from_be_bytes([data[0], data[1]]) as usize;
    let list = data.get(2..2 + list_len)?;

    let mut pos = 0;
    while pos + 3 <= list.len() {
        let name_type = list[pos];
        let name_len = u16::from_be_bytes([list[pos + 1], list[pos + 2]]) as usize;
        pos += 3;

        if name_type == 0x00 {
            let name_bytes = list.get(pos..pos + name_len)?;
            let s = std::str::from_utf8(name_bytes).ok()?;
            // Hostnames are usually already lowercase — only pay for a
            // char-mapped rebuild when one actually contains uppercase.
            return Some(if s.bytes().any(|b| b.is_ascii_uppercase()) {
                s.chars().map(|c| c.to_ascii_lowercase()).collect()
            } else {
                SmolStr::new(s)
            });
        }
        pos += name_len;
    }

    None
}

#[cfg(test)]
#[path = "../test/sniffer_tls_test.rs"]
mod tests;
