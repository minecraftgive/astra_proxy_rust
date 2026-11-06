//! Local IPC and filesystem operations shared by the cores and manager.
#[cfg(unix)]
pub use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
#[cfg(unix)]
pub use tokio::net::UnixStream as LocalStream;
#[cfg(unix)]
pub use unix_listener::LocalListener;
#[cfg(unix)]
#[path = "unix_listener.rs"]
mod unix_listener;

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
    #[cfg(windows)]
    {
        let mut command = tokio::process::Command::new(path);
        command.creation_flags(0x08000000);
        command
    }
    #[cfg(not(windows))]
    tokio::process::Command::new(path)
}

/// Named pipes are removed by their owning handle; Unix sockets need unlinking.
pub fn remove_socket(path: &Path) {
    #[cfg(unix)]
    let _ = unix_listener::recover(path);
    #[cfg(not(unix))]
    let _ = path;
}

/// Startup recovery for an IPC marker left by an unclean exit.
/// A first-instance pipe reservation proves no server owns this endpoint;
/// permission errors and live pipes must never be treated as stale markers.
pub fn recover_stale_endpoint(path: &Path) -> io::Result<bool> {
    #[cfg(windows)]
    return windows::recover_stale_manager_endpoint(path, false);
    #[cfg(unix)]
    return unix_listener::recover(path);
}

/// Explicitly discard an abandoned control endpoint whose owner cannot be
/// verified. The containing directory must satisfy the platform's user-owner
/// policy; live endpoints, links and unrelated files are still preserved.
/// This never stops a process.
pub fn force_recover_control_endpoint(path: &Path) -> io::Result<bool> {
    #[cfg(windows)]
    return windows::recover_stale_manager_endpoint(path, true);
    #[cfg(unix)]
    return unix_listener::force_recover(path);
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
    {
        // Service-launched cores deliberately receive no profile environment.
        // They still run with the original user's token, so resolve that user's
        // Local AppData folder when neither environment override is available.
        std::env::var_os("LOCALAPPDATA")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .map_or_else(
                || {
                    known_folder_dir(
                        &windows_sys::Win32::UI::Shell::FOLDERID_LocalAppData,
                        "local application data",
                    )
                },
                Ok,
            )
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(PathBuf::from).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "User home directory is unavailable",
            )
        })
    }
}

#[cfg(windows)]
fn known_folder_dir(folder: &windows_sys::core::GUID, name: &str) -> io::Result<PathBuf> {
    use std::{ffi::OsString, os::windows::ffi::OsStringExt};
    use windows_sys::Win32::{
        System::Com::CoTaskMemFree,
        UI::Shell::{SHGetKnownFolderPath, KF_FLAG_DONT_VERIFY},
    };
    let mut path = std::ptr::null_mut();
    let status = unsafe {
        SHGetKnownFolderPath(
            folder,
            KF_FLAG_DONT_VERIFY as u32,
            std::ptr::null_mut(),
            &mut path,
        )
    };
    let result = if status < 0 || path.is_null() {
        Err(io::Error::other(format!(
            "Cannot locate {name} directory (HRESULT 0x{:08X})",
            status as u32
        )))
    } else {
        let mut length = 0;
        unsafe {
            while *path.add(length) != 0 {
                length += 1;
            }
            Ok(PathBuf::from(OsString::from_wide(
                std::slice::from_raw_parts(path, length),
            )))
        }
    };
    // Free the returned allocation even when folder lookup fails.
    unsafe { CoTaskMemFree(path.cast()) };
    result
}

/// Locate the current user's Documents directory without creating it.
/// Windows uses the known-folder location, including redirected/OneDrive paths.
pub fn documents_dir() -> io::Result<PathBuf> {
    #[cfg(windows)]
    {
        known_folder_dir(
            &windows_sys::Win32::UI::Shell::FOLDERID_Documents,
            "Documents",
        )
    }
    #[cfg(not(windows))]
    {
        Ok(home_dir()?.join("Documents"))
    }
}

