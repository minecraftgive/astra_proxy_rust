//! Route-level metadata and byte accounting shared by every forwarding mode.
use astra_model::Inbound;
use astra_model::Network;
use astra_model::RouteAction;
use astra_model::Target;
use std::{
    io,
    net::SocketAddr,
    pin::Pin,
    sync::atomic::{AtomicBool, AtomicU64, Ordering},
    task::{Context, Poll},
    time::Instant,
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

use std::{
    collections::BTreeMap,
    future::Future,
    sync::{Arc, Mutex, Weak},
};

#[derive(Default)]
pub(crate) struct Metrics {
    next: AtomicU64,
    total: AtomicU64,
    upload: AtomicU64,
    download: AtomicU64,
    proxy_upload: AtomicU64,
    proxy_download: AtomicU64,
    flows: Mutex<BTreeMap<u64, Weak<Flow>>>,
    nodes: Mutex<BTreeMap<String, Arc<AtomicU64>>>,
}
impl Metrics {
    pub(crate) fn node_totals(&self) -> BTreeMap<String, u64> {
        self.nodes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .map(|(name, total)| (name.clone(), total.load(Ordering::Relaxed)))
            .collect()
    }
    fn flows(&self) -> std::sync::MutexGuard<'_, BTreeMap<u64, Weak<Flow>>> {
        match self.flows.lock() {
            Ok(flows) => flows,
            Err(poisoned) => {
                // This is an index, not routing state. Cancel every reachable
                // flow before discarding a possibly partial index, then let new
                // flows register against an empty, consistent map.
                let mut flows = poisoned.into_inner();
                for flow in flows.values().filter_map(Weak::upgrade) {
                    flow.failed.store(true, Ordering::Relaxed);
                    flow.cancel.send_replace(true);
                }
                flows.clear();
                self.flows.clear_poison();
                tracing::error!("traffic index update panicked; cancelled indexed flows and reset accounting index");
                flows
            }
        }
    }
    pub fn snapshot(&self) -> astra_model::runtime::Snapshot {
        let flows = self.flows();
        let mut snapshot = astra_model::runtime::Snapshot {
            pid: std::process::id(),
            upload: self.upload.load(Ordering::Relaxed),
            download: self.download.load(Ordering::Relaxed),
            proxy_upload: self.proxy_upload.load(Ordering::Relaxed),
            proxy_download: self.proxy_download.load(Ordering::Relaxed),
            active: flows.len(),
            total: self.total.load(Ordering::Relaxed),
            truncated: flows.len() > 128,
            ..Default::default()
        };
        snapshot.connections = flows
            .values()
            .rev()
            .filter_map(Weak::upgrade)
            .take(128)
            .map(|flow| astra_model::runtime::Connection {
                id: flow.id,
                source: flow.source.map(|s| s.to_string()),
                host: flow.target.host.chars().take(253).collect(),
                port: flow.target.port,
                network: format!("{:?}", flow.network),
                inbound: format!("{:?}", flow.inbound),
                outbound: match &flow.action {
                    RouteAction::Direct => "DIRECT".into(),
                    RouteAction::Block => "REJECT".into(),
                    RouteAction::Proxy(tag) => tag.chars().take(256).collect(),
                },
                upload: flow.up.load(Ordering::Relaxed),
                download: flow.down.load(Ordering::Relaxed),
                elapsed_ms: flow.started.elapsed().as_millis() as u64,
            })
            .collect();
        snapshot
    }
    pub fn disconnect(&self, id: Option<u64>) -> bool {
        let flows = self.flows();
        let mut found = id.is_none();
        for (key, flow) in flows.iter() {
            if id.is_none_or(|id| id == *key) {
                if let Some(flow) = flow.upgrade() {
                    flow.cancel.send_replace(true);
                    found = true;
                }
            }
        }
        found
    }
}

