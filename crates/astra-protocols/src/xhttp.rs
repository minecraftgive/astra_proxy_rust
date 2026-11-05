//! XHTTP stream-up: separate HTTP/2 upload and download streams per session.
//! Wire reference: Xray-core v26.3.27 transport/internet/splithttp/hub.go.
use meow_transport::{
    h2_common::{H2Stream, RecvState, StatusPolicy},
    Stream, Transport, TransportError,
};
use std::{
    io,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    task::JoinHandle,
};

#[derive(Clone)]
pub struct StreamUp {
    pub path: String,
    pub host: String,
    pub scheme: &'static str,
    pub headers: Vec<(String, String)>,
}

impl StreamUp {
    fn request(&self, method: &str, session: &str) -> Result<http::Request<()>, TransportError> {
        let (path, query) = self
            .path
            .split_once('?')
            .map_or((self.path.as_str(), None), |(p, q)| (p, Some(q)));
        let host = if self.host.parse::<std::net::Ipv6Addr>().is_ok() {
            format!("[{}]", self.host)
        } else {
            self.host.clone()
        };
        let url = format!(
            "{}://{}{}/{session}",
            self.scheme,
            host,
            path.trim_end_matches('/')
        );
        let mut req = http::Request::builder().method(method).uri(match query {
            Some(q) => format!("{url}?{q}"),
            None => url.clone(),
        });
        if !self
            .headers
            .iter()
            .any(|(k, _)| k.eq_ignore_ascii_case("referer"))
        {
            req = req.header("referer", format!("{url}?x_padding={}", "X".repeat(256)));
        }
        if method == "POST"
            && !self
                .headers
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case("content-type"))
        {
            req = req.header("content-type", "application/grpc");
        }
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        req.body(())
            .map_err(|e| TransportError::Config(format!("XHTTP request: {e}")))
    }
}

struct Tasks(Vec<JoinHandle<()>>);
impl Drop for Tasks {
    fn drop(&mut self) {
        for task in &self.0 {
            task.abort();
        }
    }
}

#[async_trait::async_trait]
impl Transport for StreamUp {
    async fn connect(&self, inner: Box<dyn Stream>) -> meow_transport::Result<Box<dyn Stream>> {
        let session = uuid::Uuid::new_v4().to_string();
        let download = self.request("GET", &session)?;
        let upload = self.request("POST", &session)?;
        let (h2, conn) = meow_transport::h2_common::client_builder()
            .handshake::<_, bytes::Bytes>(inner)
            .await
            .map_err(error)?;
        // Own every task before another await so cancellation cannot leak a connection.
        let mut tasks = Tasks(vec![tokio::spawn(async move {
            let _ = conn.await;
        })]);
        let abort = tasks.0[0].abort_handle();
        let open = async {
            let mut h2 = h2.ready().await.map_err(error)?;
            let (down, _) = h2.send_request(download, true).map_err(error)?;
            let mut h2 = h2.ready().await.map_err(error)?;
            let (up, send) = h2.send_request(upload, false).map_err(error)?;
            Ok::<_, TransportError>((down, up, send))
        };
        let (down, up, send) = tokio::time::timeout(Duration::from_secs(5), open)
            .await
            .map_err(|_| TransportError::Xhttp("stream-up open timed out".into()))??;
        tasks.0.push(tokio::spawn(async move {
            let result = async {
                let response = tokio::time::timeout(Duration::from_secs(15), up)
                    .await
                    .map_err(|_| "upload response timed out".to_string())?
                    .map_err(|e| e.to_string())?;
                if !response.status().is_success() {
                    return Err(format!("upload status {}", response.status()));
                }
                let mut body = response.into_body();
                while let Some(chunk) = body.data().await {
                    let chunk = chunk.map_err(|e| e.to_string())?;
                    body.flow_control()
                        .release_capacity(chunk.len())
                        .map_err(|e| e.to_string())?;
                }
                Ok::<_, String>(())
            }
            .await;
            if let Err(error) = result {
                tracing::debug!(%error, "XHTTP upload failed");
                abort.abort();
            }
        }));
        Ok(Box::new(SplitStream {
            stream: H2Stream::new(
                send,
                RecvState::with_timeout(
                    down,
                    Duration::from_secs(15),
                    StatusPolicy::Success,
                    "xhttp stream-up",
                ),
            ),
            _tasks: tasks,
        }))
    }
}