pub fn set_private(path: impl AsRef<Path>, mode: u32) -> io::Result<()> {
    #[cfg(unix)]
    {
        let path = path.as_ref();
        // A root core (macOS TUN) applies this to paths inside the user's
        // directories; following a swapped-in symlink would chmod/chown an
        // arbitrary system file.
        if std::fs::symlink_metadata(path)?.file_type().is_symlink() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{} is a symbolic link; refusing to change it",
                    path.display()
                ),
            ));
        }
        #[cfg(target_vendor = "apple")]
        {
            use std::os::unix::ffi::OsStrExt;
            let text =
                std::ffi::CString::new(path.as_os_str().as_bytes()).map_err(io::Error::other)?;
            if unsafe { libc::lchmod(text.as_ptr(), mode as libc::mode_t) } != 0 {
                return Err(io::Error::last_os_error());
            }
        }
        #[cfg(not(target_vendor = "apple"))]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
        }
        adopt_owner(path)?;
        Ok(())
    }
    #[cfg(windows)]
    {
        let _ = mode;
        windows::set_private(path.as_ref())
    }
}

/// The macOS elevated core runs as root but creates files (the control socket,
/// logs, caches) inside the unprivileged user's state directory. Hand those
/// files back to the invoking user so the next normal launch can read, rotate
/// and replace them. No-op unless the sudo hand-off set `ASTRA_ELEVATED_OWNER`
/// and this process is actually root.
pub fn adopt_owner(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        if unsafe { libc::geteuid() } != 0 {
            return Ok(());
        }
        let Some((uid, gid)) = std::env::var("ASTRA_ELEVATED_OWNER")
            .ok()
            .and_then(|value| {
                let (uid, gid) = value.split_once(':')?;
                Some((uid.parse::<u32>().ok()?, gid.parse::<u32>().ok()?))
            })
        else {
            return Ok(());
        };
        // Never follow a link the user could have swapped in for this path.
        std::os::unix::fs::lchown(path, Some(uid), Some(gid))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
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
pub use windows::{allow_unelevated_clients, LocalListener, LocalStream};

/// Connect to a local IPC endpoint and verify who serves it. Unix endpoints in
/// shared directories such as /tmp could be pre-bound by another local user to
/// spoof routing or management replies; only this user or root is accepted.
/// Windows pipes are already restricted to this user's SID by their DACL.
pub async fn connect_trusted(path: impl AsRef<Path>) -> io::Result<LocalStream> {
    let stream = LocalStream::connect(path).await?;
    #[cfg(unix)]
    {
        let peer = stream.peer_cred()?.uid();
        // Both ends stay open: macOS getpeereid fails on a closed pair.
        let (own, _peer) = tokio::net::UnixStream::pair()?;
        let me = own.peer_cred()?.uid();
        // A root core (sudo/TUN) may talk to cores of the invoking user.
        if peer != me && peer != 0 && me != 0 {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("IPC endpoint is served by another local user (uid {peer}); refusing it"),
            ));
        }
    }
    Ok(stream)
}

