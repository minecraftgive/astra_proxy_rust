use anyhow::{bail, Result};
use astra_model::Target;
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
#[path = "../test/socks5_udp_test.rs"]
mod tests;
