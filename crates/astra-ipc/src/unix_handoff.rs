//! Private hand-off protocol between the unprivileged manager and the
//! sudo-launched TUN core on Unix.
//!
//! The manager owns a 0600 socket in its private state directory and gives the
//! invoking user a random ticket. On Linux the child drops to that user and
//! keeps only the TUN capabilities; on macOS it stays root because utun and
//! route changes require it. Either way it must demonstrate the same
//! configuration digest before the manager stops the old core.
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{
    io::{self, Read, Write},
    path::PathBuf,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

// v2 explicitly carries resident-host mode across sudo's environment cleanup.
pub const PROTOCOL_VERSION: u16 = 2;
const MAX_FRAME_BYTES: u32 = 64 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Offer {
    pub version: u16,
    pub socket: PathBuf,
    pub ticket: String,
    pub binary: PathBuf,
    pub sudo: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Hello {
    pub version: u16,
    pub ticket: String,
}

/// Sent once the peer uid is verified. The child must still confirm its own
/// euid/gid after dropping privileges.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Handoff {
    pub version: u16,
    pub config: PathBuf,
    pub control: PathBuf,
    pub log: PathBuf,
    pub log_level: String,
    pub runtime_dir: PathBuf,
    pub cwd: PathBuf,
    pub digest: u64,
    pub uid: u32,
    pub gid: u32,
    #[serde(default)]
    pub hosted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Prepared {
    pub version: u16,
    pub pid: u32,
    pub uid: u32,
    pub gid: u32,
    pub euid: u32,
    pub digest: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ChildMessage {
    Prepared(Prepared),
    Failed { message: String },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Commit {
    pub version: u16,
    pub digest: u64,
}

/// Commands sent over the authenticated lifeline after the hand-off commits.
/// The manager cannot signal a root-owned macOS core directly, so shutdown is
/// requested by the core itself after it validates this framed message.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum LifelineCommand {
    Graceful,
    Forced,
}

pub fn lifecycle_frame(command: LifelineCommand) -> io::Result<Vec<u8>> {
    frame(&command)
}

/// A program sudo runs as root must not be replaceable by the invoking user
/// (or anything running as them) between this check and the exec: the file
/// and every directory above it must be owned by root and writable only by
/// root. Group write is tolerated only for gid 0.
pub fn ensure_root_controlled(path: &std::path::Path) -> io::Result<PathBuf> {
    use std::os::unix::fs::MetadataExt;
    let path = std::fs::canonicalize(path)?;
    let check = |path: &std::path::Path, metadata: &std::fs::Metadata| {
        let foreign_write =
            metadata.mode() & 0o002 != 0 || (metadata.mode() & 0o020 != 0 && metadata.gid() != 0);
        if metadata.uid() != 0 || foreign_write {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{} must be owned by root and writable only by root before it can run with sudo",
                    path.display()
                ),
            ));
        }
        Ok(())
    };
    let metadata = std::fs::metadata(&path)?;
    if !metadata.is_file() {
        return Err(invalid("the elevated program is not a regular file"));
    }
    check(&path, &metadata)?;
    for ancestor in path.ancestors().skip(1) {
        check(ancestor, &std::fs::metadata(ancestor)?)?;
    }
    Ok(path)
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn frame<T: Serialize>(message: &T) -> io::Result<Vec<u8>> {
    let body = bincode::serialize(message)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if body.is_empty() || body.len() > MAX_FRAME_BYTES as usize {
        return Err(invalid("invalid hand-off frame length"));
    }
    let mut frame = Vec::with_capacity(4 + body.len());
    frame.extend_from_slice(&(body.len() as u32).to_be_bytes());
    frame.extend_from_slice(&body);
    Ok(frame)
}

fn decode<T: DeserializeOwned>(body: &[u8]) -> io::Result<T> {
    bincode::deserialize(body).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

pub fn send_sync<T: Serialize>(
    stream: &mut std::os::unix::net::UnixStream,
    message: &T,
) -> io::Result<()> {
    stream.write_all(&frame(message)?)
}

pub fn receive_sync<T: DeserializeOwned>(
    stream: &mut std::os::unix::net::UnixStream,
) -> io::Result<T> {
    let mut length = [0u8; 4];
    stream.read_exact(&mut length)?;
    let length = u32::from_be_bytes(length);
    if length == 0 || length > MAX_FRAME_BYTES {
        return Err(invalid("invalid hand-off frame length"));
    }
    let mut body = vec![0u8; length as usize];
    stream.read_exact(&mut body)?;
    decode(&body)
}

pub async fn send_async<T: Serialize>(
    stream: &mut tokio::net::UnixStream,
    message: &T,
) -> io::Result<()> {
    stream.write_all(&frame(message)?).await
}

pub async fn receive_async<T: DeserializeOwned>(
    stream: &mut tokio::net::UnixStream,
) -> io::Result<T> {
    let length = stream.read_u32().await?;
    if length == 0 || length > MAX_FRAME_BYTES {
        return Err(invalid("invalid hand-off frame length"));
    }
    let mut body = vec![0u8; length as usize];
    stream.read_exact(&mut body).await?;
    decode(&body)
}

/// A random ticket is the only value the child presents before the manager
/// commits; the socket peer uid is still checked independently.
pub fn ticket() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    let mut text = String::with_capacity(32);
    for byte in bytes {
        text.push_str(&format!("{byte:02x}"));
    }
    Ok(text)
}

pub fn ticket_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// The kernel's view of the process on the other end of a connected Unix
/// socket. Both sides verify it independently of anything the peer claims.
#[cfg(any(target_os = "linux", target_os = "macos"))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PeerCredentials {
    pub uid: u32,
    pub gid: u32,
    /// Zero when the platform cannot name the peer process.
    pub pid: i32,
}

