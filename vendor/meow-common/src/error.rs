use std::io;
use thiserror::Error;

/// Explicit local admission pressure, distinct from socket EAGAIN. The
/// marker survives dialer context wrapping without enlarging MeowError.
#[derive(Debug, Error)]
#[error("{0}")]
struct LocalResourceLimit(&'static str);

/// An adapter exhausted its own bounded pool, rather than a remote node
/// failing. Only explicit admission sites should construct this error.
pub fn local_resource_limit(reason: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::WouldBlock, LocalResourceLimit(reason))
}

fn is_local_limit(e: &io::Error) -> bool {
    e.get_ref()
        .is_some_and(<dyn std::error::Error + Send + Sync>::is::<LocalResourceLimit>)
}

#[derive(Error, Debug)]
pub enum MeowError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Config error: {0}")]
    Config(String),
    #[error("DNS error: {0}")]
    Dns(String),
    #[error("Proxy error: {0}")]
    Proxy(String),
    #[error("Not supported: {0}")]
    NotSupported(String),
    #[error("proxy authentication failed")]
    ProxyAuthFailed,
    #[error("HTTP CONNECT failed with status {0}")]
    HttpConnectFailed(u16),
    #[error("SOCKS5 connect failed with reply code {0:#04x}")]
    Socks5ConnectFailed(u8),
    #[error("SOCKS5: no acceptable authentication method")]
    NoAcceptableMethod,
    #[error("no proxy available")]
    NoProxyAvailable,
    #[error("relay chain failed at hop {hop}: {source}")]
    RelayHopFailed { hop: usize, source: Box<MeowError> },
    #[error("UDP not supported by this relay chain")]
    UdpNotSupported,
    #[error("{0}")]
    Other(String),
}

impl MeowError {
    /// True when the failure describes member *capability*, not member
    /// health: `NotSupported`/`UdpNotSupported` at any [`RelayHopFailed`]
    /// depth, or an `Unsupported` io error a front's refusal was mapped to
    /// at the dialer io boundary ([`Self::into_io_error`]). A UDP-less
    /// front or a relay hop that cannot carry the protocol is a permanent
    /// property of the member — dead-marking cannot fix it and only
    /// blackholes the group until the next probe.
    ///
    /// [`RelayHopFailed`]: MeowError::RelayHopFailed
    pub fn is_capability_error(&self) -> bool {
        match self {
            Self::NotSupported(_) | Self::UdpNotSupported => true,
            Self::Io(e) => e.kind() == io::ErrorKind::Unsupported,
            Self::RelayHopFailed { source, .. } => source.is_capability_error(),
            _ => false,
        }
    }

    /// True when the failure is *local* resource exhaustion — this process,
    /// not the remote member, ran out: fd-table, socket-buffer, or memory
    /// pressure (`EMFILE`/`ENFILE`/`ENOBUFS`/`ENOMEM`, or the WSA
    /// equivalents), or an explicit adapter admission limit, at any
    /// [`RelayHopFailed`] depth. Dead-marking the
    /// member punishes a healthy node for a local condition; for a
    /// load-balance group it escalates to `NoProxyAvailable` until the
    /// next probe sweep (issue #668).
    ///
    /// [`RelayHopFailed`]: MeowError::RelayHopFailed
    pub fn is_local_resource_error(&self) -> bool {
        match self {
            Self::Io(e) => is_local_resource_io_error(e),
            Self::RelayHopFailed { source, .. } => source.is_local_resource_error(),
            _ => false,
        }
    }

    /// Whether an `io::Error` carries a raw errno or an errno-less
    /// `OutOfMemory` allocation failure — a concrete socket failure that
    /// classification must not lose to a later context-only error.
    pub fn io_errno_backed(e: &io::Error) -> bool {
        e.raw_os_error().is_some() || e.kind() == io::ErrorKind::OutOfMemory
    }

    /// True when the error still carries a preservable io payload (a
    /// live OS errno or an errno-less `OutOfMemory`) at any
    /// [`RelayHopFailed`] depth. Multi-candidate dial loops use this to
    /// keep a concrete socket failure (e.g. EMFILE on an early
    /// candidate's `socket()`) from being masked by a later candidate's
    /// context-only error — the errno is the classification dead-marking
    /// reads (issue #668).
    ///
    /// [`RelayHopFailed`]: MeowError::RelayHopFailed
    pub fn is_errno_backed(&self) -> bool {
        match self {
            Self::Io(e) => Self::io_errno_backed(e),
            Self::RelayHopFailed { source, .. } => source.is_errno_backed(),
            _ => false,
        }
    }

    /// Accumulator for multi-candidate dial loops: keeps `prev` when it
    /// is errno-backed and `next` is not — a concrete socket error
    /// outranks a context-only failure. Errno-vs-errno and
    /// context-vs-context keep last-wins.
    pub fn prefer_errno(prev: Option<Self>, next: Self) -> Option<Self> {
        if prev.as_ref().is_some_and(Self::is_errno_backed) && !next.is_errno_backed() {
            prev
        } else {
            Some(next)
        }
    }

