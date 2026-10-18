use anyhow::{bail, ensure, Result};
use astra_ipc::Target;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use tokio::io::{AsyncRead, AsyncReadExt};

// SOCKS/Trojan: IPv4=1, domain=3, IPv6=4. VLESS: 1,2,3. TUIC: 1,0,2.
pub async fn address<R: AsyncRead + Unpin + ?Sized>(
    r: &mut R,
    style: u8,
    port_first: bool,
) -> Result<Target> {
    let port = if port_first { r.read_u16().await? } else { 0 };
    let kind = r.read_u8().await?;
    let domain = if style == 1 {
        2
    } else if style == 2 {
        0
    } else {
        3
    };
    let ipv6 = if style == 1 {
        3
    } else if style == 2 {
        2
    } else {
        4
    };
    let host = if kind == 1 {
        let mut v = [0; 4];
        r.read_exact(&mut v).await?;
        Ipv4Addr::from(v).to_string()
    } else if kind == ipv6 {
        let mut v = [0; 16];
        r.read_exact(&mut v).await?;
        Ipv6Addr::from(v).to_string()
    } else if kind == domain {
        let len = r.read_u8().await? as usize;
        ensure!(len > 0 && len <= 253, "invalid address length");
        let mut v = vec![0; len];
        r.read_exact(&mut v).await?;
        String::from_utf8(v)?
    } else {
        bail!("unsupported address type")
    };
    let port = if port_first {
        port
    } else {
        r.read_u16().await?
    };
    ensure!(port > 0, "destination port is zero");
    Ok(Target { host, port })
}
pub fn encode(target: &Target, style: u8) -> Result<Vec<u8>> {
    let mut v = vec![];
    match target.host.parse::<IpAddr>() {
        Ok(IpAddr::V4(ip)) => {
            v.push(1);
            v.extend(ip.octets());
        }
        Ok(IpAddr::V6(ip)) => {
            v.push(if style == 1 {
                3
            } else if style == 2 {
                2
            } else {
                4
            });
            v.extend(ip.octets());
        }
        Err(_) => {
            ensure!(
                !target.host.is_empty() && target.host.len() <= 253,
                "bad domain"
            );
            v.extend([
                if style == 1 {
                    2
                } else if style == 2 {
                    0
                } else {
                    3
                },
                target.host.len() as u8,
            ]);
            v.extend(target.host.as_bytes());
        }
    }
    v.extend(target.port.to_be_bytes());
    Ok(v)
}
pub async fn crlf<R: AsyncRead + Unpin + ?Sized>(r: &mut R) -> Result<()> {
    ensure!(r.read_u16().await? == 0x0d0a, "missing CRLF");
    Ok(())
}
pub async fn varint<R: AsyncRead + Unpin + ?Sized>(r: &mut R) -> Result<u64> {
    let b = r.read_u8().await?;
    let n = 1usize << (b >> 6);
    let mut value = u64::from(b & 63);
    for _ in 1..n {
        value = (value << 8) | u64::from(r.read_u8().await?);
    }
    Ok(value)
}
pub fn put_varint(value: u64, out: &mut Vec<u8>) -> Result<()> {
    ensure!(value < (1 << 62), "varint overflow");
    let n = if value < 64 {
        1
    } else if value < 16384 {
        2
    } else if value < (1 << 30) {
        4
    } else {
        8
    };
    let bytes = value.to_be_bytes();
    let at = 8 - n;
    out.extend_from_slice(&bytes[at..]);
    let first = out.len() - n;
    out[first] |= match n {
        1 => 0,
        2 => 64,
        4 => 128,
        _ => 192,
    };
    Ok(())
}
pub async fn sized<R: AsyncRead + Unpin + ?Sized>(r: &mut R, max: usize) -> Result<Vec<u8>> {
    let n = varint(r).await? as usize;
    ensure!(n <= max, "oversized length");
    let mut v = vec![0; n];
    r.read_exact(&mut v).await?;
    Ok(v)
}
