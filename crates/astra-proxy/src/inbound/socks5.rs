use crate::App;
use anyhow::{Context, Result};
use astra_ipc::{Inbound, RouteAction};
use std::{sync::Arc, time::Duration};
use tokio::{net::TcpStream, time::timeout};

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

use astra_protocols::socks5::{read_socks_request, reply_socks5};