fn error(e: impl std::fmt::Display) -> TransportError {
    TransportError::Xhttp(e.to_string())
}

struct SplitStream {
    stream: H2Stream,
    _tasks: Tasks,
}
impl AsyncRead for SplitStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_read(cx, buf)
    }
}
impl AsyncWrite for SplitStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.stream).poll_write(cx, buf)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.stream).poll_shutdown(cx)
    }
}

// Packet-up has no wire command for upload EOF. Shutdown first confirms all
// queued POSTs, then returns Unsupported rather than pretending to half-close.
pub struct PacketUp(pub StreamUp);
const POST_BYTES: usize = 64 * 1024;
const POST_WINDOW: usize = 4;
type Upload = Pin<Box<dyn std::future::Future<Output = io::Result<()>> + Send>>;

#[async_trait::async_trait]
impl Transport for PacketUp {
    async fn connect(&self, inner: Box<dyn Stream>) -> meow_transport::Result<Box<dyn Stream>> {
        let session = uuid::Uuid::new_v4().to_string();
        let request = self.0.request("GET", &session)?;
        let (h2, conn) = meow_transport::h2_common::client_builder()
            .handshake::<_, bytes::Bytes>(inner)
            .await
            .map_err(error)?;
        let tasks = Tasks(vec![tokio::spawn(async move {
            let _ = conn.await;
        })]);
        let mut h2 = tokio::time::timeout(Duration::from_secs(5), h2.ready())
            .await
            .map_err(|_| error("packet-up open timed out"))?
            .map_err(error)?;
        let (response, closed_send) = h2.send_request(request, true).map_err(error)?;
        Ok(Box::new(PacketStream {
            down: H2Stream::new(
                closed_send,
                RecvState::with_timeout(
                    response,
                    Duration::from_secs(15),
                    StatusPolicy::Success,
                    "xhttp packet-up",
                ),
            ),
            h2,
            config: self.0.clone(),
            session,
            sequence: 0,
            staged: Vec::new(),
            jobs: std::sync::Mutex::new(Vec::new()),
            failed: None,
            shutting_down: false,
            writer_waker: None,
            tasks,
        }))
    }
}

