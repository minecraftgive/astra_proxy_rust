//! Authenticated server-side sing-mux and Mux.Cool/XUDP dispatch.
use crate::{
    net::{self, App, Hub, Stream},
    wire,
};
use anyhow::{ensure, Context, Result};
use astra_ipc::Target;
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::mpsc,
    task::JoinSet,
};
use tokio_util::compat::{FuturesAsyncReadCompatExt, TokioAsyncReadCompatExt};
struct Tasks(Vec<tokio::task::AbortHandle>);
impl Drop for Tasks {
    fn drop(&mut self) {
        for h in &self.0 {
            h.abort();
        }
    }
}

pub async fn sing(mut stream: Stream, app: Arc<App>, udp: bool) -> Result<()> {
    let request = tokio::time::timeout(
        Duration::from_secs(app.config.handshake_timeout_secs),
        async {
            let version = stream.read_u8().await?;
            let protocol = stream.read_u8().await?;
            ensure!(version <= 1 && protocol <= 2, "sing-mux request");
            let mut padding = false;
            if version == 1 {
                let flag = stream.read_u8().await?;
                ensure!(flag <= 1, "mux padding flag");
                padding = flag == 1;
                if padding {
                    let n = stream.read_u16().await? as usize;
                    ensure!(n <= 1024, "mux padding limit");
                    let mut discard = vec![0; n];
                    stream.read_exact(&mut discard).await?;
                }
            }
            Ok::<_, anyhow::Error>((protocol, padding))
        },
    )
    .await??;
    if request.1 {
        stream = Box::new(meow_proxy::mux::padding::PaddingConn::new(stream));
    }
    match request.0 {
        0 => smux(stream, app, udp).await,
        1 => yamux(stream, app, udp).await,
        2 => h2mux(stream, app, udp).await,
        _ => unreachable!(),
    }
}
async fn logical(mut stream: Stream, app: Arc<App>, udp: bool) -> Result<()> {
    let _session = app.session()?;
    let (flags, target) = tokio::time::timeout(
        Duration::from_secs(app.config.handshake_timeout_secs),
        async {
            let flags = stream.read_u16().await?;
            ensure!(flags <= 1, "unsupported mux stream flags");
            Ok::<_, anyhow::Error>((flags, wire::address(&mut stream, 0, false).await?))
        },
    )
    .await??;
    if flags == 1 {
        ensure!(udp, "UDP disabled");
        stream.write_u8(0).await?;
        stream.flush().await?;
        crate::protocol::framed_udp(stream, target, app).await
    } else {
        match app.connect(&target).await {
            Ok(remote) => {
                stream.write_u8(0).await?;
                stream.flush().await?;
                net::relay(stream, remote, app).await
            }
            Err(error) => {
                let _ = stream.write_all(&[1, 0]).await;
                Err(error)
            }
        }
    }
}
async fn yamux(stream: Stream, app: Arc<App>, udp: bool) -> Result<()> {
    let mut config = yamux::Config::default();
    config.set_max_num_streams(64);
    let mut conn = yamux::Connection::new(stream.compat(), config, yamux::Mode::Server);
    let mut children = JoinSet::new();
    while let Some(result) = futures_util::future::poll_fn(|cx| conn.poll_next_inbound(cx)).await {
        let stream = result?;
        while children.try_join_next().is_some() {}
        if children.len() >= 64 {
            drop(stream);
            continue;
        }
        let app = app.clone();
        children.spawn(async move {
            let _ = logical(Box::new(stream.compat()), app, udp).await;
        });
    }
    Ok(())
}
async fn h2mux(stream: Stream, app: Arc<App>, udp: bool) -> Result<()> {
    let mut builder = h2::server::Builder::new();
    builder
        .max_concurrent_streams(64)
        .initial_window_size(256 * 1024)
        .initial_connection_window_size(1024 * 1024)
        .max_header_list_size(8192);
    let mut conn = builder.handshake::<_, bytes::Bytes>(stream).await?;
    let mut children = JoinSet::new();
    while let Some(result) = conn.accept().await {
        let (request, mut response) = result?;
        while children.try_join_next().is_some() {}
        if request.method() != http::Method::CONNECT || children.len() >= 64 {
            let _ = response.send_response(http::Response::builder().status(429).body(())?, true);
            continue;
        }
        let send =
            response.send_response(http::Response::builder().status(200).body(())?, false)?;
        let stream = h2_stream(request.into_body(), send);
        let app = app.clone();
        children.spawn(async move {
            let _ = logical(stream, app, udp).await;
        });
    }
    Ok(())
}
pub fn h2_stream(recv: h2::RecvStream, send: h2::SendStream<bytes::Bytes>) -> Stream {
    Box::new(ServerH2 {
        recv,
        send,
        pending: bytes::Bytes::new(),
        eof: false,
        closed: false,
    })
}
struct ServerH2 {
    recv: h2::RecvStream,
    send: h2::SendStream<bytes::Bytes>,
    pending: bytes::Bytes,
    eof: bool,
    closed: bool,
}
impl tokio::io::AsyncRead for ServerH2 {
    fn poll_read(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        out: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        use bytes::Buf;
        if out.remaining() == 0 {
            return std::task::Poll::Ready(Ok(()));
        }
        while self.pending.is_empty() && !self.eof {
            match std::task::ready!(self.recv.poll_data(cx)) {
                Some(Ok(data)) => {
                    let _ = self.recv.flow_control().release_capacity(data.len());
                    self.pending = data;
                }
                Some(Err(error)) => {
                    return std::task::Poll::Ready(Err(std::io::Error::other(error)))
                }
                None => self.eof = true,
            }
        }
        let n = out.remaining().min(self.pending.len());
        out.put_slice(&self.pending[..n]);
        self.pending.advance(n);
        std::task::Poll::Ready(Ok(()))
    }
}
impl tokio::io::AsyncWrite for ServerH2 {
    fn poll_write(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        data: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        if self.closed {
            return std::task::Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into()));
        }
        if data.is_empty() {
            return std::task::Poll::Ready(Ok(0));
        }
        self.send.reserve_capacity(data.len().min(16384));
        match std::task::ready!(self.send.poll_capacity(cx)) {
            Some(Ok(n)) if n > 0 => {
                let n = n.min(data.len());
                self.send
                    .send_data(bytes::Bytes::copy_from_slice(&data[..n]), false)
                    .map_err(std::io::Error::other)?;
                std::task::Poll::Ready(Ok(n))
            }
            Some(Ok(_)) => {
                cx.waker().wake_by_ref();
                std::task::Poll::Pending
            }
            Some(Err(e)) => std::task::Poll::Ready(Err(std::io::Error::other(e))),
            None => std::task::Poll::Ready(Err(std::io::ErrorKind::BrokenPipe.into())),
        }
    }
    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
    fn poll_shutdown(
        mut self: std::pin::Pin<&mut Self>,
        _: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        if !self.closed {
            self.send
                .send_data(bytes::Bytes::new(), true)
                .map_err(std::io::Error::other)?;
            self.closed = true;
        }
        std::task::Poll::Ready(Ok(()))
    }
}
pub async fn h2_write(
    send: &mut h2::SendStream<bytes::Bytes>,
    mut bytes: bytes::Bytes,
) -> Result<()> {
    while !bytes.is_empty() {
        send.reserve_capacity(bytes.len());
        let n = futures_util::future::poll_fn(|cx| send.poll_capacity(cx))
            .await
            .context("h2 stream closed")??;
        if n == 0 {
            continue;
        }
        let count = n.min(bytes.len());
        send.send_data(bytes.split_to(count), false)?;
    }
    Ok(())
}
fn smux_frame(command: u8, id: u32, data: &[u8]) -> Vec<u8> {
    let mut out = vec![1, command];
    out.extend((data.len() as u16).to_le_bytes());
    out.extend(id.to_le_bytes());
    out.extend(data);
    out
}
async fn smux(stream: Stream, app: Arc<App>, udp: bool) -> Result<()> {
    let (mut rd, mut wr) = tokio::io::split(stream);
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);
    let writer = tokio::spawn(async move {
        while let Some(data) = rx.recv().await {
            if wr.write_all(&data).await.is_err() || wr.flush().await.is_err() {
                break;
            }
        }
    });
    let _writer = Tasks(vec![writer.abort_handle()]);
    let mut flows = HashMap::<u32, mpsc::Sender<Vec<u8>>>::new();
    let mut children = JoinSet::new();
    loop {
        let mut header = [0; 8];
        match tokio::time::timeout(
            Duration::from_secs(app.config.idle_timeout_secs),
            rd.read_exact(&mut header),
        )
        .await
        {
            Ok(Ok(_)) => {}
            _ => return Ok(()),
        };
        ensure!(header[0] == 1, "smux version");
        let cmd = header[1];
        let n = u16::from_le_bytes([header[2], header[3]]) as usize;
        let id = u32::from_le_bytes(header[4..].try_into().unwrap());
        ensure!(n <= 32768 && cmd <= 3, "smux frame");
        let mut data = vec![0; n];
        rd.read_exact(&mut data).await?;
        flows.retain(|_, tx| !tx.is_closed());
        while children.try_join_next().is_some() {}
        match cmd {
            0 => {
                ensure!(
                    n == 0 && id & 1 == 1 && !flows.contains_key(&id),
                    "smux stream ID"
                );
                if flows.len() >= 64 {
                    tx.send(smux_frame(1, id, &[])).await?;
                    continue;
                }
                let (input, receive) = mpsc::channel(4);
                flows.insert(id, input);
                let app = app.clone();
                let output = tx.clone();
                children.spawn(async move {
                    stream_flow(id, receive, output, app, udp, false, None).await;
                });
            }
            1 => {
                flows.remove(&id);
            }
            2 => {
                if let Some(input) = flows.get(&id) {
                    if !matches!(
                        tokio::time::timeout(
                            Duration::from_secs(app.config.handshake_timeout_secs),
                            input.send(data)
                        )
                        .await,
                        Ok(Ok(()))
                    ) {
                        flows.remove(&id);
                        let _ = tx.send(smux_frame(1, id, &[])).await;
                    }
                }
            }
            3 => {}
            _ => unreachable!(),
        }
    }
}
async fn stream_flow(
    id: u32,
    mut input: mpsc::Receiver<Vec<u8>>,
    output: mpsc::Sender<Vec<u8>>,
    app: Arc<App>,
    udp: bool,
    cool: bool,
    target: Option<Target>,
) {
    let (client, bridge) = tokio::io::duplex(65536);
    let (mut rd, mut wr) = tokio::io::split(bridge);
    let writeout = output.clone();
    let reader = tokio::spawn(async move {
        while let Some(data) = input.recv().await {
            if wr.write_all(&data).await.is_err() {
                break;
            }
        }
        let _ = wr.shutdown().await;
    });
    let writer = tokio::spawn(async move {
        let mut buf = [0; 8192];
        loop {
            let Ok(n) = rd.read(&mut buf).await else {
                break;
            };
            if n == 0 {
                break;
            }
            let frame = if cool {
                cool_frame(id as u16, 2, Some(&buf[..n]), None).unwrap()
            } else {
                smux_frame(2, id, &buf[..n])
            };
            if writeout.send(frame).await.is_err() {
                return;
            }
        }
        let frame = if cool {
            cool_frame(id as u16, 3, None, None).unwrap()
        } else {
            smux_frame(1, id, &[])
        };
        let _ = writeout.send(frame).await;
    });
    let _tasks = Tasks(vec![reader.abort_handle(), writer.abort_handle()]);
    if let Some(target) = target {
        if let Ok(_session) = app.session() {
            if let Ok(remote) = app.connect(&target).await {
                let _ = net::relay(Box::new(client), remote, app).await;
            }
        }
    } else {
        let _ = logical(Box::new(client), app, udp).await;
    }
    let _ = tokio::time::timeout(Duration::from_secs(2), writer).await;
}
fn cool_frame(
    id: u16,
    status: u8,
    payload: Option<&[u8]>,
    target: Option<&Target>,
) -> Result<Vec<u8>> {
    let mut meta = vec![];
    meta.extend(id.to_be_bytes());
    meta.extend([status, u8::from(payload.is_some())]);
    if let Some(target) = target {
        meta.push(2);
        let address = wire::encode(target, 1)?;
        meta.extend(target.port.to_be_bytes());
        meta.extend(&address[..address.len() - 2]);
    }
    let mut out = vec![];
    out.extend((meta.len() as u16).to_be_bytes());
    out.extend(meta);
    if let Some(payload) = payload {
        out.extend((payload.len() as u16).to_be_bytes());
        out.extend(payload);
    }
    Ok(out)
}
pub async fn cool(stream: Stream, app: Arc<App>, udp: bool) -> Result<()> {
    let (mut rd, mut wr) = tokio::io::split(stream);
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(64);
    let writer = tokio::spawn(async move {
        while let Some(data) = rx.recv().await {
            if wr.write_all(&data).await.is_err() || wr.flush().await.is_err() {
                break;
            }
        }
    });
    let _writer = Tasks(vec![writer.abort_handle()]);
    enum Flow {
        Tcp(mpsc::Sender<Vec<u8>>),
        Udp { target: Target, association: u32 },
    }
    let mut flows = HashMap::<u16, Flow>::new();
    let mut next_association = 0u32;
    let mut children = JoinSet::new();
    let mut hub = Hub::new(app.clone());
    // Keep reads in one task so a UDP reply cannot cancel half a frame header.
    let (frames, mut receive) = mpsc::channel(8);
    let reader = tokio::spawn(async move {
        loop {
            let result = async {
                let n = rd.read_u16().await? as usize;
                ensure!((4..=512).contains(&n), "Mux.Cool metadata length");
                let mut meta = vec![0; n];
                rd.read_exact(&mut meta).await?;
                ensure!(
                    !(meta[2] == 3 && meta[3] & 1 != 0),
                    "End frame carries payload"
                );
                let payload = if meta[3] & 1 != 0 {
                    let n = rd.read_u16().await? as usize;
                    ensure!(n <= 8192, "Mux.Cool payload limit");
                    let mut payload = vec![0; n];
                    rd.read_exact(&mut payload).await?;
                    Some(payload)
                } else {
                    None
                };
                Ok::<_, anyhow::Error>((meta, payload))
            }
            .await;
            match result {
                Ok(frame) => {
                    if frames.send(frame).await.is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });
    let _reader = Tasks(vec![reader.abort_handle()]);
    loop {
        tokio::select! {
            frame=receive.recv()=>{let Some((meta,payload))=frame else{return Ok(())};flows.retain(|_,flow|match flow{Flow::Tcp(input)=>!input.is_closed(),Flow::Udp{..}=>true});let id=u16::from_be_bytes([meta[0],meta[1]]);let status=meta[2];ensure!((1..=4).contains(&status)&&meta[3]&!3==0,"Mux.Cool status/options");let destination=if status==1||(status==2&&meta.get(4)==Some(&2)){ensure!(meta.len()>5&&(meta[4]==1||meta[4]==2),"Mux.Cool network");let mut rest=&meta[5..];Some((meta[4],wire::address(&mut rest,1,true).await?))}else{None};
                if status==4{continue}if status==3{flows.remove(&id);continue}if status==1{ensure!(!flows.contains_key(&id),"Mux.Cool duplicate stream");if flows.len()>=64{let _=tx.send(cool_frame(id,3,None,None)?).await;continue}let(network,target)=destination.clone().context("Mux.Cool target missing")?;if network==2{ensure!(udp,"UDP disabled");next_association=next_association.wrapping_add(1);flows.insert(id,Flow::Udp{target,association:next_association});}else{let(input,receive)=mpsc::channel(8);flows.insert(id,Flow::Tcp(input));let(app,output)=(app.clone(),tx.clone());children.spawn(async move{stream_flow(u32::from(id),receive,output,app,udp,true,Some(target)).await;});}}
                if let Some(payload)=payload{match flows.get_mut(&id){Some(Flow::Tcp(input))=>{if !matches!(tokio::time::timeout(Duration::from_secs(app.config.handshake_timeout_secs),input.send(payload)).await,Ok(Ok(()))){flows.remove(&id);let _=tx.send(cool_frame(id,3,None,None)?).await;}},Some(Flow::Udp{target,association})=>{if let Some((2,new))=destination{*target=new;}if let Ok(packet)=app.packet(*association,target.clone(),payload){let _=hub.send.try_send(packet);}},None=>{let _=tx.send(cool_frame(id,3,None,None)?).await;}}}
            },
            Some(packet)=hub.receive.recv()=>{if let Some((&id,_))=flows.iter().find(|(_,flow)|matches!(flow,Flow::Udp{association,..}if *association==packet.association)){tx.send(cool_frame(id,2,Some(&packet.payload),Some(&packet.target))?).await?;}},
            Some(_)=children.join_next(),if !children.is_empty()=>{},
            _=tokio::time::sleep(Duration::from_secs(app.config.idle_timeout_secs))=>return Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn server_accepts_padded_smux_without_client_configuration_changes() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let target = listener.local_addr().unwrap();
        let echo = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let (mut read, mut write) = socket.split();
            let _ = tokio::io::copy(&mut read, &mut write).await;
        });
        let mut config = crate::config::Config::default();
        config.allow_private_targets = true;
        let app = App::new(
            config,
            Arc::default(),
            Arc::new(tokio::sync::Semaphore::new(8)),
            Arc::new(tokio::sync::Semaphore::new(8)),
            Arc::new(tokio::sync::Semaphore::new(1024 * 1024)),
        );
        let (mut client, server) = tokio::io::duplex(65536);
        let task = tokio::spawn(sing(Box::new(server), app, true));
        client
            .write_all(&meow_proxy::mux::request::Request::new(1, 0, true).encode())
            .await
            .unwrap();
        let padded = meow_proxy::mux::padding::PaddingConn::new(client);
        let session = Arc::new(meow_proxy::mux::smux::Session::client(padded).unwrap());
        let mut stream = session.open_stream().await.unwrap();
        stream
            .write_all(
                &meow_proxy::mux::address::encode_stream_request("127.0.0.1", target.port())
                    .unwrap(),
            )
            .await
            .unwrap();
        stream.write_all(b"padded-server-flow").await.unwrap();
        stream.flush().await.unwrap();
        assert_eq!(stream.read_u8().await.unwrap(), 0);
        let mut result = [0; 18];
        stream.read_exact(&mut result).await.unwrap();
        assert_eq!(&result, b"padded-server-flow");
        stream.shutdown().await.unwrap();
        drop(stream);
        drop(session);
        task.abort();
        echo.abort();
    }
}
