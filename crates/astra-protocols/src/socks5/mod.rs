use crate::outbound::{append_socks_address, Stream};
use anyhow::{bail, Result};
use astra_ipc::Target;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};
pub async fn socks5(
    server: &str,
    username: Option<&str>,
    password: Option<&str>,
    target: &Target,
) -> Result<Stream> {
    let mut tcp = crate::egress::connect(server).await?;
    tcp.set_nodelay(true)?;
    let method = if username.is_some() { 2 } else { 0 };
    tcp.write_all(&[5, 1, method]).await?;
    let mut greeting = [0; 2];
    tcp.read_exact(&mut greeting).await?;
    if greeting != [5, method] {
        bail!("upstream SOCKS5 rejected authentication method");
    }
    if let (Some(user), Some(pass)) = (username, password) {
        let mut auth = vec![1, user.len() as u8];
        auth.extend_from_slice(user.as_bytes());
        auth.push(pass.len() as u8);
        auth.extend_from_slice(pass.as_bytes());
        tcp.write_all(&auth).await?;
        tcp.read_exact(&mut greeting).await?;
        if greeting != [1, 0] {
            bail!("upstream SOCKS5 authentication failed");
        }
    }
    let mut request = vec![5, 1, 0];
    append_socks_address(&mut request, target)?;
    tcp.write_all(&request).await?;
    let mut reply = [0; 4];
    tcp.read_exact(&mut reply).await?;
    if reply[0] != 5 || reply[1] != 0 || reply[2] != 0 {
        bail!("upstream SOCKS5 CONNECT failed ({:?})", reply);
    }
    let size = match reply[3] {
        1 => 4,
        4 => 16,
        3 => {
            let len = tcp.read_u8().await?;
            len as usize
        }
        other => bail!("unexpected SOCKS5 address type {other}"),
    };
    let mut discard = vec![0; size + 2];
    tcp.read_exact(&mut discard).await?;
    Ok(Box::new(tcp))
}

use crate::{
    accept::{secret, Request as Route},
    wire,
};
use anyhow::ensure;
pub async fn accept<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
    s: &mut S,
    users: &[crate::accept::User],
) -> Result<Route> {
    ensure!(s.read_u8().await? == 5, "SOCKS version");
    let n = s.read_u8().await? as usize;
    let mut methods = vec![0; n];
    s.read_exact(&mut methods).await?;
    if !methods.contains(&2) {
        s.write_all(&[5, 255]).await?;
        bail!("SOCKS requires authentication")
    };
    s.write_all(&[5, 2]).await?;
    ensure!(s.read_u8().await? == 1, "SOCKS auth version");
    let n = s.read_u8().await? as usize;
    let mut name = vec![0; n];
    s.read_exact(&mut name).await?;
    let n = s.read_u8().await? as usize;
    let mut password = vec![0; n];
    s.read_exact(&mut password).await?;
    let valid = users.iter().any(|u| {
        secret(u.name.as_bytes(), &name)
            & secret(u.password.as_ref().unwrap().as_bytes(), &password)
    });
    s.write_all(&[1, if valid { 0 } else { 1 }]).await?;
    ensure!(valid, "authentication failed");
    ensure!(s.read_u8().await? == 5, "SOCKS version");
    let cmd = s.read_u8().await?;
    ensure!(s.read_u8().await? == 0, "SOCKS reserved");
    if cmd == 3 {
        let kind = s.read_u8().await?;
        let count = match kind {
            1 => 4,
            4 => 16,
            3 => s.read_u8().await? as usize,
            _ => bail!("SOCKS address"),
        };
        let mut v = vec![0; count + 2];
        s.read_exact(&mut v).await?;
        return Ok(Route::SocksUdp);
    }
    ensure!(cmd == 1, "SOCKS command unsupported");
    Ok(Route::Tcp(wire::address(s, 0, false).await?))
}

use anyhow::Context;
use std::net::{Ipv4Addr, Ipv6Addr};
pub async fn read_socks_request(client: &mut TcpStream) -> Result<(u8, Target)> {
    if client.read_u8().await? != 5 {
        bail!("unsupported SOCKS version");
    }
    let count = client.read_u8().await? as usize;
    if count == 0 {
        bail!("no authentication methods offered");
    }
    let mut methods = vec![0; count];
    client.read_exact(&mut methods).await?;
    if !methods.contains(&0) {
        client.write_all(&[5, 255]).await?;
        bail!("SOCKS5 requires no-auth method");
    }
    client.write_all(&[5, 0]).await?;
    let mut request = [0; 4];
    client.read_exact(&mut request).await?;
    if request[0] != 5 || request[2] != 0 {
        bail!("invalid SOCKS5 request header");
    }
    if !matches!(request[1], 1 | 3) {
        reply_socks5(client, 7).await?;
        bail!("SOCKS5 BIND is not supported");
    }
    let host = match request[3] {
        1 => {
            let mut ip = [0; 4];
            client.read_exact(&mut ip).await?;
            Ipv4Addr::from(ip).to_string()
        }
        4 => {
            let mut ip = [0; 16];
            client.read_exact(&mut ip).await?;
            Ipv6Addr::from(ip).to_string()
        }
        3 => {
            let size = client.read_u8().await? as usize;
            if size == 0 || size > 253 {
                bail!("invalid SOCKS5 hostname length");
            }
            let mut raw = vec![0; size];
            client.read_exact(&mut raw).await?;
            String::from_utf8(raw).context("SOCKS5 hostname is not UTF-8")?
        }
        other => bail!("unsupported SOCKS5 ATYP: {other}"),
    };
    let port = client.read_u16().await?;
    if port == 0 && request[1] == 1 {
        bail!("port must not be 0");
    }
    Ok((request[1], Target { host, port }))
}

pub async fn reply_socks5(client: &mut TcpStream, reply: u8) -> Result<()> {
    client
        .write_all(&[5, reply, 0, 1, 0, 0, 0, 0, 0, 0])
        .await?;
    Ok(())
}

pub mod udp;