pub(crate) struct Traffic {
    metrics: Arc<Metrics>,
    flow: Arc<Flow>,
    node: Option<Arc<AtomicU64>>,
}
impl std::ops::Deref for Traffic {
    type Target = Flow;
    fn deref(&self) -> &Flow {
        &self.flow
    }
}
pub(crate) struct Flow {
    id: u64,
    source: Option<SocketAddr>,
    target: Target,
    inbound: Inbound,
    network: Network,
    action: RouteAction,
    started: Instant,
    up: AtomicU64,
    down: AtomicU64,
    packets_up: AtomicU64,
    packets_down: AtomicU64,
    failed: AtomicBool,
    connected: AtomicBool,
    cancel: tokio::sync::watch::Sender<bool>,
}
impl Traffic {
    pub(crate) fn new(
        metrics: &Arc<Metrics>,
        source: Option<SocketAddr>,
        target: &Target,
        inbound: Inbound,
        network: Network,
        action: &RouteAction,
    ) -> Option<Self> {
        let id = metrics.next.fetch_add(1, Ordering::Relaxed) + 1;
        let flow = Arc::new(Flow {
            id,
            source,
            target: target.clone(),
            inbound,
            network,
            action: action.clone(),
            started: Instant::now(),
            up: AtomicU64::new(0),
            down: AtomicU64::new(0),
            packets_up: AtomicU64::new(0),
            packets_down: AtomicU64::new(0),
            failed: AtomicBool::new(false),
            connected: AtomicBool::new(false),
            cancel: tokio::sync::watch::channel(false).0,
        });
        metrics.total.fetch_add(1, Ordering::Relaxed);
        metrics.flows().insert(id, Arc::downgrade(&flow));
        // Resolve once at flow creation; forwarding adds only an atomic increment.
        let node = match action {
            RouteAction::Proxy(name) => Some(
                metrics
                    .nodes
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .entry(name.clone())
                    .or_default()
                    .clone(),
            ),
            _ => None,
        };
        Some(Self {
            metrics: metrics.clone(),
            flow,
            node,
        })
    }
    pub(crate) fn cancellation(&self) -> Pin<Box<dyn Future<Output = ()> + Send>> {
        let mut receiver = self.cancel.subscribe();
        Box::pin(async move {
            let _ = receiver.wait_for(|closed| *closed).await;
        })
    }
    pub(crate) async fn cancelled(traffic: &Option<Self>) {
        match traffic {
            Some(traffic) => traffic.cancellation().await,
            None => std::future::pending().await,
        }
    }
    pub(crate) fn connected(&self) {
        self.connected.store(true, Ordering::Relaxed);
        tracing::debug!(target: "astra_route", flow = self.id, phase = "forward", source = ?self.source, host = ?self.target.host, port = self.target.port, inbound = ?self.inbound, network = ?self.network, action = ?self.action, "forwarding started");
    }
    pub(crate) fn upload(&self, bytes: usize) {
        if let Some(node) = &self.node {
            node.fetch_add(bytes as u64, Ordering::Relaxed);
        }
        if matches!(self.action, RouteAction::Proxy(_)) {
            self.metrics
                .proxy_upload
                .fetch_add(bytes as u64, Ordering::Relaxed);
        }
        self.up.fetch_add(bytes as u64, Ordering::Relaxed);
        self.metrics
            .upload
            .fetch_add(bytes as u64, Ordering::Relaxed);
        if self.network == Network::Udp {
            self.packets_up.fetch_add(1, Ordering::Relaxed);
        }
    }
    pub(crate) fn download(&self, bytes: usize) {
        if let Some(node) = &self.node {
            node.fetch_add(bytes as u64, Ordering::Relaxed);
        }
        if matches!(self.action, RouteAction::Proxy(_)) {
            self.metrics
                .proxy_download
                .fetch_add(bytes as u64, Ordering::Relaxed);
        }
        self.down.fetch_add(bytes as u64, Ordering::Relaxed);
        self.metrics
            .download
            .fetch_add(bytes as u64, Ordering::Relaxed);
        if self.network == Network::Udp {
            self.packets_down.fetch_add(1, Ordering::Relaxed);
        }
    }
    pub(crate) fn failed(&self, error: impl std::fmt::Display) {
        if !self.failed.swap(true, Ordering::Relaxed) {
            tracing::debug!(target: "astra_route", flow = self.id, phase = "failed", source = ?self.source, host = ?self.target.host, port = self.target.port, inbound = ?self.inbound, network = ?self.network, action = ?self.action, error = %error, "forwarding failed");
        }
    }
}
impl Drop for Traffic {
    fn drop(&mut self) {
        self.metrics.flows().remove(&self.id);
        let outcome = if self.failed.load(Ordering::Relaxed) {
            "failed"
        } else if self.connected.load(Ordering::Relaxed) {
            "closed"
        } else {
            "cancelled"
        };
        let up = self.up.load(Ordering::Relaxed);
        let down = self.down.load(Ordering::Relaxed);
        let elapsed_ms = self.started.elapsed().as_millis() as u64;
        if self.network == Network::Udp {
            tracing::debug!(target: "astra_route", flow = self.id, phase = "closed", source = ?self.source, host = ?self.target.host, port = self.target.port, inbound = ?self.inbound, network = ?self.network, action = ?self.action, outcome, up, down, packets_up = self.packets_up.load(Ordering::Relaxed), packets_down = self.packets_down.load(Ordering::Relaxed), elapsed_ms, "forwarding ended");
        } else {
            tracing::debug!(target: "astra_route", flow = self.id, phase = "closed", source = ?self.source, host = ?self.target.host, port = self.target.port, inbound = ?self.inbound, network = ?self.network, action = ?self.action, outcome, up, down, elapsed_ms, "forwarding ended");
        }
    }
}

