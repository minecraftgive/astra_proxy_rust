//! Local IPC and filesystem operations shared by the cores and manager.
#[cfg(unix)]
pub use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
#[cfg(unix)]
pub use tokio::net::{UnixListener as LocalListener, UnixStream as LocalStream};

use std::{
    io,
    path::{Path, PathBuf},
};

/// Configure a background process without exposing OS process flags to callers.
pub fn background_command(command: &mut std::process::Command, detached: bool) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let _ = detached;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(if detached {
            0x00000008 | 0x00000200
        } else {
            0x08000000 | 0x00000200
        });
    }
}

pub fn background_core(path: impl AsRef<std::ffi::OsStr>) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(path);
    #[cfg(windows)]
    command.creation_flags(0x08000000);
    command
}

/// Named pipes are removed by their owning handle; Unix sockets need unlinking.
pub fn remove_socket(path: &Path) {
    #[cfg(unix)]
    let _ = std::fs::remove_file(path);
    #[cfg(not(unix))]
    let _ = path;
}

/// Startup recovery for an IPC marker left by an unclean exit.
/// A first-instance pipe reservation proves no server owns this endpoint;
/// permission errors and live pipes must never be treated as stale markers.
pub fn recover_stale_endpoint(path: &Path) -> io::Result<bool> {
    #[cfg(windows)]
    return windows::recover_stale_manager_endpoint(path);
    #[cfg(not(windows))]
    {
        let _ = path;
        Ok(false)
    }
}

/// Compatibility alias for manager startup recovery.
pub fn recover_stale_manager_endpoint(path: &Path) -> io::Result<bool> {
    recover_stale_endpoint(path)
}

/// Bind a core endpoint, recovering only abandoned empty Windows markers.
/// Live/inaccessible pipes and non-marker files are never replaced.
pub fn bind_listener(path: &Path) -> io::Result<LocalListener> {
    #[cfg(windows)]
    recover_stale_endpoint(path)?;
    LocalListener::bind(path)
}

pub fn validate_state_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    if path.as_os_str().len() + 30 >= 104 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "State directory path is too long for Unix sockets",
        ));
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

pub fn executable(name: &str) -> String {
    format!("{name}{}", std::env::consts::EXE_SUFFIX)
}

pub fn home_dir() -> io::Result<PathBuf> {
    #[cfg(windows)]
    let home = std::env::var_os("LOCALAPPDATA").or_else(|| std::env::var_os("USERPROFILE"));
    #[cfg(not(windows))]
    let home = std::env::var_os("HOME");
    home.map(PathBuf::from).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "User home directory is unavailable",
        )
    })
}

pub fn set_private(path: impl AsRef<Path>, mode: u32) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
    }
    #[cfg(windows)]
    {
        let _ = mode;
        windows::set_private(path.as_ref())
    }
}

/// Legacy example /tmp endpoints map into the current user's temporary folder.
pub fn endpoint_path(path: impl AsRef<Path>) -> PathBuf {
    let path = path.as_ref();
    #[cfg(windows)]
    if let Some(name) = path.to_str().and_then(|s| s.strip_prefix("/tmp/")) {
        return std::env::temp_dir().join(name);
    }
    path.to_owned()
}

pub struct ReloadSignal {
    #[cfg(unix)]
    signal: tokio::signal::unix::Signal,
}
impl ReloadSignal {
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            #[cfg(unix)]
            signal: tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup())?,
        })
    }
    pub async fn recv(&mut self) {
        #[cfg(unix)]
        {
            self.signal.recv().await;
        }
        #[cfg(windows)]
        std::future::pending::<()>().await;
    }
}

#[cfg(windows)]
pub use windows::{LocalListener, LocalStream};

pub async fn stream_pair() -> io::Result<(LocalStream, LocalStream)> {
    #[cfg(unix)]
    {
        LocalStream::pair()
    }
    #[cfg(windows)]
    {
        LocalStream::pair().await
    }
}

// Creation inherits the containing directory ACL; set_private applies the
// explicit owner-only ACL before writing secrets to a newly created file.
#[cfg(windows)]
pub trait OpenOptionsExt {
    fn mode(&mut self, mode: u32) -> &mut Self;
}
#[cfg(windows)]
impl OpenOptionsExt for std::fs::OpenOptions {
    fn mode(&mut self, _: u32) -> &mut Self {
        self
    }
}
#[cfg(windows)]
pub trait DirBuilderExt {
    fn mode(&mut self, mode: u32) -> &mut Self;
}
#[cfg(windows)]
impl DirBuilderExt for std::fs::DirBuilder {
    fn mode(&mut self, _: u32) -> &mut Self {
        self
    }
}

