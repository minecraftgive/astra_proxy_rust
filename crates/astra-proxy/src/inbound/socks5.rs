use crate::App;
use anyhow::{Context, Result};
use astra_model::Inbound;
use astra_model::RouteAction;
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
    time::timeout,
};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

pub async fn handle_socks5(client: TcpStream, app: Arc<App>) -> Result<()> {
    let source = client.peer_addr()?;
    let local = client.local_addr()?;
    super::handle_stream(client, app, Inbound::Socks5, source, local).await
}

pub(super) async fn handle_stream<S>(
    mut client: S,
    app: Arc<App>,
    source: SocketAddr,
    local: SocketAddr,
) -> Result<()>
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let users = app.authentication(Some(source.ip()));
    let (command, target) = timeout(
        HANDSHAKE_TIMEOUT,
        read_socks_request_with_auth(&mut client, users),
    )
    .await
    .context("SOCKS5 handshake timeout")??;
    if command == 3 {
        return crate::udp::associate_stream(client, target, app, source, local).await;
    }
    let action = match app
        .route_connection(
            &target,
            Inbound::Socks5,
            astra_model::Network::Tcp,
            Some(source),
            Some(local),
        )
        .await
    {
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
    let mut upstream = match app
        .connect_tracked(&target, &action, Inbound::Socks5, Some(source))
        .await
    {
        Ok(s) => s,
        Err(err) => {
            reply_socks5(&mut client, 5).await?;
            return Err(err);
        }
    };
    let _route_leases = app.route_leases.clone();
    drop(app);
    reply_socks5(&mut client, 0).await?;
    let (up, down) = crate::proxy::relay::relay(&mut client, &mut upstream).await?;
    tracing::debug!(host = %target.host, port = target.port, up, down, "SOCKS5 stream complete");
    Ok(())
}

use astra_protocols::socks5::{read_socks_request_with_auth, reply_socks5};
