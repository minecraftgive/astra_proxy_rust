//! Workers never inherit the resident host's network-administration rights.
use anyhow::{Context, Result};
use std::{
    io,
    path::Path,
    process::{ExitStatus, Stdio},
    time::Duration,
};
use tokio::io::AsyncWriteExt;
#[cfg(windows)]
#[path = "worker_windows.rs"]
mod windows;

/// Resident and worker endpoints must live in an existing private state
/// directory. Validate it without changing permissions on a caller's path
/// (especially a shared directory such as /tmp).
pub fn validate_endpoint_directory(directory: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(directory).with_context(|| {
        format!(
            "inspect resident endpoint directory {}",
            directory.display()
        )
    })?;
    anyhow::ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "resident endpoint directory must be a real directory, not a link: {}",
        directory.display()
    );
    #[cfg(unix)]
    {
        let uid = invoking_user()?.map_or_else(|| unsafe { libc::geteuid() }, |owner| owner.0);
        validate_unix_directory(directory, uid)?;
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        anyhow::ensure!(
            metadata.file_attributes()
                & windows_sys::Win32::Storage::FileSystem::FILE_ATTRIBUTE_REPARSE_POINT
                == 0,
            "resident endpoint directory cannot be a link or junction"
        );
        // Windows state directories and each pipe already use the current-user
        // SID ACL through platform::set_private. Preserve that existing policy;
        // Unix ownership/mode rules do not apply to NTFS permissions.
    }
    Ok(())
}