#[cfg(windows)]
mod windows {
    use super::*;
    use std::{
        ffi::c_void,
        os::windows::{ffi::OsStrExt, io::AsRawHandle},
        pin::Pin,
        task::{Context, Poll},
    };
    use tokio::{
        io::{AsyncRead, AsyncWrite, ReadBuf},
        net::windows::named_pipe::{
            ClientOptions, NamedPipeClient, NamedPipeServer, ServerOptions,
        },
        sync::Mutex,
    };
    use windows_sys::Win32::{
        Foundation::{CloseHandle, LocalFree},
        Security::{
            Authorization::{
                ConvertSidToStringSidW, ConvertStringSecurityDescriptorToSecurityDescriptorW,
                SetNamedSecurityInfoW, SE_FILE_OBJECT,
            },
            GetSecurityDescriptorDacl, GetTokenInformation, TokenUser, DACL_SECURITY_INFORMATION,
            PROTECTED_DACL_SECURITY_INFORMATION, SECURITY_ATTRIBUTES, TOKEN_QUERY, TOKEN_USER,
        },
        System::Threading::{GetCurrentProcess, OpenProcessToken},
    };

    struct Descriptor(*mut c_void);
    impl Drop for Descriptor {
        fn drop(&mut self) {
            unsafe {
                LocalFree(self.0);
            }
        }
    }
    fn descriptor() -> io::Result<Descriptor> {
        // Use the concrete user SID: access stays private across different
        // logon sessions of this user, without granting other local users access.
        unsafe {
            let mut token = std::ptr::null_mut();
            if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut size = 0;
            GetTokenInformation(token, TokenUser, std::ptr::null_mut(), 0, &mut size);
            let mut data = vec![0usize; (size as usize).div_ceil(std::mem::size_of::<usize>())];
            let ok =
                GetTokenInformation(token, TokenUser, data.as_mut_ptr().cast(), size, &mut size);
            let error = io::Error::last_os_error();
            CloseHandle(token);
            if ok == 0 {
                return Err(error);
            }
            let user = &*data.as_ptr().cast::<TOKEN_USER>();
            let mut sid = std::ptr::null_mut();
            if ConvertSidToStringSidW(user.User.Sid, &mut sid) == 0 {
                return Err(io::Error::last_os_error());
            }
            let mut len = 0;
            while *sid.add(len) != 0 {
                len += 1;
            }
            let sid_string = String::from_utf16_lossy(std::slice::from_raw_parts(sid, len));
            LocalFree(sid.cast());
            let sddl: Vec<u16> = format!("D:P(A;OICI;GA;;;{sid_string})\0")
                .encode_utf16()
                .collect();
            let mut descriptor = std::ptr::null_mut();
            if ConvertStringSecurityDescriptorToSecurityDescriptorW(
                sddl.as_ptr(),
                1,
                &mut descriptor,
                std::ptr::null_mut(),
            ) == 0
            {
                return Err(io::Error::last_os_error());
            }
            Ok(Descriptor(descriptor))
        }
    }
    pub(super) fn set_private(path: &Path) -> io::Result<()> {
        let descriptor = descriptor()?;
        let mut path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        unsafe {
            let (mut present, mut defaulted, mut acl) = (0, 0, std::ptr::null_mut());
            if GetSecurityDescriptorDacl(descriptor.0, &mut present, &mut acl, &mut defaulted) == 0
            {
                return Err(io::Error::last_os_error());
            }
            let error = SetNamedSecurityInfoW(
                path.as_mut_ptr(),
                SE_FILE_OBJECT,
                DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                acl,
                std::ptr::null_mut(),
            );
            if error != 0 {
                return Err(io::Error::from_raw_os_error(error as i32));
            }
        }
        Ok(())
    }
    fn pipe_name(path: &Path) -> io::Result<String> {
        let absolute = std::path::absolute(path)?;
        // Canonicalize the existing parent so relative/absolute and extended
        // Windows paths identify the same endpoint in different processes.
        let parent = absolute.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "IPC path needs a parent")
        })?;
        let canonical =
            std::fs::canonicalize(parent)?.join(absolute.file_name().ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "IPC path needs a name")
            })?);
        // Fixed FNV-1a rather than DefaultHasher: names must remain compatible
        // when cores were built with different Rust toolchain versions.
        let normalized = canonical
            .to_string_lossy()
            .replace('/', "\\")
            .to_lowercase();
        let hash = normalized
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .fold(0xcbf29ce484222325u64, |hash, byte| {
                (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3)
            });
        Ok(format!(r"\\.\pipe\astra-{hash:016x}"))
    }
    fn server(name: &str, first: bool) -> io::Result<NamedPipeServer> {
        let descriptor = descriptor()?;
        let mut attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: descriptor.0,
            bInheritHandle: 0,
        };
        // SAFETY: attributes and its descriptor stay valid through CreateNamedPipe.
        unsafe {
            ServerOptions::new()
                .first_pipe_instance(first)
                .reject_remote_clients(true)
                .create_with_security_attributes_raw(
                    name,
                    (&mut attributes as *mut SECURITY_ATTRIBUTES).cast(),
                )
        }
    }

    pub struct LocalListener {
        server: Mutex<NamedPipeServer>,
        name: String,
        marker: PathBuf,
    }
    pub(super) fn recover_stale_manager_endpoint(path: &Path) -> io::Result<bool> {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::Storage::FileSystem::FILE_SHARE_DELETE;
        let marker = endpoint_path(path);
        let metadata = match std::fs::symlink_metadata(&marker) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(error),
        };
        if !metadata.file_type().is_file() || metadata.len() != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "IPC endpoint is not an empty regular marker file",
            ));
        }
        // Serialize recoveries through the marker itself. Allow our unlink but
        // deny other readers/writers until the pipe reservation is released.
        let marker_handle = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_DELETE)
            .open(&marker)?;
        // Keep the reservation alive through removal. This also protects live
        // servers from older versions that do not keep a marker handle open.
        let reservation = server(&pipe_name(&marker)?, true)?;
        std::fs::remove_file(&marker)?;
        drop(reservation);
        drop(marker_handle);
        Ok(true)
    }
    impl LocalListener {
        pub fn bind(path: impl AsRef<Path>) -> io::Result<Self> {
            let marker = endpoint_path(path);
            let name = pipe_name(&marker)?;
            // Preserve Unix behavior: existing files/endpoints are never replaced.
            let file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&marker)?;
            let result = set_private(&marker).and_then(|()| server(&name, true));
            drop(file);
            match result {
                Ok(server) => Ok(Self {
                    server: Mutex::new(server),
                    name,
                    marker,
                }),
                Err(error) => {
                    let _ = std::fs::remove_file(marker);
                    Err(error)
                }
            }
        }
        pub async fn accept(&self) -> io::Result<(LocalStream, ())> {
            let mut current = self.server.lock().await;
            current.connect().await?;
            // Always keep an instance available; accept is cancellation-safe.
            let next = server(&self.name, false)?;
            Ok((
                LocalStream::Server(std::mem::replace(&mut *current, next)),
                (),
            ))
        }
    }
    impl Drop for LocalListener {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.marker);
        }
    }
    pub enum LocalStream {
        Client(NamedPipeClient),
        Server(NamedPipeServer),
    }
    impl LocalStream {
        pub async fn connect(path: impl AsRef<Path>) -> io::Result<Self> {
            let name = pipe_name(&endpoint_path(path))?;
            loop {
                match ClientOptions::new().open(&name) {
                    Ok(client) => return Ok(Self::Client(client)),
                    Err(error) if error.raw_os_error() == Some(231) => {
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await
                    }
                    Err(error) => return Err(error),
                }
            }
        }
        pub async fn pair() -> io::Result<(Self, Self)> {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let name = format!(
                r"\\.\pipe\astra-pair-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            );
            let server = server(&name, true)?;
            let client = ClientOptions::new().open(name)?;
            server.connect().await?;
            Ok((Self::Client(client), Self::Server(server)))
        }
    }
    impl AsyncRead for LocalStream {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            match self.get_mut() {
                Self::Client(s) => Pin::new(s).poll_read(cx, buf),
                Self::Server(s) => Pin::new(s).poll_read(cx, buf),
            }
        }
    }
    impl AsyncWrite for LocalStream {
        fn poll_write(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            match self.get_mut() {
                Self::Client(s) => Pin::new(s).poll_write(cx, buf),
                Self::Server(s) => Pin::new(s).poll_write(cx, buf),
            }
        }
        fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            match self.get_mut() {
                Self::Client(s) => Pin::new(s).poll_flush(cx),
                Self::Server(s) => Pin::new(s).poll_flush(cx),
            }
        }
        fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            match self.get_mut() {
                Self::Client(s) => Pin::new(s).poll_shutdown(cx),
                Self::Server(s) => Pin::new(s).poll_shutdown(cx),
            }
        }
    }

    /// Stable volume/file ID from the open handle, including after log rotation.
    pub fn file_identity(file: &std::fs::File) -> io::Result<(u64, u64)> {
        use windows_sys::Win32::Storage::FileSystem::{
            GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION,
        };
        let mut info: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { GetFileInformationByHandle(file.as_raw_handle(), &mut info) } == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((
            info.dwVolumeSerialNumber as u64,
            (u64::from(info.nFileIndexHigh) << 32) | u64::from(info.nFileIndexLow),
        ))
    }
}

