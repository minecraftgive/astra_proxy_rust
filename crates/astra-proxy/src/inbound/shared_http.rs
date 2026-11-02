use crate::App;
use anyhow::{Context, Result};
use astra_ipc::{Inbound, RouteAction};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncWrite, AsyncWriteExt},
    net::TcpStream,
    time::timeout,
};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

pub async fn handle_http(client: TcpStream, app: Arc<App>) -> Result<()> {
    let source = client.peer_addr()?;
    let local = client.local_addr()?;
    super::handle_stream(client, app, Inbound::HttpConnect, source, local).await
}

pub(super) async fn handle_stream<S>(
    mut client: super::BufferedStream<S>,
    app: Arc<App>,
    source: SocketAddr,
    local: SocketAddr,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    {
        // Lookahead is replayed to either parser, even on non-socket streams.
        timeout(HANDSHAKE_TIMEOUT, client.prefetch(8))
            .await
            .context("HTTP handshake timeout")??;
        if client.buffer[..8] != *b"CONNECT " {
            return crate::http_forward::handle_stream(client, app, source, local).await;
        }
    }
    let users = app.authentication(Some(source.ip()));
    if !super::authorize_connect(&mut client, users).await? {
        return Ok(());
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
    let action = match app
        .route_connection(
            &target,
            Inbound::HttpConnect,
            astra_ipc::Network::Tcp,
            Some(source),
            Some(local),
        )
        .await
    {
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
    let mut upstream = match app
        .connect_tracked(&target, &action, Inbound::HttpConnect, Some(source))
        .await
    {
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