/// Kernel-reported process at the other end; never trust a PID in a reply alone.
pub fn peer_process_id(stream: &LocalStream) -> io::Result<u32> {
    #[cfg(unix)]
    {
        use std::os::fd::AsRawFd;
        let peer = crate::unix_handoff::peer_credentials(stream.as_raw_fd())?;
        u32::try_from(peer.pid)
            .ok()
            .filter(|pid| *pid != 0)
            .ok_or_else(|| io::Error::other("IPC peer process identity unavailable"))
    }
    #[cfg(windows)]
    {
        stream.peer_process_id()
    }
}

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
                SetNamedSecurityInfoW, SetSecurityInfo, SE_FILE_OBJECT,
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
    static UNELEVATED_CLIENTS: std::sync::atomic::AtomicBool =
        std::sync::atomic::AtomicBool::new(false);
    /// Called by a UAC-launched TUN core: it is controlled by its non-elevated
    /// launcher by design, so its endpoints keep the default (medium) label.
    pub fn allow_unelevated_clients() {
        UNELEVATED_CLIENTS.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    /// Any other elevated process labels its pipes High integrity, so a
    /// non-elevated process of the same user cannot drive it (UAC boundary).
    static ELEVATED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    fn high_integrity_only() -> bool {
        !UNELEVATED_CLIENTS.load(std::sync::atomic::Ordering::Relaxed)
            && *ELEVATED.get_or_init(|| crate::elevation::is_elevated().unwrap_or(false))
    }
    fn descriptor() -> io::Result<Descriptor> {
        descriptor_with(false)
    }
    fn descriptor_with(label: bool) -> io::Result<Descriptor> {
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
            let integrity = if label { "S:(ML;;NWNR;;;HI)" } else { "" };
            let sddl: Vec<u16> = format!("D:P(A;OICI;GA;;;{sid_string}){integrity}\0")
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
        let mut name: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        unsafe {
            let (mut present, mut defaulted, mut acl) = (0, 0, std::ptr::null_mut());
            if GetSecurityDescriptorDacl(descriptor.0, &mut present, &mut acl, &mut defaulted) == 0
            {
                return Err(io::Error::last_os_error());
            }
            let information = DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION;
            let error =
                if *ELEVATED.get_or_init(|| crate::elevation::is_elevated().unwrap_or(false)) {
                    // An elevated core gets this path from the unelevated user;
                    // set the ACL through a checked handle so a planted junction
                    // or link cannot hand the user a protected file.
                    let object = crate::elevation::open_user_object(
                        path,
                        windows_sys::Win32::Storage::FileSystem::WRITE_DAC,
                    )?;
                    SetSecurityInfo(
                        object.0,
                        SE_FILE_OBJECT,
                        information,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        acl,
                        std::ptr::null_mut(),
                    )
                } else {
                    SetNamedSecurityInfoW(
                        name.as_mut_ptr(),
                        SE_FILE_OBJECT,
                        information,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        acl,
                        std::ptr::null_mut(),
                    )
                };
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
        let descriptor = descriptor_with(high_integrity_only())?;
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
        marker: std::fs::File,
    }
    fn remove_marker(file: &std::fs::File) -> io::Result<()> {
        use windows_sys::Win32::Storage::FileSystem::{
            FileDispositionInfo, SetFileInformationByHandle, FILE_DISPOSITION_INFO,
        };
        let info = FILE_DISPOSITION_INFO { DeleteFile: true };
        if unsafe {
            SetFileInformationByHandle(
                file.as_raw_handle(),
                FileDispositionInfo,
                (&info as *const FILE_DISPOSITION_INFO).cast(),
                std::mem::size_of_val(&info) as u32,
            )
        } == 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    fn marker_options() -> std::fs::OpenOptions {
        use std::os::windows::fs::OpenOptionsExt;
        use windows_sys::Win32::{
            Foundation::{GENERIC_READ, GENERIC_WRITE},
            Storage::FileSystem::{
                DELETE, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_READ, FILE_SHARE_WRITE,
            },
        };
        let mut options = std::fs::OpenOptions::new();
        options
            .read(true)
            .write(true)
            .access_mode(GENERIC_READ | GENERIC_WRITE | DELETE)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        options
    }
    pub(super) fn recover_stale_manager_endpoint(path: &Path, force: bool) -> io::Result<bool> {
        let marker = endpoint_path(path);
        let metadata = match std::fs::symlink_metadata(&marker) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                // A missing marker is not proof the pipe has gone away. Force
                // callers may discard process records after this succeeds.
                if force {
                    let _reservation = server(&pipe_name(&marker)?, true)?;
                }
                return Ok(false);
            }
            Err(error) => return Err(error),
        };
        if !metadata.file_type().is_file() || metadata.len() != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "IPC endpoint is not an empty regular marker file",
            ));
        }
        // Hold an exclusive deletion right so replacement/rename cannot race
        // ownership checks, and remove the checked object through its handle.
        let marker_handle = marker_options().open(&marker)?;
        let checked = marker_handle.metadata()?;
        if !checked.is_file() || checked.len() != 0 {
            return Err(io::Error::other("IPC marker changed during recovery"));
        }
        use windows_sys::Win32::Storage::FileSystem::{
            GetFileInformationByHandle, BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT,
        };
        let mut information: BY_HANDLE_FILE_INFORMATION = unsafe { std::mem::zeroed() };
        if unsafe { GetFileInformationByHandle(marker_handle.as_raw_handle(), &mut information) }
            == 0
        {
            return Err(io::Error::last_os_error());
        }
        if information.nNumberOfLinks != 1
            || information.dwFileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "IPC marker links are preserved",
            ));
        }
        // Force relaxes only the abandoned marker's owner, never its type,
        // links or liveness. Keep the checked parent fixed until deletion.
        let _parent = if force {
            Some(crate::elevation::force_recovery_parent(
                &marker_handle,
                &marker,
            )?)
        } else {
            crate::elevation::validate_user_file(&marker_handle, &marker)?;
            None
        };
        let reservation = server(&pipe_name(&marker)?, true)?;
        remove_marker(&marker_handle)?;
        drop(reservation);
        drop(marker_handle);
        Ok(true)
    }
    #[cfg(test)]
    include!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/test/platform_windows_test.rs"
    ));
    impl LocalListener {
        pub fn bind(path: impl AsRef<Path>) -> io::Result<Self> {
            use std::os::windows::fs::OpenOptionsExt;
            use windows_sys::Win32::{
                Foundation::{GENERIC_READ, GENERIC_WRITE},
                Storage::FileSystem::{DELETE, WRITE_OWNER},
            };
            let marker = endpoint_path(path);
            let name = pipe_name(&marker)?;
            // Preserve Unix behavior: existing files/endpoints are never replaced.
            let file = marker_options()
                .access_mode(GENERIC_READ | GENERIC_WRITE | DELETE | WRITE_OWNER)
                .create_new(true)
                .open(&marker)?;
            let result = crate::elevation::set_new_marker_owner(&file, &marker)
                .and_then(|()| set_private(&marker))
                .and_then(|()| server(&name, true));
            match result {
                Ok(server) => Ok(Self {
                    server: Mutex::new(server),
                    name,
                    marker: file,
                }),
                Err(error) => {
                    let _ = remove_marker(&file);
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
            let _ = remove_marker(&self.marker);
        }
    }
    pub enum LocalStream {
        Client(NamedPipeClient),
        Server(NamedPipeServer),
    }
    impl LocalStream {
        pub(super) fn peer_process_id(&self) -> io::Result<u32> {
            use windows_sys::Win32::System::Pipes::{
                GetNamedPipeClientProcessId, GetNamedPipeServerProcessId,
            };
            let mut pid = 0;
            let ok = unsafe {
                match self {
                    Self::Client(stream) => {
                        GetNamedPipeServerProcessId(stream.as_raw_handle(), &mut pid)
                    }
                    Self::Server(stream) => {
                        GetNamedPipeClientProcessId(stream.as_raw_handle(), &mut pid)
                    }
                }
            };
            if ok == 0 || pid == 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(pid)
            }
        }
        pub async fn connect(path: impl AsRef<Path>) -> io::Result<Self> {
            let name = pipe_name(&endpoint_path(path))?;
            loop {
                match ClientOptions::new().open(&name) {
                    Ok(client) => return Ok(Self::Client(client)),
                    Err(error) if error.raw_os_error() == Some(231) => {
                        tokio::time::sleep(std::time::Duration::from_millis(10)).await
                    }
                    Err(error) if error.raw_os_error() == Some(5) => {
                        return Err(io::Error::new(
                            io::ErrorKind::PermissionDenied,
                            "access denied: the endpoint belongs to an elevated (administrator) \
                             process or another account; run this tool the same way",
                        ))
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
#[path = "test/platform_test.rs"]
mod tests;

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