pub async fn file_identity(file: &tokio::fs::File) -> io::Result<(u64, u64)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let metadata = file.metadata().await?;
        Ok((metadata.dev(), metadata.ino()))
    }
    #[cfg(windows)]
    {
        let file = file.try_clone().await?.into_std().await;
        windows::file_identity(&file)
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;
    use tokio::time::{timeout, Duration};

    #[tokio::test]
    async fn recovering_bind_handles_stale_markers_without_replacing_live_endpoints() {
        let directory =
            std::env::temp_dir().join(format!("astra-core-recovery-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("rule.sock");
        std::fs::write(&path, b"").unwrap();
        let listener = bind_listener(&path).unwrap();
        assert!(path.exists());
        assert!(bind_listener(&path).is_err());
        assert!(path.exists());
        drop(listener);
        std::fs::write(&path, b"preserve").unwrap();
        assert!(bind_listener(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"preserve");
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }

    #[tokio::test]
    async fn manager_recovery_only_removes_abandoned_empty_markers() {
        let directory =
            std::env::temp_dir().join(format!("astra-manager-recovery-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("manager.sock");
        assert!(!recover_stale_manager_endpoint(&path).unwrap());
        std::fs::write(&path, b"preserve this file").unwrap();
        assert!(recover_stale_manager_endpoint(&path).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"preserve this file");
        std::fs::remove_file(&path).unwrap();
        let listener = LocalListener::bind(&path).unwrap();
        assert!(recover_stale_manager_endpoint(&path).is_err());
        assert!(path.exists());
        drop(listener);
        // An unclean process exit leaves just this marker, with no pipe server.
        std::fs::write(&path, b"").unwrap();
        assert!(recover_stale_manager_endpoint(&path).unwrap());
        assert!(!path.exists());
        let listener = LocalListener::bind(&path).unwrap();
        drop(listener);
        std::fs::remove_dir(directory).unwrap();
    }

    #[tokio::test]
    async fn named_pipe_accept_cancellation_concurrency_and_cleanup() {
        let directory = std::env::temp_dir().join(format!("astra-ipc-中文 {}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let path = directory.join("local.sock");
        let listener = LocalListener::bind(&path).unwrap();
        assert!(LocalListener::bind(&path).is_err());
        assert!(timeout(Duration::from_millis(20), listener.accept())
            .await
            .is_err());
        let server = tokio::spawn(async move {
            let mut workers = tokio::task::JoinSet::new();
            for _ in 0..16 {
                let (mut stream, _) = listener.accept().await.unwrap();
                workers.spawn(async move {
                    let data: Vec<u8> = crate::receive_management(&mut stream).await.unwrap();
                    crate::send_management(&mut stream, &data).await.unwrap();
                });
            }
            while let Some(result) = workers.join_next().await {
                result.unwrap();
            }
        });
        let mut clients = tokio::task::JoinSet::new();
        for value in 0..16u8 {
            let path = path.clone();
            clients.spawn(async move {
                let mut stream = LocalStream::connect(path).await.unwrap();
                let payload = vec![value; 65536];
                crate::send_management(&mut stream, &payload).await.unwrap();
                let echoed: Vec<u8> = crate::receive_management(&mut stream).await.unwrap();
                assert_eq!(payload, echoed);
            });
        }
        timeout(Duration::from_secs(10), async {
            while let Some(result) = clients.join_next().await {
                result.unwrap();
            }
            server.await.unwrap();
        })
        .await
        .unwrap();
        assert!(!path.exists());
        std::fs::remove_dir(directory).unwrap();
    }
}

/// Both interactive runs and service managers get the same shutdown path.
pub async fn shutdown_signal() -> io::Result<()> {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = terminate.recv() => Ok(()),
        }
    }
    #[cfg(windows)]
    {
        let mut interrupt = tokio::signal::windows::ctrl_break()?;
        let mut close = tokio::signal::windows::ctrl_close()?;
        let mut shutdown = tokio::signal::windows::ctrl_shutdown()?;
        tokio::select! {
            result = tokio::signal::ctrl_c() => result,
            _ = interrupt.recv() => Ok(()),
            _ = close.recv() => Ok(()),
            _ = shutdown.recv() => Ok(()),
        }
    }
}