    /// `io::Error`-level counterpart of [`Self::prefer_errno`] for dial
    /// loops that accumulate `Option<io::Error>` directly.
    pub fn prefer_errno_io(prev: Option<io::Error>, next: io::Error) -> Option<io::Error> {
        if prev.as_ref().is_some_and(Self::io_errno_backed) && !Self::io_errno_backed(&next) {
            prev
        } else {
            Some(next)
        }
    }

    /// Flatten for an `io::Result` boundary (e.g. `ProxyDialer` under a
    /// transport stack) while preserving the classification the inner
    /// error carried:
    ///
    /// - Capability refusals become `ErrorKind::Unsupported` so the caller
    ///   side of the boundary can reconstitute them ([`Self::is_capability_error`]
    ///   recognizes the kind).
    /// - A chain whose innermost error is `Io` passes that io error through
    ///   verbatim — `raw_os_error()` and `kind()` survive, which is what
    ///   dead-marking reads to tell local resource exhaustion from member
    ///   health (issue #668). The `context` prefix is dropped for these:
    ///   std io errors cannot carry context without discarding the errno.
    /// - Anything else collapses to `Other` with a `{context}: {error}`
    ///   message, matching the historical `io::Error::other` flattening.
    pub fn into_io_error(self, context: &str) -> io::Error {
        if self.is_capability_error() {
            return io::Error::new(io::ErrorKind::Unsupported, format!("{context}: {self}"));
        }
        match self.innermost_io() {
            Ok(e) => e,
            Err(e) => io::Error::other(format!("{context}: {e}")),
        }
    }

    /// Wrap a transport-layer `io::Error` at an adapter boundary (e.g.
    /// `ss tcp connect: {e}`). An error carrying a preservable io
    /// payload (raw errno, explicit admission limit, or errno-less
    /// `OutOfMemory`), or one carrying the `Unsupported` capability
    /// class, passes through as
    /// `Io` verbatim — `Proxy`/`Other` stringification would erase the
    /// classification dead-marking reads to tell local resource
    /// exhaustion and capability refusals from member health (issues
    /// #663/#668), and the OS message is self-descriptive. Other
    /// synthesized io errors keep the historical `{context}: {e}`
    /// `Proxy` shape.
    pub fn io_with(context: &str, e: io::Error) -> Self {
        if Self::io_errno_backed(&e) || is_local_limit(&e) || e.kind() == io::ErrorKind::Unsupported
        {
            Self::Io(e)
        } else {
            Self::Proxy(format!("{context}: {e}"))
        }
    }

    /// Unwrap [`RelayHopFailed`] layers down to the innermost `Io` error,
    /// rebuilding the wrapper when the chain bottoms in a non-io leaf so
    /// the caller keeps the full hop context.
    ///
    /// [`RelayHopFailed`]: MeowError::RelayHopFailed
    fn innermost_io(self) -> std::result::Result<io::Error, Self> {
        match self {
            Self::Io(e) => Ok(e),
            Self::RelayHopFailed { hop, source } => match source.innermost_io() {
                Ok(e) => Ok(e),
                Err(source) => Err(Self::RelayHopFailed {
                    hop,
                    source: Box::new(source),
                }),
            },
            e => Err(e),
        }
    }
}

/// `raw_os_error` values that mean the *local* process is out of
/// resources: `EMFILE`/`ENFILE` (fd table), `ENOBUFS` (socket buffers),
/// `ENOMEM`. The set stays tight — `EAGAIN`/`EADDRNOTAVAIL` are transient
/// or config-shaped and are already bounded by dial timeouts.
#[cfg(unix)]
fn is_local_resource_io_error(e: &io::Error) -> bool {
    // `OutOfMemory` covers errno-less synthesized allocation failures.
    is_local_limit(e)
        || e.kind() == io::ErrorKind::OutOfMemory
        || matches!(
            e.raw_os_error(),
            Some(libc::EMFILE | libc::ENFILE | libc::ENOBUFS | libc::ENOMEM)
        )
}

/// Winsock equivalents of the unix set: `WSAEMFILE` (10024),
/// `WSAENOBUFS` (10055), `ERROR_NOT_ENOUGH_MEMORY` (8) and
/// `ERROR_OUTOFMEMORY` (14), which socket/allocation failures surface as
/// raw os errors.
#[cfg(windows)]
fn is_local_resource_io_error(e: &io::Error) -> bool {
    is_local_limit(e)
        || e.kind() == io::ErrorKind::OutOfMemory
        || matches!(e.raw_os_error(), Some(10024 | 10055 | 8 | 14))
}

#[cfg(not(any(unix, windows)))]
fn is_local_resource_io_error(e: &io::Error) -> bool {
    is_local_limit(e) || e.kind() == io::ErrorKind::OutOfMemory
}

pub type Result<T> = std::result::Result<T, MeowError>;

#[cfg(test)]
#[path = "test/error_test.rs"]
mod tests;
