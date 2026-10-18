use crate::outbound::Stream;
use anyhow::{bail, Context, Result};
use astra_ipc::Target;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
};
pub async fn http_connect(
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

use crate::accept::secret;
use anyhow::ensure;
pub async fn accept<S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin>(
    s: &mut S,
    users: &[crate::accept::User],
) -> Result<Target> {
    let mut header = vec![];
    while !header.ends_with(b"\r\n\r\n") {
        ensure!(header.len() < 8192, "HTTP headers exceed limit");
        header.push(s.read_u8().await?);
    }
    let text = std::str::from_utf8(&header)?;
    let mut lines = text.split("\r\n");
    let mut request = lines.next().context("HTTP request")?.split_whitespace();
    ensure!(
        request.next() == Some("CONNECT"),
        "HTTP server supports CONNECT"
    );
    let authority = request.next().context("CONNECT destination")?;
    use base64::Engine;
    let mut valid = false;
    let mut seen = false;
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("proxy-authorization") {
                ensure!(!seen, "duplicate proxy credentials");
                seen = true;
                let mut parts = value.split_whitespace();
                if parts
                    .next()
                    .is_some_and(|v| v.eq_ignore_ascii_case("basic"))
                {
                    if let Some(value) = parts.next() {
                        if let Ok(data) = base64::engine::general_purpose::STANDARD.decode(value) {
                            valid = users.iter().any(|u| {
                                secret(
                                    format!("{}:{}", u.name, u.password.as_ref().unwrap())
                                        .as_bytes(),
                                    &data,
                                )
                            });
                        }
                    }
                }
            }
        }
    }
    if !valid {
        s.write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"Astra\"\r\nContent-Length: 0\r\n\r\n").await?;
        bail!("authentication failed")
    }
    authority_target(authority)
}
pub fn authority_target(value: &str) -> Result<Target> {
    let (host, port) = value
        .rsplit_once(':')
        .context("destination must include port")?;
    let host = host
        .trim_start_matches('[')
        .trim_end_matches(']')
        .to_owned();
    let port = port.parse()?;
    ensure!(
        !host.is_empty() && host.len() <= 253 && port > 0,
        "invalid destination"
    );
    Ok(Target { host, port })
}

pub async fn read_connect(client: &mut TcpStream) -> Result<(Target, Vec<u8>)> {
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
