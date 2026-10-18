use astra_protocols::datagram::{decode, encode, Fragments};
// Native QUIC inbounds; authentication is completed before dispatching requests.
use crate::{
    config::Protocol,
    net::{App, Hub},
    protocol::Inbound,
};
use anyhow::{bail, Context, Result};
use std::{
    collections::HashMap,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::io::AsyncWriteExt;

pub fn bind(inbound: &Inbound) -> Result<quinn::Endpoint> {
    let tls = (**inbound.tls.as_ref().context("QUIC TLS missing")?).clone();
    let crypto = quinn::crypto::rustls::QuicServerConfig::try_from(tls)?;
    let mut server = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    let mut transport = quinn::TransportConfig::default();
    transport
        .max_concurrent_bidi_streams(128u32.into())
        .max_concurrent_uni_streams(128u32.into())
        .stream_receive_window((1024u32 * 1024).into())
        .receive_window((4u32 * 1024 * 1024).into())
        .datagram_receive_buffer_size(Some(1024 * 1024))
        .datagram_send_buffer_size(1024 * 1024)
        .max_idle_timeout(Some(Duration::from_secs(120).try_into()?));
    if inbound.options.obfs_password.is_some() {
        transport.mtu_discovery_config(None);
    }
    server.transport_config(Arc::new(transport));
    if let Some(password) = &inbound.options.obfs_password {
        let runtime: Arc<dyn quinn::Runtime> = Arc::new(quinn::TokioRuntime);
        let socket = std::net::UdpSocket::bind(inbound.options.listen)?;
        socket.set_nonblocking(true)?;
        let socket = crate::salamander::Socket::new(runtime.wrap_udp_socket(socket)?, password);
        Ok(quinn::Endpoint::new_with_abstract_socket(
            quinn::EndpointConfig::default(),
            Some(server),
            socket,
            runtime,
        )?)
    } else {
        Ok(quinn::Endpoint::server(server, inbound.options.listen)?)
    }
}
pub async fn serve(incoming: quinn::Incoming, inbound: Arc<Inbound>, app: Arc<App>) -> Result<()> {
    let _session = app.session()?;
    let timeout = Duration::from_secs(app.config.handshake_timeout_secs);
    let conn = tokio::time::timeout(timeout, incoming).await??;
    let mut control = None;
    let authenticated = tokio::time::timeout(timeout, async {
        match inbound.options.protocol {
            Protocol::Tuic => {
                astra_protocols::tuic::authenticate(&conn, &inbound.options.users).await?
            }
            Protocol::Hysteria2 => {
                control = Some(
                    astra_protocols::hysteria2::authenticate(
                        &conn,
                        &inbound.options.users,
                        inbound.options.udp,
                    )
                    .await?,
                )
            }
            _ => bail!("non-QUIC protocol on QUIC listener"),
        }
        Ok::<(), anyhow::Error>(())
    })
    .await;
    if !matches!(authenticated, Ok(Ok(()))) {
        conn.close(1u32.into(), b"authentication failed");
        bail!("QUIC authentication failed")
    }
    let mut tasks = tokio::task::JoinSet::new();
    let mut hub = Hub::new(app.clone());
    let mut fragments = Fragments::new(app.bytes.clone());
    let mut native = HashMap::<u32, (bool, Instant)>::new();
    let mut counter = 0u16;
    let (uni_tx, mut uni_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(32);
    let mut cleanup = tokio::time::interval(Duration::from_secs(1));
    loop {
        tokio::select! {
            streams=conn.accept_bi()=>{let Ok((send,recv))=streams else{break};let app=app.clone();let inbound=inbound.clone();if tasks.len()>=128{drop(send);drop(recv);continue}tasks.spawn(async move{let _=tcp(send,recv,inbound,app).await;});},
            stream=conn.accept_uni()=>{let Ok(mut stream)=stream else{break};if tasks.len()>=128{continue}let tx=uni_tx.clone();let tuic=inbound.options.protocol==Protocol::Tuic;tasks.spawn(async move{if tuic{if let Ok(Ok(bytes))=tokio::time::timeout(Duration::from_secs(10),stream.read_to_end(66048)).await{let _=tx.try_send(bytes);}}else{let mut buf=[0;4096];while let Ok(Some(_))=stream.read(&mut buf).await{}}});},
            datagram=conn.read_datagram()=>{let Ok(data)=datagram else{break};if !inbound.options.udp{continue}if let Ok(Some(p))=decode(&data,inbound.options.protocol,&mut fragments).await{if native.len()<app.config.max_udp_flows||native.contains_key(&p.0){native.retain(|_,v|v.1.elapsed()<Duration::from_secs(app.config.idle_timeout_secs));native.insert(p.0,(true,Instant::now()));if let Ok(packet)=app.packet(p.0,p.1,p.2){let _=hub.send.try_send(packet);}}}},
            Some(data)=uni_rx.recv()=>{if !inbound.options.udp{continue}if data.starts_with(&[5,3])&&data.len()==4{native.remove(&u32::from(u16::from_be_bytes([data[2],data[3]])));continue}if let Ok(Some(p))=decode(&data,Protocol::Tuic,&mut fragments).await{if native.len()<app.config.max_udp_flows||native.contains_key(&p.0){native.retain(|_,v|v.1.elapsed()<Duration::from_secs(app.config.idle_timeout_secs));native.insert(p.0,(false,Instant::now()));if let Ok(packet)=app.packet(p.0,p.1,p.2){let _=hub.send.try_send(packet);}}}},
            Some(p)=hub.receive.recv()=>{counter=counter.wrapping_add(1);let mode=native.get(&p.association).map(|v|v.0).unwrap_or(true);let max=if mode{conn.max_datagram_size().unwrap_or(1200)}else{65535};if let Ok(parts)=encode(p.association,counter,&p.target,&p.payload,inbound.options.protocol,max){for part in parts{if mode{let _=conn.send_datagram(part.into());}else{let c=conn.clone();if tasks.len()<128{tasks.spawn(async move{if let Ok(mut s)=c.open_uni().await{let _=s.write_all(&part).await;let _=s.finish();}});}}}}},
            Some(_)=tasks.join_next(),if !tasks.is_empty()=>{},
            _=cleanup.tick()=>{
                fragments.expire();
                native.retain(|_,entry|entry.1.elapsed()<Duration::from_secs(app.config.idle_timeout_secs));
            },
        }
    }
    drop(control);
    conn.close(0u32.into(), b"session ended");
    Ok(())
}
async fn tcp(
    send: quinn::SendStream,
    mut recv: quinn::RecvStream,
    inbound: Arc<Inbound>,
    app: Arc<App>,
) -> Result<()> {
    let _session = app.session()?;
    let target = tokio::time::timeout(
        Duration::from_secs(app.config.handshake_timeout_secs),
        async {
            match inbound.options.protocol {
                Protocol::Tuic => astra_protocols::tuic::accept_tcp(&mut recv).await,
                Protocol::Hysteria2 => astra_protocols::hysteria2::accept_tcp(&mut recv).await,
                _ => bail!("non-QUIC protocol"),
            }
        },
    )
    .await??;
    let mut stream = QuicStream { send, recv };
    match app.connect(&target).await {
        Ok(remote) => {
            if inbound.options.protocol == Protocol::Hysteria2 {
                stream.write_all(&[0, 0, 0]).await?;
            }
            crate::net::relay(Box::new(stream), remote, app).await
        }
        Err(error) => {
            if inbound.options.protocol == Protocol::Hysteria2 {
                let _ = stream.write_all(&[1, 0, 0]).await;
            }
            Err(error)
        }
    }
}
pub struct QuicStream {
    send: quinn::SendStream,
    recv: quinn::RecvStream,
}
impl tokio::io::AsyncRead for QuicStream {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        b: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        tokio::io::AsyncRead::poll_read(std::pin::Pin::new(&mut self.recv), cx, b)
    }
}
impl tokio::io::AsyncWrite for QuicStream {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        b: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        tokio::io::AsyncWrite::poll_write(std::pin::Pin::new(&mut self.send), cx, b)
    }
    fn poll_flush(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        tokio::io::AsyncWrite::poll_flush(std::pin::Pin::new(&mut self.send), cx)
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        tokio::io::AsyncWrite::poll_shutdown(std::pin::Pin::new(&mut self.send), cx)
    }
}