struct PacketStream {
    down: H2Stream,
    h2: h2::client::SendRequest<bytes::Bytes>,
    config: StreamUp,
    session: String,
    sequence: u64,
    staged: Vec<u8>,
    // Polling requires &mut self; the mutex makes the suspended futures Sync
    // without requiring each HTTP future itself to be Sync.
    jobs: std::sync::Mutex<Vec<Upload>>,
    failed: Option<String>,
    shutting_down: bool,
    writer_waker: Option<std::task::Waker>,
    tasks: Tasks,
}
impl PacketStream {
    fn poll_jobs(&mut self, cx: &mut Context<'_>) -> io::Result<()> {
        if let Some(error) = &self.failed {
            return Err(io::Error::other(error.clone()));
        }
        let jobs = self
            .jobs
            .get_mut()
            .map_err(|_| io::Error::other("XHTTP upload state poisoned"))?;
        let mut index = 0;
        while index < jobs.len() {
            match jobs[index].as_mut().poll(cx) {
                Poll::Pending => index += 1,
                Poll::Ready(Ok(())) => {
                    drop(jobs.swap_remove(index));
                    // Read and write halves may poll these shared POST jobs.
                    // Completion observed by the reader must wake a writer
                    // parked in flush or waiting for an upload window slot.
                    if let Some(waker) = &self.writer_waker {
                        waker.wake_by_ref();
                    }
                }
                Poll::Ready(Err(error)) => {
                    self.failed = Some(error.to_string());
                    if let Some(waker) = &self.writer_waker {
                        waker.wake_by_ref();
                    }
                    jobs.clear();
                    self.staged.clear();
                    for task in &self.tasks.0 {
                        task.abort();
                    }
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    fn queue_staged(&mut self) -> io::Result<()> {
        let jobs = self
            .jobs
            .get_mut()
            .map_err(|_| io::Error::other("XHTTP upload state poisoned"))?;
        if self.staged.is_empty() || jobs.len() >= POST_WINDOW {
            return Ok(());
        }
        let body = bytes::Bytes::from(std::mem::take(&mut self.staged));
        let request = self
            .config
            .request("POST", &format!("{}/{}", self.session, self.sequence));
        self.sequence += 1;
        let client = self.h2.clone();
        jobs.push(Box::pin(async move {
            tokio::time::timeout(Duration::from_secs(15), async move {
                let mut request = request.map_err(io::Error::other)?;
                request.headers_mut().insert(
                    http::header::CONTENT_LENGTH,
                    http::HeaderValue::from_str(&body.len().to_string())
                        .map_err(io::Error::other)?,
                );
                let mut client = client.ready().await.map_err(io::Error::other)?;
                let (response, mut send) = client
                    .send_request(request, false)
                    .map_err(io::Error::other)?;
                let mut remaining = body;
                while !remaining.is_empty() {
                    send.reserve_capacity(remaining.len());
                    let capacity = std::future::poll_fn(|cx| send.poll_capacity(cx))
                        .await
                        .ok_or_else(|| {
                            io::Error::new(io::ErrorKind::BrokenPipe, "XHTTP upload closed")
                        })?
                        .map_err(io::Error::other)?;
                    if capacity == 0 {
                        continue;
                    }
                    let chunk = remaining.split_to(capacity.min(remaining.len()));
                    send.send_data(chunk, remaining.is_empty())
                        .map_err(io::Error::other)?;
                }
                let response = response.await.map_err(io::Error::other)?;
                if !response.status().is_success() {
                    return Err(io::Error::other(format!(
                        "XHTTP upload rejected: HTTP {}",
                        response.status()
                    )));
                }
                let mut body = response.into_body();
                while let Some(chunk) = body.data().await {
                    let chunk = chunk.map_err(io::Error::other)?;
                    body.flow_control()
                        .release_capacity(chunk.len())
                        .map_err(io::Error::other)?;
                }
                Ok(())
            })
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "XHTTP POST timed out"))?
        }));
        Ok(())
    }

    fn drive_uploads(&mut self, cx: &mut Context<'_>) -> io::Result<()> {
        loop {
            self.poll_jobs(cx)?;
            if !self.staged.is_empty()
                && self
                    .jobs
                    .get_mut()
                    .map_err(|_| io::Error::other("XHTTP upload state poisoned"))?
                    .len()
                    < POST_WINDOW
            {
                self.queue_staged()?;
            } else {
                return Ok(());
            }
        }
    }

    fn flush_pending(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.writer_waker = Some(cx.waker().clone());
        self.drive_uploads(cx)?;
        if self.staged.is_empty()
            && self
                .jobs
                .get_mut()
                .map_err(|_| io::Error::other("XHTTP upload state poisoned"))?
                .is_empty()
        {
            Poll::Ready(Ok(()))
        } else {
            Poll::Pending
        }
    }
}
impl AsyncRead for PacketStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        self.drive_uploads(cx)?;
        Pin::new(&mut self.down).poll_read(cx, buf)
    }
}
impl AsyncWrite for PacketStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.writer_waker = Some(cx.waker().clone());
        if self.shutting_down {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "XHTTP upload already closed",
            )));
        }
        self.poll_jobs(cx)?;
        if bytes.is_empty() {
            return Poll::Ready(Ok(0));
        }
        if self.staged.len() == POST_BYTES {
            self.queue_staged()?;
        }
        if self.staged.len() == POST_BYTES {
            return Poll::Pending;
        }
        let count = bytes.len().min(POST_BYTES - self.staged.len());
        self.staged.extend_from_slice(&bytes[..count]);
        if self.staged.len() == POST_BYTES {
            self.queue_staged()?;
        }
        self.poll_jobs(cx)?;
        Poll::Ready(Ok(count))
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.flush_pending(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        self.shutting_down = true;
        std::task::ready!(self.flush_pending(cx))?;
        Poll::Ready(Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "XHTTP packet-up cannot signal upload EOF; close ends the whole tunnel",
        )))
    }
}

#[cfg(test)]
#[path = "test/xhttp_test.rs"]
mod tests;

#[cfg(test)]
#[path = "test/xhttp_packet_test.rs"]
mod packet_tests;