/// `SO_PEERCRED` on Linux; `getpeereid` plus `LOCAL_PEERPID` on macOS.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub fn peer_credentials(fd: std::os::fd::RawFd) -> io::Result<PeerCredentials> {
    #[cfg(target_os = "linux")]
    {
        let mut credentials: libc::ucred = unsafe { std::mem::zeroed() };
        let mut length = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        let result = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                (&mut credentials as *mut libc::ucred).cast(),
                &mut length,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(PeerCredentials {
            uid: credentials.uid,
            gid: credentials.gid,
            pid: credentials.pid,
        })
    }
    #[cfg(target_os = "macos")]
    {
        let mut uid = 0;
        let mut gid = 0;
        if unsafe { libc::getpeereid(fd, &mut uid, &mut gid) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let mut pid: libc::pid_t = 0;
        let mut length = std::mem::size_of::<libc::pid_t>() as libc::socklen_t;
        let result = unsafe {
            libc::getsockopt(
                fd,
                libc::SOL_LOCAL,
                libc::LOCAL_PEERPID,
                (&mut pid as *mut libc::pid_t).cast(),
                &mut length,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(PeerCredentials {
            uid,
            gid,
            pid: pid as i32,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tickets_compare_without_length_leaks() {
        assert!(ticket_eq("abc", "abc"));
        assert!(!ticket_eq("abc", "abd"));
        assert!(!ticket_eq("abc", "ab"));
        let value = ticket().unwrap();
        assert_eq!(value.len(), 32);
        assert_ne!(value, ticket().unwrap());
    }

    #[test]
    fn handoff_preserves_explicit_resident_mode() {
        for hosted in [false, true] {
            let handoff = Handoff {
                version: PROTOCOL_VERSION,
                config: "/private/config.toml".into(),
                control: "/private/control.sock".into(),
                log: "/private/core.log".into(),
                log_level: "info".into(),
                runtime_dir: "/private/runtime".into(),
                cwd: "/private".into(),
                digest: 7,
                uid: 1000,
                gid: 1000,
                hosted,
            };
            let bytes = frame(&handoff).unwrap();
            let decoded: Handoff = decode(&bytes[4..]).unwrap();
            assert_eq!(decoded.version, PROTOCOL_VERSION);
            assert_eq!(decoded.hosted, hosted);
        }
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn peer_credentials_come_from_the_kernel() {
        use std::os::fd::AsRawFd;

        let path = std::env::temp_dir().join(format!("astra-peer-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let client = std::os::unix::net::UnixStream::connect(&path).unwrap();
        let (server, _) = listener.accept().unwrap();
        let from_client = peer_credentials(client.as_raw_fd()).unwrap();
        let from_server = peer_credentials(server.as_raw_fd()).unwrap();
        let _ = std::fs::remove_file(&path);

        assert_eq!(from_client.uid, unsafe { libc::geteuid() });
        assert_eq!(from_client.pid, std::process::id() as i32);
        assert_eq!(from_server.uid, from_client.uid);
        assert_eq!(from_server.pid, from_client.pid);
    }

    #[tokio::test]
    async fn async_frames_round_trip() {
        let (mut a, mut b) = tokio::net::UnixStream::pair().unwrap();
        let offer = Offer {
            version: PROTOCOL_VERSION,
            socket: "/tmp/example.sock".into(),
            ticket: "t".into(),
            binary: "/bin/astra-proxy".into(),
            sudo: "/usr/bin/sudo".into(),
        };
        send_async(&mut a, &offer).await.unwrap();
        let decoded: Offer = receive_async(&mut b).await.unwrap();
        assert_eq!(decoded.ticket, offer.ticket);
        assert_eq!(decoded.socket, offer.socket);
    }
}
