use crate::config::OutboundConfig;
use anyhow::{bail, Context, Result};
use astra_ipc::Target;
use sha2::{Digest, Sha224};
use std::{
    net::IpAddr,
    sync::{Arc, OnceLock},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
};
use tokio_rustls::{
    rustls::{self, pki_types::ServerName, RootCertStore},
    TlsConnector,
};
use uuid::Uuid;

pub trait Duplex: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T> Duplex for T where T: AsyncRead + AsyncWrite + Unpin + Send {}
pub type Stream = Box<dyn Duplex>;

mod vless_stream;
use vless_stream::VlessStream;

pub async fn open(target: &Target, outbound: Option<&OutboundConfig>) -> Result<Stream> {
    match outbound {
        Some(OutboundConfig::Native { .. }) => bail!("native outbound was not initialized"),
        None => {
            let stream = TcpStream::connect((target.host.as_str(), target.port)).await?;
            stream.set_nodelay(true)?;
            Ok(Box::new(stream))
        }
        Some(OutboundConfig::Socks5 {
            server,
            username,
            password,
            ..
        }) => socks5(server, username.as_deref(), password.as_deref(), target).await,
        Some(OutboundConfig::Http {
            server,
            username,
            password,
            ..
        }) => http_connect(server, username.as_deref(), password.as_deref(), target).await,
        Some(OutboundConfig::TrojanTls {
            server,
            sni,
            password,
            ..
        }) => trojan_tls(server, sni, password, target).await,
        Some(OutboundConfig::VlessTls {
            server, sni, uuid, ..
        }) => vless_tls(server, sni, uuid, target).await,
    }
}

async fn tls_connect(
    server: &str,
    sni: &str,
) -> Result<tokio_rustls::client::TlsStream<TcpStream>> {
    let tcp = TcpStream::connect(server)
        .await
        .context("TLS endpoint TCP connect")?;
    tcp.set_nodelay(true)?;
    static TLS_CONFIG: OnceLock<Arc<rustls::ClientConfig>> = OnceLock::new();
    let config = TLS_CONFIG.get_or_init(|| {
        let mut roots = RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        Arc::new(
            rustls::ClientConfig::builder()
                .with_root_certificates(roots)
                .with_no_client_auth(),
        )
    });
    let connector = TlsConnector::from(config.clone());
    let name = ServerName::try_from(sni.to_owned()).context("invalid TLS SNI")?;
    connector
        .connect(name, tcp)
        .await
        .context("TLS handshake failed")
}

/// SOCKS5 addressing, also used by Trojan but NOT VLESS.
pub(crate) fn append_socks_address(out: &mut Vec<u8>, target: &Target) -> Result<()> {
    if let Ok(addr) = target.host.parse::<IpAddr>() {
        match addr {
            IpAddr::V4(ip) => {
                out.push(1);
                out.extend_from_slice(&ip.octets());
            }
            IpAddr::V6(ip) => {
                out.push(4);
                out.extend_from_slice(&ip.octets());
            }
        }
    } else {
        let bytes = target.host.as_bytes();
        if bytes.is_empty() || bytes.len() > 253 {
            bail!("invalid destination domain");
        }
        out.extend_from_slice(&[3, bytes.len() as u8]);
        out.extend_from_slice(bytes);
    }
    out.extend_from_slice(&target.port.to_be_bytes());
    Ok(())
}

async fn socks5(
    server: &str,
    username: Option<&str>,
    password: Option<&str>,
    target: &Target,
) -> Result<Stream> {
    let mut tcp = TcpStream::connect(server).await?;
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

async fn http_connect(
    server: &str,
    username: Option<&str>,
    password: Option<&str>,
    target: &Target,
) -> Result<Stream> {
    use base64::Engine;
    let mut tcp = TcpStream::connect(server).await?;
    tcp.set_nodelay(true)?;
    if target.host.bytes().any(|b| b <= 32 || b == 127) {
        bail!("invalid HTTP destination");
    }
    let authority = if target.host.contains(':') {
        format!("[{}]:{}", target.host, target.port)
    } else {
        format!("{}:{}", target.host, target.port)
    };
    let mut request = format!("CONNECT {authority} HTTP/1.1\r\nHost: {authority}\r\n");
    if let (Some(user), Some(pass)) = (username, password) {
        let encoded = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{pass}"));
        request.push_str(&format!("Proxy-Authorization: Basic {encoded}\r\n"));
    }
    request.push_str("\r\n");
    tcp.write_all(request.as_bytes()).await?;
    // BufReader keeps any server-first tunnel data read with the headers.
    use tokio::io::{AsyncBufReadExt, BufReader};
    let mut tcp = BufReader::new(tcp);
    let mut response = Vec::new();
    {
        let mut limited = (&mut tcp).take(8192);
        while !response.ends_with(b"\r\n\r\n") {
            if limited.read_until(b'\n', &mut response).await? == 0 {
                bail!("upstream HTTP response truncated or too large");
            }
        }
    }
    let line = std::str::from_utf8(&response)?
        .split("\r\n")
        .next()
        .unwrap_or("");
    let mut fields = line.split_ascii_whitespace();
    let version = fields.next().unwrap_or("");
    let status = fields.next().unwrap_or("").parse::<u16>().unwrap_or(0);
    if !matches!(version, "HTTP/1.0" | "HTTP/1.1") || !(200..300).contains(&status) {
        bail!("upstream HTTP CONNECT rejected: {line}");
    }
    Ok(Box::new(tcp))
}

async fn trojan_tls(server: &str, sni: &str, password: &str, target: &Target) -> Result<Stream> {
    let mut tls = tls_connect(server, sni).await?;
    let hash = hex::encode(Sha224::digest(password.as_bytes()));
    let mut header = Vec::with_capacity(320);
    header.extend_from_slice(hash.as_bytes());
    header.extend_from_slice(b"\r\n");
    header.push(1); // CONNECT
    append_socks_address(&mut header, target)?;
    header.extend_from_slice(b"\r\n");
    tls.write_all(&header).await?;
    // Trojan has no successful TCP CONNECT response header.
    Ok(Box::new(tls))
}

async fn vless_tls(server: &str, sni: &str, uuid: &str, target: &Target) -> Result<Stream> {
    let mut tls = tls_connect(server, sni).await?;
    let user = Uuid::parse_str(uuid).context("invalid VLESS UUID")?;
    let mut header = Vec::with_capacity(320);
    header.push(0); // protocol version
    header.extend_from_slice(user.as_bytes());
    header.push(0); // addons length: no Vision flow
    header.push(1); // TCP
    header.extend_from_slice(&target.port.to_be_bytes());
    if let Ok(addr) = target.host.parse::<IpAddr>() {
        match addr {
            IpAddr::V4(ip) => {
                header.push(1);
                header.extend_from_slice(&ip.octets());
            }
            IpAddr::V6(ip) => {
                header.push(3);
                header.extend_from_slice(&ip.octets());
            }
        }
    } else {
        let bytes = target.host.as_bytes();
        if bytes.is_empty() || bytes.len() > 253 {
            bail!("invalid destination domain");
        }
        header.extend_from_slice(&[2, bytes.len() as u8]);
        header.extend_from_slice(bytes);
    }
    tls.write_all(&header).await?;
    tls.flush().await?;
    Ok(Box::new(VlessStream::new(tls)))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn sock5_domain_is_encoded_with_length() {
        let mut out = Vec::new();
        append_socks_address(
            &mut out,
            &Target {
                host: "abc.test".into(),
                port: 443,
            },
        )
        .unwrap();
        assert_eq!(out, b"\x03\x08abc.test\x01\xbb");
    }
}