pub(crate) struct Stream<S> {
    pub inner: S,
    pub traffic: Option<Traffic>,
    cancellation: Option<Pin<Box<dyn Future<Output = ()> + Send>>>,
    closed: bool,
}
impl<S> Stream<S> {
    pub fn new(inner: S, traffic: Option<Traffic>) -> Self {
        let cancellation = traffic.as_ref().map(Traffic::cancellation);
        Self {
            inner,
            traffic,
            cancellation,
            closed: false,
        }
    }
    fn check_cancelled(&mut self, cx: &mut Context<'_>) -> io::Result<()> {
        if !self.closed
            && self
                .cancellation
                .as_mut()
                .is_some_and(|future| future.as_mut().poll(cx).is_ready())
        {
            self.closed = true;
            self.cancellation = None;
        }
        if self.closed {
            Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "connection closed",
            ))
        } else {
            Ok(())
        }
    }
}
impl<S: AsyncRead + Unpin> AsyncRead for Stream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if let Err(error) = this.check_cancelled(cx) {
            return Poll::Ready(Err(error));
        }
        let before = buf.filled().len();
        let result = Pin::new(&mut this.inner).poll_read(cx, buf);
        if let Some(traffic) = &this.traffic {
            match &result {
                Poll::Ready(Ok(())) => traffic.download(buf.filled().len() - before),
                Poll::Ready(Err(error)) => traffic.failed(error),
                _ => {}
            }
        }
        result
    }
}
impl<S: AsyncWrite + Unpin> AsyncWrite for Stream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if let Err(error) = this.check_cancelled(cx) {
            return Poll::Ready(Err(error));
        }
        let result = Pin::new(&mut this.inner).poll_write(cx, bytes);
        if let Some(traffic) = &this.traffic {
            match &result {
                Poll::Ready(Ok(size)) => traffic.upload(*size),
                Poll::Ready(Err(error)) => traffic.failed(error),
                _ => {}
            }
        }
        result
    }
    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bytes: &[io::IoSlice<'_>],
    ) -> Poll<io::Result<usize>> {
        let this = self.get_mut();
        if let Err(error) = this.check_cancelled(cx) {
            return Poll::Ready(Err(error));
        }
        let result = Pin::new(&mut this.inner).poll_write_vectored(cx, bytes);
        if let Some(traffic) = &this.traffic {
            match &result {
                Poll::Ready(Ok(size)) => traffic.upload(*size),
                Poll::Ready(Err(error)) => traffic.failed(error),
                _ => {}
            }
        }
        result
    }
    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if let Err(error) = this.check_cancelled(cx) {
            return Poll::Ready(Err(error));
        }
        let result = Pin::new(&mut this.inner).poll_flush(cx);
        if let (Some(traffic), Poll::Ready(Err(error))) = (&this.traffic, &result) {
            traffic.failed(error);
        }
        result
    }
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let this = self.get_mut();
        if let Err(error) = this.check_cancelled(cx) {
            return Poll::Ready(Err(error));
        }
        let result = Pin::new(&mut this.inner).poll_shutdown(cx);
        if let (Some(traffic), Poll::Ready(Err(error))) = (&this.traffic, &result) {
            traffic.failed(error);
        }
        result
    }
}

#[cfg(test)]
#[path = "../test/proxy_traffic_test.rs"]
mod tests;
