use crate::App;
use anyhow::{bail, Context, Result};
use astra_ipc::{Inbound, RouteAction, Target};
use std::{
    net::{Ipv4Addr, Ipv6Addr},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::timeout,
};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

pub async fn handle_socks5(mut client: TcpStream, app: Arc<App>) -> Result<()> {
    let (command, target) = timeout(HANDSHAKE_TIMEOUT, read_socks_request(&mut client))
        .await
        .context("SOCKS5 handshake timeout")??;
    if command == 3 {
        return crate::udp::associate(client, target, app).await;
    }
    let action = match app.route(&target, Inbound::Socks5).await {
        Ok(route) => route,
        Err(err) => {
            reply_socks5(&mut client, 1).await?;
            return Err(err);
        }
    };
    if action == RouteAction::Block {
        reply_socks5(&mut client, 2).await?;
        return Ok(());
    }
    let mut upstream = match app.connect(&target, &action).await {
        Ok(s) => s,
        Err(err) => {
            reply_socks5(&mut client, 5).await?;
            return Err(err);
        }
    };
    reply_socks5(&mut client, 0).await?;
    let (up, down) = tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
    tracing::debug!(host = %target.host, port = target.port, up, down, "SOCKS5 stream complete");
    Ok(())
}

async fn read_socks_request(client: &mut TcpStream) -> Result<(u8, Target)> {
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

async fn reply_socks5(client: &mut TcpStream, reply: u8) -> Result<()> {
    client
        .write_all(&[5, reply, 0, 1, 0, 0, 0, 0, 0, 0])
        .await?;
    Ok(())
}

pub async fn handle_http(mut client: TcpStream, app: Arc<App>) -> Result<()> {
    let (target, tail) = match timeout(HANDSHAKE_TIMEOUT, read_connect(&mut client)).await {
        Ok(Ok(data)) => data,
        result => {
            let _ = client
                .write_all(b"HTTP/1.1 400 Bad Request\r\nConnection: close\r\n\r\n")
                .await;
            match result {
                Ok(Err(err)) => return Err(err),
                Err(err) => return Err(err.into()),
                _ => unreachable!(),
            }
        }
    };
    let action = match app.route(&target, Inbound::HttpConnect).await {
        Ok(route) => route,
        Err(err) => {
            client
                .write_all(b"HTTP/1.1 503 Service Unavailable\r\nConnection: close\r\n\r\n")
                .await?;
            return Err(err);
        }
    };
    if action == RouteAction::Block {
        client
            .write_all(b"HTTP/1.1 403 Forbidden\r\nConnection: close\r\n\r\n")
            .await?;
        return Ok(());
    }
    let mut upstream = match app.connect(&target, &action).await {
        Ok(s) => s,
        Err(err) => {
            client
                .write_all(b"HTTP/1.1 502 Bad Gateway\r\nConnection: close\r\n\r\n")
                .await?;
            return Err(err);
        }
    };
    client
        .write_all(b"HTTP/1.1 200 Connection Established\r\n\r\n")
        .await?;
    if !tail.is_empty() {
        upstream.write_all(&tail).await?;
    }
    let (up, down) = tokio::io::copy_bidirectional(&mut client, &mut upstream).await?;
    tracing::debug!(host = %target.host, port = target.port, up, down, "HTTP CONNECT stream complete");
    Ok(())
}

async fn read_connect(client: &mut TcpStream) -> Result<(Target, Vec<u8>)> {
    let mut data = Vec::with_capacity(1024);
    let end = loop {
        if data.len() >= 8192 {
            bail!("HTTP CONNECT headers too large");
        }
        let mut part = [0u8; 2048];
        let chunk_size = (8192 - data.len()).min(part.len());
        let read = client.read(&mut part[..chunk_size]).await?;
        if read == 0 {
            bail!("EOF in HTTP CONNECT headers");
        }
        data.extend_from_slice(&part[..read]);
        if let Some(i) = data.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
    };
    let line_end = data[..end]
        .windows(2)
        .position(|w| w == b"\r\n")
        .context("invalid HTTP CONNECT request line")?;
    let first_line = std::str::from_utf8(&data[..line_end])?;
    let mut parts = first_line.split_ascii_whitespace();
    if parts.next() != Some("CONNECT") {
        bail!("only HTTP CONNECT is supported");
    }
    let authority = parts.next().context("missing CONNECT authority")?;
    let fields: Vec<&str> = first_line.split_ascii_whitespace().collect();
    if fields.len() != 3 || !(fields[2] == "HTTP/1.0" || fields[2] == "HTTP/1.1") {
        bail!("invalid CONNECT HTTP version");
    }
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let (host, port) = rest
            .split_once("]:")
            .context("invalid IPv6 CONNECT authority")?;
        (host.to_owned(), port.parse::<u16>()?)
    } else {
        let (host, port) = authority
            .rsplit_once(':')
            .context("CONNECT requires an explicit port")?;
        (host.to_owned(), port.parse::<u16>()?)
    };
    if host.is_empty() || host.len() > 253 || port == 0 {
        bail!("invalid CONNECT target");
    }
    Ok((Target { host, port }, data[end..].to_vec()))
}
