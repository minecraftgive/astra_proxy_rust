use crate::net::{App, Hub, Stream};
use anyhow::Result;
use astra_ipc::Target;
use astra_protocols::wire;
use std::{net::SocketAddr, sync::Arc, time::Duration};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
pub async fn socks_udp(mut stream: Stream, peer: SocketAddr, app: Arc<App>) -> Result<()> {
    let socket = tokio::net::UdpSocket::bind(SocketAddr::new(
        peer.ip()
            .is_ipv4()
            .then_some(std::net::IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED))
            .unwrap_or(std::net::IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED)),
        0,
    ))
    .await?;
    // A wildcard reply address asks the client to use the TCP server address.
    let a = socket.local_addr()?;
    let mut reply = vec![5, 0, 0];
    reply.extend(wire::encode(
        &Target {
            host: a.ip().to_string(),
            port: a.port(),
        },
        0,
    )?);
    stream.write_all(&reply).await?;
    let mut hub = Hub::new(app.clone());
    let mut endpoint = None;
    let mut buf = vec![0; 65536];
    let mut eof = [0];
    loop {
        tokio::select! {
            _=stream.read(&mut eof)=>return Ok(()),
            received=socket.recv_from(&mut buf)=>{let(n,from)=received?;if from.ip()!=peer.ip()||endpoint.is_some_and(|p|p!=from){continue}let Ok((target,data))=astra_protocols::socks5::udp::decode(&buf[..n]) else{continue};endpoint=Some(from);let Ok(packet)=app.packet(0,target,data.to_vec())else{continue};let _=hub.send.try_send(packet);},
            Some(packet)=hub.receive.recv()=>{if let Some(to)=endpoint{let mut out=vec![0,0,0];out.extend(wire::encode(&packet.target,0)?);out.extend(&packet.payload);socket.send_to(&out,to).await?;}},
            _=tokio::time::sleep(Duration::from_secs(app.config.idle_timeout_secs))=>return Ok(()),
        }
    }
}
