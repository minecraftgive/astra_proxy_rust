//! The sudo-launched half of the Unix TUN hand-off.
//!
//! This runs before any Tokio runtime or worker thread exists: the process
//! forks, the parent reports success to sudo only after the child has
//! established the hand-off (and, on Linux, dropped to the invoking user),
//! and the child keeps the hand-off socket as a lifeline.
//!
//! Linux drops to the invoking user and keeps only the capabilities TUN needs.
//! macOS has no capability model: a utun device and its routes require a live
//! root process, so this core stays root for its lifetime and is confined by
//! its relationship with the manager (ticket, kernel peer-uid checks,
//! configuration digest, and the lifeline that stops it when the manager exits).
use anyhow::{bail, Context, Result};
use astra_ipc::unix_handoff::{
    self, ChildMessage, Commit, Handoff, Hello, Prepared, PROTOCOL_VERSION,
};
use std::{
    fs::OpenOptions,
    os::{
        fd::AsRawFd,
        unix::{fs::OpenOptionsExt, net::UnixStream},
    },
    path::{Path, PathBuf},
    time::Duration,
};

#[cfg(target_os = "linux")]
const CAP_NET_BIND_SERVICE: u32 = 10;
#[cfg(target_os = "linux")]
const CAP_NET_ADMIN: u32 = 12;
#[cfg(target_os = "linux")]
const CAP_NET_RAW: u32 = 13;
#[cfg(target_os = "linux")]
const _LINUX_CAPABILITY_VERSION_3: u32 = 0x2008_0522;

#[cfg(target_os = "linux")]
#[repr(C)]
struct CapHeader {
    version: u32,
    pid: i32,
}

#[cfg(target_os = "linux")]
#[repr(C)]
#[derive(Clone, Copy)]
struct CapData {
    effective: u32,
    permitted: u32,
    inheritable: u32,
}

/// Returns `Some` only when this process was launched for a hand-off; the
/// normal path must still construct its Tokio runtime in `main`.
pub fn entry() -> Option<Result<()>> {
    let mut args = std::env::args_os().skip(1);
    if args.next()? != "--unix-elevated" {
        return None;
    }
    Some((|| {
        let socket = PathBuf::from(args.next().context("expected hand-off socket")?);
        let ticket = args
            .next()
            .context("expected hand-off ticket")?
            .into_string()
            .map_err(|_| anyhow::anyhow!("hand-off ticket must be UTF-8"))?;
        anyhow::ensure!(args.next().is_none(), "unexpected hand-off argument");
        run(socket, ticket)
    })())
}

fn run(socket: PathBuf, ticket: String) -> Result<()> {
    anyhow::ensure!(
        unsafe { libc::geteuid() } == 0,
        "The elevated core must be started with sudo"
    );
    let uid = number("SUDO_UID")?;
    let gid = number("SUDO_GID")?;
    anyhow::ensure!(uid != 0, "Refusing to hand off to uid 0");
    let pipes = status_pipe()?;
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        bail!("fork: {}", std::io::Error::last_os_error());
    }
    if pid > 0 {
        parent(pipes[0], pipes[1]);
    }
    child(socket, ticket, uid, gid, pipes[0], pipes[1]);
}

fn parent(read_fd: i32, write_fd: i32) -> ! {
    unsafe {
        libc::close(write_fd);
    }
    let mut status = 1u8;
    loop {
        let read = unsafe { libc::read(read_fd, (&mut status as *mut u8).cast(), 1) };
        if read == 1 || read == 0 {
            break;
        }
        if std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            break;
        }
    }
    unsafe {
        libc::close(read_fd);
        libc::_exit(if status == 0 { 0 } else { 1 });
    }
}

fn child(socket: PathBuf, ticket: String, uid: u32, gid: u32, read_fd: i32, status_fd: i32) -> ! {
    unsafe {
        libc::close(read_fd);
    }
    match child_main(socket, ticket, uid, gid, status_fd) {
        Ok(()) => unsafe { libc::_exit(0) },
        Err(error) => {
            report_status(status_fd, 1);
            eprintln!("Astra TUN elevation failed: {error:#}");
            unsafe { libc::_exit(1) }
        }
    }
}

