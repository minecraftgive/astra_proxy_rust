use crate::{runtime::Current, App};
use anyhow::{ensure, Context, Result};
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    net::{TcpListener, UdpSocket},
    task::{JoinHandle, JoinSet},
};

pub(crate) struct Prepared {
    sockets: Vec<(Arc<UdpSocket>, Arc<TcpListener>)>,
}
pub(crate) struct Guard {
    sockets: Vec<(Arc<UdpSocket>, Arc<TcpListener>)>,
    pub addresses: Vec<SocketAddr>,
    tasks: Vec<JoinHandle<()>>,
}
impl Drop for Guard {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}
impl Prepared {
    pub async fn bind(addresses: &[SocketAddr], previous: Option<&Guard>) -> Result<Self> {
        let mut sockets = vec![];
        for address in addresses {
            if let Some(pair) = previous.and_then(|guard| {
                guard
                    .sockets
                    .iter()
                    .find(|(_, tcp)| tcp.local_addr().ok().as_ref() == Some(address))
            }) {
                sockets.push(pair.clone());
                continue;
            }
            sockets.push((
                Arc::new(
                    UdpSocket::bind(address)
                        .await
                        .with_context(|| format!("bind DNS UDP {address}"))?,
                ),
                Arc::new(
                    TcpListener::bind(address)
                        .await
                        .with_context(|| format!("bind DNS TCP {address}"))?,
                ),
            ));
        }
        Ok(Self { sockets })
    }
    pub fn start(self, current: Current) -> Guard {
        let mut guard = Guard {
            sockets: self.sockets.clone(),
            addresses: vec![],
            tasks: vec![],
        };
        for (udp, tcp) in self.sockets {
            guard.addresses.push(tcp.local_addr().unwrap());
            let active = current.clone();
            guard.tasks.push(tokio::spawn(async move {
                let mut jobs=JoinSet::new();let mut bytes=vec![0;65535];
                loop {tokio::select! {
                    Some(_)=jobs.join_next(),if !jobs.is_empty()=>{},
                    packet=udp.recv_from(&mut bytes)=>{
                        let Ok((size,peer))=packet else{break;};
                        let app=active.load_full();
                        // DNS listeners follow the same LAN ACL as proxy inbounds;
                        // a non-loopback bind must never become an open resolver.
                        if !app.inbound_allowed(peer.ip()){continue;}
                        let limit=app.dns.as_ref().map(|dns|dns.config.max_concurrent).unwrap_or(0);
                        if jobs.len()>=limit{continue;}
                        let request=bytes[..size].to_vec();let udp=udp.clone();
                        jobs.spawn(async move {
                            let response=reply(&app,&request).await;
                            if let Some(response)=response.and_then(|r|astra_dns::wire::udp_reply(&request,&r).ok()){let _=udp.send_to(&response,peer).await;}
                        });
                    }
                }}
            }));
            let active = current.clone();
            guard.tasks.push(tokio::spawn(async move {
                let mut jobs=JoinSet::new();
                loop {tokio::select! {
                    Some(_)=jobs.join_next(),if !jobs.is_empty()=>{},
                    accepted=tcp.accept()=>{
                        let Ok((stream,peer))=accepted else{break;};let app=active.load_full();
                        if !app.inbound_allowed(peer.ip()){continue;}
                        let _=stream.set_nodelay(true);
                        let limit=app.dns.as_ref().map(|dns|dns.config.max_concurrent).unwrap_or(0);
                        if jobs.len()>=limit{continue;}
                        jobs.spawn(async move {let _=serve_tcp(stream,app).await;});
                    }
                }}
            }));
        }
        guard
    }
}
async fn reply(app: &App, raw: &[u8]) -> Option<Vec<u8>> {
    match app.dns_answer(raw).await {
        Ok(reply) => Some(reply),
        Err(error) => {
            tracing::debug!(%error,"DNS query failed");
            astra_dns::wire::error(raw)
        }
    }
}
pub(crate) async fn serve_tcp<S: AsyncRead + AsyncWrite + Unpin>(
    mut stream: S,
    app: Arc<App>,
) -> Result<()> {
    for _ in 0..256 {
        let size =
            tokio::time::timeout(Duration::from_secs(10), stream.read_u16()).await?? as usize;
        ensure!(size >= 12, "invalid DNS TCP request length");
        let mut query = vec![0; size];
        tokio::time::timeout(Duration::from_secs(5), stream.read_exact(&mut query)).await??;
        let response = reply(&app, &query)
            .await
            .ok_or_else(|| anyhow::anyhow!("malformed DNS query"))?;
        tokio::time::timeout(Duration::from_secs(5), async {
            stream.write_u16(response.len() as u16).await?;
            stream.write_all(&response).await?;
            stream.flush().await
        })
        .await??;
    }
    Ok(())
}
pub(crate) use serve_tcp as tcp;
