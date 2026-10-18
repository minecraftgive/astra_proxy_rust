use crate::App;
use anyhow::{Context, Result};
use astra_ipc::{Inbound, RouteAction};
use std::{sync::Arc, time::Duration};
use tokio::{io::AsyncWriteExt, net::TcpStream, time::timeout};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

pub async fn handle_http(mut client: TcpStream, app: Arc<App>) -> Result<()> {
    {
        // Peek preserves bytes for either HTTP parser. Fragmented request lines
        // share the same bounded handshake deadline as CONNECT.
        let connect = timeout(HANDSHAKE_TIMEOUT, async {
            let mut prefix = [0u8; 8];
            loop {
                let size = client.peek(&mut prefix).await?;
                if size == 0 {
                    return Err(std::io::Error::from(std::io::ErrorKind::UnexpectedEof));
                }
                if size >= prefix.len() {
                    return Ok(prefix == *b"CONNECT ");
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .context("HTTP handshake timeout")??;
        if !connect {
            return crate::http_forward::handle(client, app).await;
        }
    }
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

use astra_protocols::http::read_connect;
