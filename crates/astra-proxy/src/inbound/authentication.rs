use anyhow::{Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::{timeout, Duration},
};

pub(crate) fn http_authorized(headers: &[&str], users: &[String]) -> bool {
    if users.is_empty() {
        return true;
    }
    if headers.len() != 1 || headers[0].len() > 1024 {
        return false;
    }
    let Some((scheme, encoded)) = headers[0].trim().split_once(' ') else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("Basic") {
        return false;
    }
    let Ok(decoded) = STANDARD.decode(encoded.trim()) else {
        return false;
    };
    users.iter().any(|user| {
        let expected = user.as_bytes();
        let mut difference = expected.len() ^ decoded.len();
        for index in 0..expected.len().max(decoded.len()) {
            difference |= usize::from(
                expected.get(index).copied().unwrap_or(0)
                    ^ decoded.get(index).copied().unwrap_or(0),
            );
        }
        difference == 0
    })
}

/// Inspect CONNECT authentication without consuming the parser's buffered bytes.
pub(crate) async fn authorize_connect(client: &mut TcpStream, users: &[String]) -> Result<bool> {
    if users.is_empty() {
        return Ok(true);
    }
    let headers = timeout(Duration::from_secs(10), async {
        let mut buffer = vec![0u8; 32 * 1024];
        loop {
            let count = client.peek(&mut buffer).await?;
            anyhow::ensure!(count != 0, "connection closed during proxy authentication");
            if let Some(end) = buffer[..count].windows(4).position(|v| v == b"\r\n\r\n") {
                return Ok::<_, anyhow::Error>(String::from_utf8(buffer[..end].to_vec())?);
            }
            anyhow::ensure!(count < buffer.len(), "proxy request headers exceed 32 KiB");
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .context("proxy authentication timeout")??;
    let auth: Vec<_> = headers
        .split("\r\n")
        .skip(1)
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("Proxy-Authorization")
                .then_some(value.trim())
        })
        .collect();
    if http_authorized(&auth, users) {
        return Ok(true);
    }
    // Consume the inspected header before closing. Dropping a socket with unread
    // request bytes can reset the connection and discard the 407 on Windows.
    let mut consumed = vec![0u8; headers.len() + 4];
    client.read_exact(&mut consumed).await?;
    client.write_all(b"HTTP/1.1 407 Proxy Authentication Required\r\nProxy-Authenticate: Basic realm=\"Astra\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await?;
    client.shutdown().await?;
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn credentials_are_required_and_duplicate_or_invalid_headers_are_rejected() {
        let users = vec!["alice:correct:password".into()];
        let good = format!("Basic {}", STANDARD.encode("alice:correct:password"));
        assert!(http_authorized(&[&good], &users));
        assert!(!http_authorized(&[], &users));
        assert!(!http_authorized(&[&good, &good], &users));
        assert!(!http_authorized(&["Basic !!!!"], &users));
        assert!(!http_authorized(&["Basic YWxpY2U6d3Jvbmc="], &users));
    }
}
