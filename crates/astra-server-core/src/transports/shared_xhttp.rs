//! Server XHTTP stream-one, stream-up and ordered packet-up sessions.
use crate::{
    multiplex::h2_write,
    net::{App, Stream},
    protocol::Inbound,
};
use anyhow::{ensure, Context, Result};
use std::{
    collections::{BTreeMap, HashMap},
    net::{IpAddr, SocketAddr},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, Weak,
    },
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::mpsc,
};
type Key = (IpAddr, String);
enum Upload {
    Data(Option<u64>, bytes::Bytes),
    End,
}
struct Session {
    input: mpsc::Sender<Upload>,
    download: Mutex<Option<tokio::io::ReadHalf<tokio::io::DuplexStream>>>,
    down_claimed: AtomicBool,
    up_claimed: AtomicBool,
    last: Mutex<Instant>,
    idle: Duration,
    tasks: Mutex<Vec<tokio::task::AbortHandle>>,
}
impl Drop for Session {
    fn drop(&mut self) {
        for task in self.tasks.get_mut().unwrap() {
            task.abort();
        }
    }
}
pub struct State {
    sessions: Mutex<HashMap<Key, Arc<Session>>>,
    cleanup: Mutex<Option<tokio::task::AbortHandle>>,
}
impl Drop for State {
    fn drop(&mut self) {
        if let Some(handle) = self.cleanup.get_mut().unwrap() {
            handle.abort();
        }
    }
}
impl State {
    pub fn new() -> Arc<Self> {
        let state = Arc::new(Self {
            sessions: Mutex::new(HashMap::new()),
            cleanup: Mutex::new(None),
        });
        let weak = Arc::downgrade(&state);
        let task = tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(1)).await;
                let Some(state) = weak.upgrade() else { break };
                state
                    .sessions
                    .lock()
                    .unwrap()
                    .retain(|_, s| s.last.lock().unwrap().elapsed() < s.idle);
            }
        });
        *state.cleanup.lock().unwrap() = Some(task.abort_handle());
        state
    }
    fn session(
        &self,
        key: Key,
        peer: SocketAddr,
        inbound: Arc<Inbound>,
        app: Arc<App>,
    ) -> Result<Arc<Session>> {
        let mut map = self.sessions.lock().unwrap();
        if let Some(session) = map.get(&key) {
            return Ok(session.clone());
        }
        ensure!(map.len() < 256, "XHTTP session limit");
        let permit = app.session()?;
        let (client, bridge) = tokio::io::duplex(65536);
        let (rd, mut wr) = tokio::io::split(bridge);
        let (input, mut uploads) = mpsc::channel(8);
        let session = Arc::new(Session {
            input,
            download: Mutex::new(Some(rd)),
            down_claimed: AtomicBool::new(false),
            up_claimed: AtomicBool::new(false),
            last: Mutex::new(Instant::now()),
            idle: Duration::from_secs(app.config.idle_timeout_secs),
            tasks: Mutex::new(vec![]),
        });
        let feed = tokio::spawn(async move {
            let result = async {
                let mut next = 0;
                let mut pending = BTreeMap::new();
                while let Some(message) = uploads.recv().await {
                    match message {
                        Upload::Data(None, bytes) => wr.write_all(&bytes).await?,
                        Upload::Data(Some(index), bytes) => {
                            ensure!(
                                index >= next && index < next + 8 && !pending.contains_key(&index),
                                "XHTTP packet ordering/replay"
                            );
                            pending.insert(index, bytes);
                            while let Some(bytes) = pending.remove(&next) {
                                wr.write_all(&bytes).await?;
                                next += 1;
                            }
                        }
                        Upload::End => break,
                    }
                }
                wr.shutdown().await?;
                Ok::<_, anyhow::Error>(())
            }
            .await;
            let _ = result;
        });
        let handler = tokio::spawn(async move {
            let _permit = permit;
            let _ = crate::protocol::serve_inner(Box::new(client), peer, inbound, app, None).await;
        });
        session
            .tasks
            .lock()
            .unwrap()
            .extend([feed.abort_handle(), handler.abort_handle()]);
        map.insert(key, session.clone());
        Ok(session)
    }
}
struct Lease {
    state: Weak<State>,
    key: Key,
    session: Weak<Session>,
}
impl Drop for Lease {
    fn drop(&mut self) {
        if let Some(state) = self.state.upgrade() {
            let mut map = state.sessions.lock().unwrap();
            if self.session.upgrade().is_some_and(|session| {
                map.get(&self.key)
                    .is_some_and(|active| Arc::ptr_eq(active, &session))
            }) {
                map.remove(&self.key);
            }
        }
    }
}
pub async fn serve(
    stream: Stream,
    peer: SocketAddr,
    inbound: Arc<Inbound>,
    app: Arc<App>,
) -> Result<()> {
    let mut builder = h2::server::Builder::new();
    builder
        .max_concurrent_streams(64)
        .initial_window_size(256 * 1024)
        .initial_connection_window_size(1024 * 1024)
        .max_header_list_size(8192);
    let mut conn = builder.handshake::<_, bytes::Bytes>(stream).await?;
    let mut jobs = tokio::task::JoinSet::new();
    let base = format!(
        "{}/",
        inbound
            .options
            .path
            .as_deref()
            .unwrap_or("/")
            .trim_end_matches('/')
    );
    while let Some(request) = conn.accept().await {
        let (request, mut response) = request?;
        while jobs.try_join_next().is_some() {}
        if jobs.len() >= 64 {
            let _ = response.send_response(http::Response::builder().status(429).body(())?, true);
            continue;
        }
        let path = request.uri().path();
        if !path.starts_with(&base) {
            let _ = response.send_response(http::Response::builder().status(404).body(())?, true);
            continue;
        }
        let rest = &path[base.len()..];
        let segments: Vec<_> = rest
            .trim_end_matches('/')
            .split('/')
            .filter(|s| !s.is_empty())
            .collect();
        let method = request.method().clone();
        if segments.is_empty() && method == http::Method::POST {
            let send = response.send_response(
                http::Response::builder()
                    .status(200)
                    .header("content-type", "application/grpc")
                    .body(())?,
                false,
            )?;
            let stream = crate::multiplex::h2_stream(request.into_body(), send);
            let (app, inbound) = (app.clone(), inbound.clone());
            jobs.spawn(async move {
                if let Ok(_permit) = app.session() {
                    let _ = crate::protocol::serve_inner(stream, peer, inbound, app, None).await;
                }
            });
            continue;
        }
        if segments.is_empty()
            || segments.len() > 2
            || segments[0].len() > 64
            || !segments[0]
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            let _ = response.send_response(http::Response::builder().status(400).body(())?, true);
            continue;
        }
        let key = (peer.ip(), segments[0].to_owned());
        let session = match inbound
            .xhttp
            .session(key.clone(), peer, inbound.clone(), app.clone())
        {
            Ok(s) => s,
            Err(_) => {
                let _ =
                    response.send_response(http::Response::builder().status(429).body(())?, true);
                continue;
            }
        };
        if method == http::Method::GET && segments.len() == 1 {
            if session.down_claimed.swap(true, Ordering::AcqRel) {
                let _ =
                    response.send_response(http::Response::builder().status(409).body(())?, true);
                continue;
            }
            let mut rd = session
                .download
                .lock()
                .unwrap()
                .take()
                .context("XHTTP download already claimed")?;
            let mut send = response.send_response(
                http::Response::builder()
                    .status(200)
                    .header("content-type", "application/octet-stream")
                    .body(())?,
                false,
            )?;
            let lease = Lease {
                state: Arc::downgrade(&inbound.xhttp),
                key,
                session: Arc::downgrade(&session),
            };
            jobs.spawn(async move {
                let _lease = lease;
                let result = async {
                    let mut buf = [0; 16384];
                    loop {
                        let n = rd.read(&mut buf).await?;
                        if n == 0 {
                            break;
                        }
                        *session.last.lock().unwrap() = Instant::now();
                        h2_write(&mut send, bytes::Bytes::copy_from_slice(&buf[..n])).await?;
                    }
                    send.send_data(bytes::Bytes::new(), true)?;
                    Ok::<_, anyhow::Error>(())
                }
                .await;
                let _ = result;
            });
        } else if method == http::Method::POST {
            let sequence = if segments.len() == 2 {
                match segments[1].parse::<u64>() {
                    Ok(v) => Some(v),
                    Err(_) => {
                        let _ = response
                            .send_response(http::Response::builder().status(400).body(())?, true);
                        continue;
                    }
                }
            } else {
                None
            };
            if sequence.is_none() && session.up_claimed.swap(true, Ordering::AcqRel) {
                let _ =
                    response.send_response(http::Response::builder().status(409).body(())?, true);
                continue;
            }
            let mut recv = request.into_body();
            jobs.spawn(async move {
                let result = async {
                    let mut packet = vec![];
                    while let Some(bytes) = recv.data().await {
                        let bytes = bytes?;
                        let n = bytes.len();
                        *session.last.lock().unwrap() = Instant::now();
                        if sequence.is_some() {
                            ensure!(packet.len() + n <= 65536, "XHTTP POST exceeds 64 KiB");
                            packet.extend(&bytes);
                        } else {
                            session.input.send(Upload::Data(None, bytes)).await?;
                        }
                        recv.flow_control().release_capacity(n)?;
                    }
                    if let Some(index) = sequence {
                        session
                            .input
                            .send(Upload::Data(Some(index), packet.into()))
                            .await?;
                    } else {
                        session.input.send(Upload::End).await?;
                    }
                    response
                        .send_response(http::Response::builder().status(200).body(())?, true)?;
                    Ok::<_, anyhow::Error>(())
                }
                .await;
                if result.is_err() {
                    let _ = response.send_response(
                        http::Response::builder().status(400).body(()).unwrap(),
                        true,
                    );
                }
                let _ = result;
            });
        } else {
            let _ = response.send_response(http::Response::builder().status(405).body(())?, true);
        }
    }
    Ok(())
}