/// Normalize sudo's verified owner before sockets or worker threads exist.
///
/// # Safety
/// Call only from the single-threaded executable bootstrap, before creating a
/// Tokio runtime or any other threads that could inspect the environment.
#[cfg(unix)]
pub(crate) unsafe fn initialize_owner() -> Result<()> {
    if std::env::var_os("ASTRA_HOSTED_EXPERIMENTAL").is_some() {
        if let Some((uid, gid)) = invoking_user()? {
            // SAFETY: the caller guarantees single-threaded process bootstrap.
            unsafe {
                std::env::set_var("ASTRA_ELEVATED_OWNER", format!("{uid}:{gid}"));
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn invoking_user() -> Result<Option<(u32, u32)>> {
    if unsafe { libc::geteuid() } != 0 {
        return Ok(None);
    }
    let owner = std::env::var("ASTRA_ELEVATED_OWNER").ok();
    let uid = owner
        .as_deref()
        .and_then(|s| s.split(':').next())
        .map(str::to_owned)
        .or_else(|| std::env::var("SUDO_UID").ok())
        .context("resident host requires an invoking user for worker privilege separation")?
        .parse::<u32>()?;
    let gid = owner
        .as_deref()
        .and_then(|s| s.split(':').nth(1))
        .map(str::to_owned)
        .or_else(|| std::env::var("SUDO_GID").ok())
        .context("missing invoking group")?
        .parse::<u32>()?;
    anyhow::ensure!(uid != 0, "worker owner must not be root");
    Ok(Some((uid, gid)))
}

#[cfg(unix)]
fn validate_unix_directory(directory: &Path, uid: u32) -> Result<()> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::symlink_metadata(directory)?;
    anyhow::ensure!(
        metadata.is_dir() && !metadata.file_type().is_symlink(),
        "resident endpoint directory cannot be a link"
    );
    anyhow::ensure!(
        metadata.uid() == uid && metadata.mode() & 0o7777 == 0o700,
        "resident endpoint directory must be owned by uid {uid} with mode 0700: {}",
        directory.display()
    );
    // A private child can still be renamed by an untrusted owner of an
    // ancestor. Root-owned sticky /tmp is safe; a shared non-sticky parent is
    // not. Trusted system aliases such as macOS /tmp -> /private/tmp are fine.
    let absolute = std::path::absolute(directory)?;
    for ancestor in absolute.ancestors().skip(1) {
        let metadata = std::fs::symlink_metadata(ancestor)?;
        anyhow::ensure!(
            metadata.uid() == 0 || metadata.uid() == uid,
            "resident endpoint ancestor is owned by another user: {}",
            ancestor.display()
        );
        if metadata.file_type().is_symlink() {
            continue;
        }
        anyhow::ensure!(
            metadata.mode() & 0o022 == 0 || metadata.mode() & 0o1000 != 0,
            "resident endpoint ancestor is writable by other users without sticky protection: {}",
            ancestor.display()
        );
    }
    for ancestor in std::fs::canonicalize(directory)?.ancestors().skip(1) {
        let metadata = std::fs::metadata(ancestor)?;
        anyhow::ensure!(
            (metadata.uid() == 0 || metadata.uid() == uid)
                && (metadata.mode() & 0o022 == 0 || metadata.mode() & 0o1000 != 0),
            "resident endpoint directory has an untrusted writable ancestor: {}",
            ancestor.display()
        );
    }
    Ok(())
}

pub enum WorkerChild {
    Normal(Box<tokio::process::Child>),
    #[cfg(windows)]
    Windows(windows::Child),
}
impl WorkerChild {
    pub fn id(&self) -> Option<u32> {
        match self {
            Self::Normal(child) => child.id(),
            #[cfg(windows)]
            Self::Windows(child) => child.id(),
        }
    }
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        match self {
            Self::Normal(child) => child.try_wait(),
            #[cfg(windows)]
            Self::Windows(child) => child.try_wait(),
        }
    }
    pub async fn kill(&mut self) -> io::Result<()> {
        match self {
            Self::Normal(child) => child.kill().await,
            #[cfg(windows)]
            Self::Windows(child) => child.kill().await,
        }
    }
}

/// `config` is the complete bounded stdin frame, never an argument, environment
/// variable or temporary file. Dropping a returned worker terminates it.
pub async fn spawn(binary: &Path, config: Vec<u8>, log: std::fs::File) -> Result<WorkerChild> {
    anyhow::ensure!(
        config.len() <= 1024 * 1024 + 4,
        "worker startup frame too large"
    );
    #[cfg(windows)]
    if astra_platform::elevation::is_elevated()? {
        let (child, stdin) = windows::spawn(binary, &log)?;
        let child = WorkerChild::Windows(child);
        let mut stdin = tokio::fs::File::from_std(stdin);
        tokio::time::timeout(Duration::from_secs(15), async {
            stdin.write_all(&config).await?;
            stdin.flush().await
        })
        .await
        .context("worker startup pipe timed out")??;
        drop(stdin);
        return Ok(child);
    }
    let mut command = command(binary)?;
    command
        .arg("--host-worker")
        .stdin(Stdio::piped())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .with_context(|| format!("start {}", binary.display()))?;
    let mut stdin = child.stdin.take().context("worker stdin unavailable")?;
    tokio::time::timeout(Duration::from_secs(15), stdin.write_all(&config))
        .await
        .context("worker startup pipe timed out")??;
    drop(stdin);
    Ok(WorkerChild::Normal(Box::new(child)))
}

fn command(binary: &Path) -> Result<tokio::process::Command> {
    let mut command = astra_platform::platform::background_core(binary);
    command.env_remove("ASTRA_SERVICE_OWNER");
    for name in REMOVED_ENV {
        command.env_remove(name);
    }
    #[cfg(unix)]
    {
        let owner = invoking_user()?;
        // setgroups must precede setuid. Use only syscalls in this post-fork hook.
        unsafe {
            command.pre_exec(move || {
                if let Some((uid, gid)) = owner {
                    if libc::setgroups(0, std::ptr::null()) != 0
                        || libc::setgid(gid) != 0
                        || libc::setuid(uid) != 0
                    {
                        return Err(io::Error::last_os_error());
                    }
                }
                #[cfg(target_os = "linux")]
                clear_capabilities()?;
                Ok(())
            });
        }
    }
    Ok(command)
}
const REMOVED_ENV: &[&str] = &[
    "ASTRA_SERVICE_OWNER",
    "ASTRA_CONTROL_SOCKET",
    "ASTRA_HOSTED",
    "ASTRA_HOSTED_EXPERIMENTAL",
    "ASTRA_ELEVATED_OWNER",
    "SUDO_UID",
    "SUDO_GID",
    "SUDO_USER",
];

#[cfg(target_os = "linux")]
unsafe fn clear_capabilities() -> io::Result<()> {
    #[repr(C)]
    struct Header {
        version: u32,
        pid: i32,
    }
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct Data {
        effective: u32,
        permitted: u32,
        inheritable: u32,
    }
    let header = Header {
        version: 0x2008_0522,
        pid: 0,
    };
    let data = [Data {
        effective: 0,
        permitted: 0,
        inheritable: 0,
    }; 2];
    // Linux hosts can already have the user's UID while retaining ambient caps.
    if unsafe {
        libc::prctl(
            libc::PR_CAP_AMBIENT,
            libc::PR_CAP_AMBIENT_CLEAR_ALL,
            0,
            0,
            0,
        )
    } != 0
        || unsafe { libc::syscall(libc::SYS_capset, &header, data.as_ptr()) } != 0
        || unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
#[path = "../test/platform_worker_endpoint_directory_test.rs"]
mod endpoint_directory_tests;