fn child_main(
    socket_path: PathBuf,
    ticket: String,
    uid: u32,
    gid: u32,
    status_fd: i32,
) -> Result<()> {
    // The exec'd process may already be a process-group leader, which makes
    // setsid fail with EPERM; a new session is only a convenience here.
    unsafe {
        libc::setsid();
    }
    // Never leave a privileged process attached to the terminal that answered
    // the sudo prompt.
    if let Ok(null) = OpenOptions::new().read(true).write(true).open("/dev/null") {
        unsafe {
            libc::dup2(null.as_raw_fd(), libc::STDIN_FILENO);
        }
    }
    let mut stream = UnixStream::connect(&socket_path)
        .with_context(|| format!("connect to {}", socket_path.display()))?;
    stream.set_read_timeout(Some(Duration::from_secs(60)))?;
    stream.set_write_timeout(Some(Duration::from_secs(10)))?;
    let peer = unix_handoff::peer_credentials(stream.as_raw_fd())
        .context("read hand-off peer credentials")?;
    anyhow::ensure!(
        peer.uid == uid && peer.pid > 0,
        "Hand-off endpoint is not the expected manager"
    );
    unix_handoff::send_sync(
        &mut stream,
        &Hello {
            version: PROTOCOL_VERSION,
            ticket,
        },
    )?;
    let handoff: Handoff = unix_handoff::receive_sync(&mut stream)
        .context("read hand-off parameters from the manager")?;
    anyhow::ensure!(
        handoff.version == PROTOCOL_VERSION,
        "hand-off protocol version mismatch"
    );
    anyhow::ensure!(
        handoff.uid == uid && handoff.gid == gid,
        "hand-off target user does not match sudo"
    );

    #[cfg(target_os = "linux")]
    drop_privileges(uid, gid)?;
    #[cfg(target_os = "macos")]
    {
        // macOS cannot drop this process to the user without losing the utun
        // device and route ownership it needs for its whole lifetime.
        anyhow::ensure!(
            unsafe { libc::getuid() } == 0
                && unsafe { libc::geteuid() } == 0
                && unsafe { libc::getgid() } == 0
                && unsafe { libc::getegid() } == 0,
            "The elevated core must run as root on macOS"
        );
        // Files this root process creates (control socket, caches, logs) live
        // in the user's state directory; hand them back to the invoking user.
        std::env::set_var(
            "ASTRA_ELEVATED_OWNER",
            format!("{uid}:{gid}"),
        );
    }
    unsafe {
        libc::umask(0o077);
    }
    std::env::set_current_dir(&handoff.cwd)?;
    std::env::set_var("ASTRA_CONTROL_SOCKET", &handoff.control);
    std::env::set_var("ASTRA_LOG_LEVEL", &handoff.log_level);
    std::env::set_var("ASTRA_RUNTIME_DIR", &handoff.runtime_dir);
    // The elevated core turns TUN on for this process only; the switch is not
    // persisted, so a later unprivileged restart does not fail trying to
    // create a TUN device it cannot own.
    crate::ELEVATED_TUN.store(true, std::sync::atomic::Ordering::SeqCst);

    let text = read_limited(&handoff.config)?;
    anyhow::ensure!(
        astra_ipc::control::config_digest(&text) == handoff.digest,
        "Configuration changed while enabling TUN"
    );
    let app = crate::App::load(&handoff.config).context("validate the configuration as the user")?;

    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&handoff.log)
        .with_context(|| format!("open {}", handoff.log.display()))?;
    #[cfg(target_os = "macos")]
    unsafe {
        // A log created here would otherwise be root-owned and unreadable to
        // the manager's log view and later log rotation.
        anyhow::ensure!(
            libc::fchown(log.as_raw_fd(), uid, gid) == 0,
            "hand the core log to the invoking user: {}",
            std::io::Error::last_os_error()
        );
        anyhow::ensure!(
            libc::fchmod(log.as_raw_fd(), 0o600) == 0,
            "make the core log private: {}",
            std::io::Error::last_os_error()
        );
    }
    unsafe {
        libc::dup2(log.as_raw_fd(), libc::STDOUT_FILENO);
        libc::dup2(log.as_raw_fd(), libc::STDERR_FILENO);
    }
    drop(log);

    let prepared = Prepared {
        version: PROTOCOL_VERSION,
        pid: std::process::id(),
        uid: unsafe { libc::getuid() },
        gid: unsafe { libc::getgid() },
        euid: unsafe { libc::geteuid() },
        digest: handoff.digest,
    };
    #[cfg(target_os = "linux")]
    {
        anyhow::ensure!(
            prepared.euid == uid && prepared.uid == uid,
            "privilege drop did not take effect"
        );
        anyhow::ensure!(
            prepared.gid == gid,
            "privilege drop did not change the group"
        );
    }
    #[cfg(target_os = "macos")]
    anyhow::ensure!(
        prepared.euid == 0 && prepared.uid == 0 && prepared.gid == 0,
        "the elevated core did not retain root"
    );
    unix_handoff::send_sync(&mut stream, &ChildMessage::Prepared(prepared))?;
    report_status(status_fd, 0);

    stream.set_read_timeout(Some(Duration::from_secs(180)))?;
    let commit: Commit = unix_handoff::receive_sync(&mut stream)
        .context("the manager did not commit the elevated core")?;
    anyhow::ensure!(
        commit.version == PROTOCOL_VERSION && commit.digest == handoff.digest,
        "hand-off commit mismatch"
    );
    stream.set_read_timeout(None)?;
    stream.set_nonblocking(true)?;

    let _log = astra_ipc::logging::init()?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let lifeline = tokio::net::UnixStream::from_std(stream)?;
        crate::proxy::runtime::run_with_lifeline(handoff.config, app, lifeline).await
    })
}

