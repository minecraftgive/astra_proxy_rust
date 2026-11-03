//! Serialize endpoint ownership across startup, crashes and recovery.
use super::*;
use std::os::unix::fs::{FileTypeExt, MetadataExt};

fn trusted_owner(uid: u32) -> bool {
    let current = unsafe { libc::geteuid() };
    uid == current
        || uid == 0
        || (current == 0
            && std::env::var("ASTRA_ELEVATED_OWNER")
                .ok()
                .and_then(|owner| owner.split(':').next()?.parse::<u32>().ok())
                == Some(uid))
}

fn lock(path: &Path) -> io::Result<std::fs::File> {
    let mut name = path.as_os_str().to_os_string();
    name.push(".owner-lock");
    let path = PathBuf::from(name);
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.nlink() != 1 || !trusted_owner(metadata.uid()) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "invalid endpoint ownership lock",
        ));
    }
    file.try_lock()
        .map_err(|e| io::Error::new(io::ErrorKind::AddrInUse, e))?;
    super::set_private(&path, 0o600)?;
    Ok(file)
}
fn recover_locked(path: &Path) -> io::Result<bool> {
    let previous = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(e) => return Err(e),
    };
    if !previous.file_type().is_socket() || !trusted_owner(previous.uid()) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "endpoint is not an owned Unix socket",
        ));
    }
    match std::os::unix::net::UnixStream::connect(path) {
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AddrInUse,
                "endpoint is still serving",
            ))
        }
        Err(e) if e.kind() == io::ErrorKind::ConnectionRefused => {}
        Err(e) => return Err(e),
    }
    let current = std::fs::symlink_metadata(path)?;
    if current.dev() != previous.dev() || current.ino() != previous.ino() {
        return Err(io::Error::other("endpoint changed during recovery"));
    }
    std::fs::remove_file(path)?;
    Ok(true)
}
pub(super) fn recover(path: &Path) -> io::Result<bool> {
    if !path.try_exists()? {
        return Ok(false);
    }
    let _lock = lock(path)?;
    recover_locked(path)
}
pub struct LocalListener {
    listener: tokio::net::UnixListener,
    _lock: std::fs::File,
    path: PathBuf,
    inode: (u64, u64),
}
impl LocalListener {
    pub fn bind(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = path.as_ref();
        let lock = lock(path)?;
        recover_locked(path)?;
        let listener = tokio::net::UnixListener::bind(path)?;
        let metadata = std::fs::symlink_metadata(path)?;
        Ok(Self {
            listener,
            _lock: lock,
            path: path.into(),
            inode: (metadata.dev(), metadata.ino()),
        })
    }
    pub async fn accept(&self) -> io::Result<(LocalStream, tokio::net::unix::SocketAddr)> {
        self.listener.accept().await
    }
}
impl Drop for LocalListener {
    fn drop(&mut self) {
        if let Ok(metadata) = std::fs::symlink_metadata(&self.path) {
            if metadata.file_type().is_socket() && (metadata.dev(), metadata.ino()) == self.inode {
                let _ = std::fs::remove_file(&self.path);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn live_socket_and_non_socket_are_preserved_but_abandoned_socket_recovers() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ipc");
        let listener = LocalListener::bind(&path).unwrap();
        assert!(recover(&path).is_err());
        assert!(LocalListener::bind(&path).is_err());
        drop(listener);
        assert!(!path.exists());
        let abandoned = std::os::unix::net::UnixListener::bind(&path).unwrap();
        drop(abandoned);
        assert!(recover(&path).unwrap());
        std::fs::write(&path, b"keep").unwrap();
        assert!(LocalListener::bind(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"keep");
    }
    #[tokio::test]
    async fn listener_drop_preserves_replaced_file() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("ipc");
        let listener = LocalListener::bind(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"replacement").unwrap();
        drop(listener);
        assert_eq!(std::fs::read(&path).unwrap(), b"replacement");
    }
}
