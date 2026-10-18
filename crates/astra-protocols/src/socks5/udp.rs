use anyhow::{bail, Result};
use astra_ipc::Target;
use std::net::{Ipv4Addr, Ipv6Addr};
const MAX_PACKET: usize = 65507;
pub fn decode(packet: &[u8]) -> Result<(Target, &[u8])> {
    if packet.len() > MAX_PACKET || packet.len() < 4 || packet[..3] != [0, 0, 0] {
        bail!("invalid or fragmented UDP packet");
    }
    let mut cursor = std::io::Cursor::new(&packet[4..]);
    let host = match packet[3] {
        1 => {
            let mut ip = [0; 4];
            std::io::Read::read_exact(&mut cursor, &mut ip)?;
            Ipv4Addr::from(ip).to_string()
        }
        4 => {
            let mut ip = [0; 16];
            std::io::Read::read_exact(&mut cursor, &mut ip)?;
            Ipv6Addr::from(ip).to_string()
        }
        3 => {
            let mut size = [0];
            std::io::Read::read_exact(&mut cursor, &mut size)?;
            if size[0] == 0 || size[0] > 253 {
                bail!("invalid UDP hostname length");
            }
            let mut host = vec![0; size[0] as usize];
            std::io::Read::read_exact(&mut cursor, &mut host)?;
            String::from_utf8(host)?
        }
        _ => bail!("invalid UDP address type"),
    };
    let mut port = [0; 2];
    std::io::Read::read_exact(&mut cursor, &mut port)?;
    let port = u16::from_be_bytes(port);
    if port == 0 {
        bail!("UDP destination port is zero");
    }
    Ok((
        Target { host, port },
        &packet[4 + cursor.position() as usize..],
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn udp_headers_reject_truncation_fragments_and_preserve_empty_payloads() {
        for host in ["127.0.0.1", "::1", "example.test"] {
            let expected = Target {
                host: host.into(),
                port: 53,
            };
            let mut packet = vec![0, 0, 0];
            crate::outbound::append_socks_address(&mut packet, &expected).unwrap();
            for size in 0..packet.len() {
                assert!(decode(&packet[..size]).is_err());
            }
            let (target, body) = decode(&packet).unwrap();
            assert_eq!(target, expected);
            assert!(body.is_empty());
            packet.extend_from_slice(b"payload");
            assert_eq!(decode(&packet).unwrap().1, b"payload");
            packet[2] = 1;
            assert!(decode(&packet).is_err());
            packet[2] = 0;
            packet[0] = 1;
            assert!(decode(&packet).is_err());
        }
    }
}
