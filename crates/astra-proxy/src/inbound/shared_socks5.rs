use crate::App;
use anyhow::{Context, Result};
use astra_ipc::{Inbound, RouteAction};
use std::{sync::Arc, time::Duration};
use tokio::{net::TcpStream, time::timeout};

const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

pub async fn handle_socks5(mut client: TcpStream, app: Arc<App>) -> Result<()> {
    let users = app.authentication(client.peer_addr().ok().map(|s| s.ip()));
    let (command, target) = timeout(
        HANDSHAKE_TIMEOUT,
        read_socks_request_with_auth(&mut client, users),
    )
    .await
    .context("SOCKS5 handshake timeout")??;
    if command == 3 {
        return crate::udp::associate(client, target, app).await;
    }
    let action = match app
        .route_connection(
            &target,
            Inbound::Socks5,
            astra_ipc::Network::Tcp,
            client.peer_addr().ok(),
            client.local_addr().ok(),
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
        .connect_tracked(&target, &action, Inbound::Socks5, client.peer_addr().ok())
        .await
    {
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

use astra_protocols::socks5::{read_socks_request_with_auth, reply_socks5};