fn report_status(fd: i32, value: u8) {
    unsafe {
        let _ = libc::write(fd, (&value as *const u8).cast(), 1);
        libc::close(fd);
    }
}

fn number(name: &str) -> Result<u32> {
    let text = std::env::var(name).with_context(|| format!("{name} is missing; run through sudo"))?;
    let value: u32 = text.parse().with_context(|| format!("{name} is invalid"))?;
    Ok(value)
}

fn read_limited(path: &Path) -> Result<String> {
    use std::io::Read;
    let mut text = String::new();
    std::fs::File::open(path)?
        .take(480 * 1024 + 1)
        .read_to_string(&mut text)?;
    anyhow::ensure!(text.len() <= 480 * 1024, "Configuration exceeds 480 KiB");
    Ok(text)
}

/// The parent reports to sudo through this pipe only after the child is ready
/// or has failed; it must not survive into helper processes.
fn status_pipe() -> Result<[i32; 2]> {
    let mut pipes = [0i32; 2];
    anyhow::ensure!(
        unsafe { libc::pipe(pipes.as_mut_ptr()) } == 0,
        "create hand-off status pipe: {}",
        std::io::Error::last_os_error()
    );
    for fd in pipes {
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } != 0 {
            let error = std::io::Error::last_os_error();
            unsafe {
                libc::close(pipes[0]);
                libc::close(pipes[1]);
            }
            return Err(error).context("mark the hand-off status pipe close-on-exec");
        }
    }
    Ok(pipes)
}

#[cfg(target_os = "linux")]
fn capset(effective: u64, permitted: u64, inheritable: u64) -> Result<()> {
    let header = CapHeader {
        version: _LINUX_CAPABILITY_VERSION_3,
        pid: 0,
    };
    let data = [
        CapData {
            effective: effective as u32,
            permitted: permitted as u32,
            inheritable: inheritable as u32,
        },
        CapData {
            effective: (effective >> 32) as u32,
            permitted: (permitted >> 32) as u32,
            inheritable: (inheritable >> 32) as u32,
        },
    ];
    let result = unsafe { libc::syscall(libc::SYS_capset, &header, data.as_ptr()) };
    anyhow::ensure!(
        result == 0,
        "capset: {}",
        std::io::Error::last_os_error()
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn has_net_admin() -> bool {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status
                .lines()
                .find_map(|line| line.strip_prefix("CapEff:\t"))
                .and_then(|hex| u64::from_str_radix(hex.trim(), 16).ok())
        })
        .is_some_and(|caps| caps & (1 << CAP_NET_ADMIN) != 0)
}

/// Drop to the invoking user while retaining only the capabilities needed to
/// create the TUN device, install routes, and bind privileged ports. The
/// ambient set survives `execve` for the `ip` helper as well.
#[cfg(target_os = "linux")]
fn drop_privileges(uid: u32, gid: u32) -> Result<()> {
    let mask = (1u64 << CAP_NET_BIND_SERVICE) | (1u64 << CAP_NET_ADMIN) | (1u64 << CAP_NET_RAW);
    unsafe {
        anyhow::ensure!(
            libc::prctl(libc::PR_SET_KEEPCAPS, 1 as libc::c_ulong, 0, 0, 0) == 0,
            "PR_SET_KEEPCAPS: {}",
            std::io::Error::last_os_error()
        );
        anyhow::ensure!(
            libc::setgroups(0, std::ptr::null()) == 0,
            "setgroups: {}",
            std::io::Error::last_os_error()
        );
        anyhow::ensure!(
            libc::setresgid(gid, gid, gid) == 0,
            "setresgid: {}",
            std::io::Error::last_os_error()
        );
        anyhow::ensure!(
            libc::setresuid(uid, uid, uid) == 0,
            "setresuid: {}",
            std::io::Error::last_os_error()
        );
    }
    capset(mask, mask, mask)?;
    for capability in [CAP_NET_BIND_SERVICE, CAP_NET_ADMIN, CAP_NET_RAW] {
        let result = unsafe {
            libc::prctl(
                libc::PR_CAP_AMBIENT,
                libc::PR_CAP_AMBIENT_RAISE as libc::c_ulong,
                capability as libc::c_ulong,
                0,
                0,
            )
        };
        anyhow::ensure!(
            result == 0,
            "raise ambient capability {capability}: {}",
            std::io::Error::last_os_error()
        );
    }
    unsafe {
        anyhow::ensure!(
            libc::prctl(
                libc::PR_SET_NO_NEW_PRIVS,
                1 as libc::c_ulong,
                0,
                0,
                0
            ) == 0,
            "PR_SET_NO_NEW_PRIVS: {}",
            std::io::Error::last_os_error()
        );
        anyhow::ensure!(
            libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0) == 0,
            "PR_SET_DUMPABLE: {}",
            std::io::Error::last_os_error()
        );
    }
    anyhow::ensure!(has_net_admin(), "CAP_NET_ADMIN was not retained");
    Ok(())
}
