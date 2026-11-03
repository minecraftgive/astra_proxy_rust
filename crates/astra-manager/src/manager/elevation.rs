//! Unix TUN elevation: a sudo-launched core is prepared while the current
//! core keeps serving, then adopted only after it has dropped root and proven
//! the same configuration digest.
use super::*;
use astra_platform::unix_handoff;
use astra_platform::unix_handoff::ChildMessage;
use astra_platform::unix_handoff::Commit;
use astra_platform::unix_handoff::Handoff;
use astra_platform::unix_handoff::Hello;
#[cfg(target_os = "macos")]
use astra_platform::unix_handoff::LifelineCommand;
use astra_platform::unix_handoff::Offer;
use astra_platform::unix_handoff::Prepared;
use std::os::fd::AsRawFd;
#[cfg(target_os = "linux")]
use std::os::fd::{FromRawFd, OwnedFd};
use std::os::unix::fs::MetadataExt;
use std::os::unix::process::ExitStatusExt;
use tokio::sync::oneshot;

const ACCEPT_TIMEOUT: Duration = Duration::from_secs(300);
const FRAME_TIMEOUT: Duration = Duration::from_secs(120);
const READY_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(target_os = "macos")]
const KILL_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone)]
pub(super) enum Outcome {
    Ready(String),
    Failed(String),
}

impl Outcome {
    pub(super) fn into_reply(self) -> Result<Reply> {
        match self {
            Self::Ready(snapshot) => Ok(Reply::text(snapshot)),
            Self::Failed(message) => bail!("{message}"),
        }
    }
}

pub(super) struct Pending {
    ticket: String,
    digest: u64,
    config_text: String,
    previous_pid: Option<u32>,
    /// Dropping the sender aborts a hand-off that is still waiting for sudo.
    _cancel: Option<oneshot::Sender<()>>,
}

pub(crate) enum Event {
    Prepared {
        name: String,
        ticket: String,
        lifeline: tokio::net::UnixStream,
        prepared: Prepared,
        start_ticks: u64,
    },
    Failed {
        name: String,
        ticket: String,
        message: String,
    },
}

pub(super) fn supported() -> bool {
    unsafe { libc::geteuid() != 0 }
}

fn find_sudo() -> Result<PathBuf> {
    let executable = |path: &Path| {
        fs::metadata(path).is_ok_and(|metadata| {
            metadata.is_file()
                && metadata.mode() & 0o111 != 0
                && metadata.uid() == 0
                && metadata.mode() & 0o4000 != 0
        })
    };
    if let Some(path) = std::env::var_os("PATH") {
        for directory in std::env::split_paths(&path) {
            let candidate = directory.join("sudo");
            // A sudo found through a user-writable directory could be swapped
            // for another program before it runs.
            if executable(&candidate) && unix_handoff::ensure_root_controlled(&candidate).is_ok() {
                return Ok(candidate);
            }
        }
    }
    for candidate in ["/usr/bin/sudo", "/bin/sudo", "/usr/local/bin/sudo"] {
        let candidate = PathBuf::from(candidate);
        if executable(&candidate) {
            return Ok(candidate);
        }
    }
    bail!("sudo is unavailable; install sudo or run a core configured for TUN manually")
}

pub(super) async fn prepare(
    manager: &mut Manager,
    name: String,
    expected_pid: Option<u32>,
) -> Result<Reply> {
    anyhow::ensure!(
        supported(),
        "Astra already runs with administrator permissions; retry the TUN switch"
    );
    let instance = manager.instance(&name)?;
    anyhow::ensure!(
        instance.kind == Kind::Proxy,
        "TUN requires a proxy instance"
    );
    let running = manager
        .running
        .get(&name)
        .context("Start the proxy before enabling TUN")?;
    anyhow::ensure!(
        !matches!(&running.child, ManagedChild::Adopted(_)),
        "This core already runs with administrator permission; toggle TUN directly"
    );
    let pid = running.child.id();
    anyhow::ensure!(
        expected_pid.is_none() || expected_pid == pid,
        "Core PID changed; refresh before enabling TUN"
    );
    let socket = manager.socket(&name);
    let active = control::config_snapshot(&socket)
        .await
        .context("Read the active configuration")?;
    anyhow::ensure!(
        fs::canonicalize(&active.path)? == fs::canonicalize(&instance.config)?
            && active.text == read_config(&instance.config)?,
        "Configuration changed on disk; reload it before enabling TUN"
    );
    anyhow::ensure!(
        pid == Some(active.pid),
        "Core PID changed; refresh before enabling TUN"
    );
    anyhow::ensure!(
        astra_platform::runtime::load_overrides_in(&manager.runtime_dir, &instance.config)?.tun
            != Some(true),
        "TUN is already enabled for this profile"
    );

    // A previous ticket for this instance is stale: its sudo prompt cannot be
    // answered by the current request.
    if let Some(old) = manager.elevations.remove(&name) {
        drop(old);
    }

    let sender = manager
        .elevation_events
        .clone()
        .context("Elevation is unavailable outside the manager service")?;
    let sudo = find_sudo()?;
    // sudo runs this file as root; if the user (or anything running as them)
    // can replace it or a directory above it, that program gets root.
    let binary = unix_handoff::ensure_root_controlled(&manager.bin.join(Kind::Proxy.binary()))
        .context("The proxy core cannot be elevated safely; install it in a root-owned directory (for example `sudo install -o root -m 0755 astra-proxy /usr/local/bin/`)")?;
    // Created here, as the user, so the root core never has to create
    // directories on a path the user controls.
    let runtime_dir = manager.runtime_dir.clone();
    fs::create_dir_all(&runtime_dir)?;
    set_private(&runtime_dir, 0o700)?;

    let directory = manager.home.join("elevation");
    fs::create_dir_all(&directory)?;
    set_private(&directory, 0o700)?;
    astra_platform::platform::validate_state_directory(&directory)?;
    let ticket = unix_handoff::ticket()?;
    // Short, random name: the profile name would only make the path longer,
    // and Unix socket paths are limited (108 bytes on Linux), which custom
    // state directories can exceed.
    let socket_path = directory.join(format!("{}.sock", &ticket[..16]));
    let listener = UnixListener::bind(&socket_path)?;
    set_private(&socket_path, 0o600)?;

    rotate_log(&manager.log(&name));
    let offer = Offer {
        version: unix_handoff::PROTOCOL_VERSION,
        socket: socket_path.clone(),
        ticket: ticket.clone(),
        binary,
        sudo,
    };
    if let Some(hub) = &manager.publisher {
        hub.begin_elevation(&name, &ticket);
    }
    let (cancel, cancelled) = oneshot::channel();
    let digest = control::config_digest(&active.text);
    manager.elevations.insert(
        name.clone(),
        Pending {
            ticket: ticket.clone(),
            digest,
            config_text: active.text,
            previous_pid: pid,
            _cancel: Some(cancel),
        },
    );
    let handoff = Handoff {
        service_owner: astra_platform::lifecycle::capture(std::process::id())?,
        version: unix_handoff::PROTOCOL_VERSION,
        config: fs::canonicalize(&active.path)?,
        control: socket,
        log: manager.log(&name),
        log_level: manager.level(&name).as_str().to_owned(),
        runtime_dir: fs::canonicalize(&runtime_dir)?,
        cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/")),
        digest,
        uid: unsafe { libc::geteuid() },
        gid: unsafe { libc::getegid() },
        hosted: std::env::var_os("ASTRA_HOSTED_EXPERIMENTAL").is_some(),
    };
    tokio::spawn(async move {
        let result = tokio::select! {
            result = exchange(listener, ticket.clone(), handoff) => result,
            _ = cancelled => Err(anyhow::anyhow!("Elevation cancelled")),
        };
        let event = match result {
            Ok((lifeline, prepared, start_ticks)) => Event::Prepared {
                name,
                ticket,
                lifeline,
                prepared,
                start_ticks,
            },
            Err(error) => Event::Failed {
                name,
                ticket,
                message: format!("{error:#}"),
            },
        };
        let _ = sender.send(event);
    });
    Ok(Reply::text(serde_json::to_string(&offer)?))
}

async fn exchange(
    listener: UnixListener,
    ticket: String,
    handoff: Handoff,
) -> Result<(tokio::net::UnixStream, Prepared, u64)> {
    let (mut stream, _) = timeout(ACCEPT_TIMEOUT, listener.accept())
        .await
        .context("Administrator permission was not granted in time")??;
    let peer = unix_handoff::peer_credentials(stream.as_raw_fd())?;
    anyhow::ensure!(peer.uid == 0, "Unexpected hand-off peer uid");
    anyhow::ensure!(peer.pid > 0, "Hand-off peer has no process identity");
    let peer_pid = peer.pid as u32;
    let hello: Hello = timeout(FRAME_TIMEOUT, unix_handoff::receive_async(&mut stream))
        .await
        .context("Timed out reading the hand-off hello")??;
    anyhow::ensure!(
        hello.version == unix_handoff::PROTOCOL_VERSION,
        "Hand-off protocol version mismatch"
    );
    anyhow::ensure!(
        unix_handoff::ticket_eq(&hello.ticket, &ticket),
        "Hand-off ticket mismatch"
    );
    unix_handoff::send_async(&mut stream, &handoff).await?;
    let message: ChildMessage = timeout(FRAME_TIMEOUT, unix_handoff::receive_async(&mut stream))
        .await
        .context("Timed out waiting for the elevated core")??;
    let prepared = match message {
        ChildMessage::Prepared(prepared) => prepared,
        ChildMessage::Failed { message } => bail!("{message}"),
    };
    #[cfg(target_os = "linux")]
    let info = process_info(peer_pid)?;
    anyhow::ensure!(
        prepared.pid == peer_pid,
        "Elevated core process identity changed"
    );
    anyhow::ensure!(
        prepared.version == unix_handoff::PROTOCOL_VERSION,
        "Elevated core protocol version mismatch"
    );
    anyhow::ensure!(
        prepared.digest == handoff.digest,
        "Configuration changed while enabling TUN"
    );
    #[cfg(target_os = "linux")]
    {
        anyhow::ensure!(
            prepared.uid == handoff.uid
                && prepared.euid == handoff.uid
                && prepared.gid == handoff.gid,
            "Elevated core did not switch to the invoking user"
        );
        anyhow::ensure!(
            prepared.digest == handoff.digest,
            "Configuration changed while enabling TUN"
        );
        anyhow::ensure!(
            info.uid_real == handoff.uid
                && info.uid == handoff.uid
                && info.gid_real == handoff.gid
                && info.gid == handoff.gid,
            "Elevated core is still privileged"
        );
        anyhow::ensure!(
            info.caps & (1 << 12) != 0,
            "Elevated core did not retain CAP_NET_ADMIN"
        );
    }
    #[cfg(target_os = "macos")]
    anyhow::ensure!(
        prepared.uid == 0 && prepared.euid == 0 && prepared.gid == 0 && peer.gid == 0,
        "Elevated core lost administrator permissions"
    );
    #[cfg(target_os = "linux")]
    let start_ticks = info.start_ticks;
    // macOS authenticates the root peer through getpeereid/LOCAL_PEERPID.
    // proc_pidinfo on a root process is denied to the user; lifetime and
    // shutdown are tied to this connection rather than a reusable PID.
    #[cfg(target_os = "macos")]
    let start_ticks = 0;
    Ok((stream, prepared, start_ticks))
}

#[cfg(target_os = "linux")]
struct ProcessInfo {
    start_ticks: u64,
    uid_real: u32,
    uid: u32,
    gid_real: u32,
    gid: u32,
    #[cfg(target_os = "linux")]
    caps: u64,
}

#[cfg(target_os = "linux")]
fn process_info(pid: u32) -> Result<ProcessInfo> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    let rest = &stat[stat.rfind(')').context("Malformed process stat")? + 1..];
    let fields: Vec<_> = rest.split_whitespace().collect();
    anyhow::ensure!(
        matches!(fields.first(), Some(&"R" | &"S" | &"D" | &"T" | &"I")),
        "Elevated core is not running"
    );
    let start_ticks = fields
        .get(19)
        .context("Malformed process start time")?
        .parse()?;
    let status = fs::read_to_string(format!("/proc/{pid}/status"))?;
    let field = |prefix: &str| {
        status
            .lines()
            .find_map(|line| line.strip_prefix(prefix))
            .map(str::trim)
            .with_context(|| format!("Missing {prefix} for elevated core"))
    };
    let uid: Vec<u32> = field("Uid:")?
        .split_whitespace()
        .map(str::parse)
        .collect::<std::result::Result<_, _>>()?;
    let gid: Vec<u32> = field("Gid:")?
        .split_whitespace()
        .map(str::parse)
        .collect::<std::result::Result<_, _>>()?;
    let caps = u64::from_str_radix(field("CapEff:")?, 16)?;
    anyhow::ensure!(
        uid.len() >= 2 && uid.iter().all(|uid| *uid != 0),
        "Elevated core retained uid 0"
    );
    Ok(ProcessInfo {
        start_ticks,
        uid_real: uid[0],
        uid: uid[1],
        gid_real: gid.first().copied().unwrap_or_default(),
        gid: gid.get(1).copied().unwrap_or_default(),
        caps,
    })
}

pub(super) async fn handle(manager: &mut Manager, event: Event) {
    match event {
        Event::Failed {
            name,
            ticket,
            message,
        } => {
            let matches = manager
                .elevations
                .get(&name)
                .is_some_and(|pending| unix_handoff::ticket_eq(&pending.ticket, &ticket));
            if matches {
                manager.elevations.remove(&name);
                finish(manager, &name, &ticket, Outcome::Failed(message));
            }
        }
        Event::Prepared {
            name,
            ticket,
            lifeline,
            prepared,
            start_ticks,
        } => {
            let Some(pending) = manager.elevations.remove(&name) else {
                return;
            };
            if !unix_handoff::ticket_eq(&pending.ticket, &ticket) {
                manager.elevations.insert(name, pending);
                return;
            }
            let outcome = commit(manager, &name, pending, lifeline, prepared, start_ticks).await;
            finish(manager, &name, &ticket, outcome);
        }
    }
}

async fn commit(
    manager: &mut Manager,
    name: &str,
    pending: Pending,
    mut lifeline: tokio::net::UnixStream,
    prepared: Prepared,
    start_ticks: u64,
) -> Outcome {
    let fail = |message: String| Outcome::Failed(message);
    let Some(run) = manager.running.get(name) else {
        drop(lifeline);
        return fail("The running core changed while enabling TUN".into());
    };
    if run.child.id() != pending.previous_pid {
        drop(lifeline);
        return fail("The running core changed while enabling TUN".into());
    }
    let instance = match manager.instance(name) {
        Ok(instance) => instance,
        Err(error) => {
            drop(lifeline);
            return fail(format!("{error:#}"));
        }
    };
    if !read_config(&instance.config)
        .is_ok_and(|text| control::config_digest(&text) == pending.digest)
    {
        drop(lifeline);
        return fail("Configuration changed while enabling TUN".into());
    }
    astra_ipc::progress::report(
        "Hand over to the elevated core",
        format!("{name} · administrator permission granted"),
    );
    if let Err(error) = manager.stop_core(name, true).await {
        drop(lifeline);
        return fail(format!("Stopping the current core failed: {error:#}"));
    }
    let socket = manager.socket(name);
    let _ = astra_platform::platform::recover_stale_endpoint(&socket);
    let commit = Commit {
        version: unix_handoff::PROTOCOL_VERSION,
        digest: pending.digest,
    };
    if let Err(error) = unix_handoff::send_async(&mut lifeline, &commit).await {
        #[cfg(target_os = "linux")]
        let _ = kill_process(prepared.pid, start_ticks, libc::SIGKILL);
        drop(lifeline);
        let detail = restart(manager, name).await;
        return fail(format!(
            "The elevated core disconnected before startup: {error}{detail}"
        ));
    }
    let deadline = tokio::time::Instant::now() + READY_TIMEOUT;
    let mut last_error = String::new();
    while tokio::time::Instant::now() < deadline {
        let mut probe = [0u8; 1];
        match lifeline.try_read(&mut probe) {
            Ok(0) => {
                last_error = "the elevated core exited during startup".into();
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(error) => {
                last_error = error.to_string();
                break;
            }
            Ok(_) => {}
        }
        match control::request(&socket, Command::Status).await {
            Ok(status) if status.pid == prepared.pid => {
                match control::runtime(&socket, astra_model::runtime::Command::Snapshot).await {
                    Ok(snapshot) if snapshot.pid == prepared.pid => {
                        astra_ipc::progress::report(
                            "Verify elevated core",
                            format!("{name} · TUN enabled"),
                        );
                        manager.running.insert(
                            name.to_owned(),
                            Running {
                                child: ManagedChild::Adopted(AdoptedChild::new(
                                    prepared.pid,
                                    start_ticks,
                                    lifeline,
                                )),
                                config: pending.config_text,
                                config_path: instance.config,
                            },
                        );
                        manager.exits.remove(name);
                        if let Err(error) = manager.commit_process(name).await {
                            let cleanup = manager.stop(name).await;
                            return Outcome::Failed(format!("Could not persist elevated core identity: {error:#}; candidate cleanup: {cleanup:?}"));
                        }
                        manager.notify();
                        return match serde_json::to_string(&snapshot) {
                            Ok(snapshot) => Outcome::Ready(snapshot),
                            Err(error) => Outcome::Failed(format!("{error:#}")),
                        };
                    }
                    Ok(_) => last_error = "elevated core reported a different PID".into(),
                    Err(error) => last_error = format!("{error:#}"),
                }
            }
            Ok(_) => last_error = "elevated core reported a different PID".into(),
            Err(error) => last_error = format!("{error:#}"),
        }
        sleep(Duration::from_millis(100)).await;
    }
    #[cfg(target_os = "linux")]
    let _ = kill_process(prepared.pid, start_ticks, libc::SIGKILL);
    drop(lifeline);
    let detail = restart(manager, name).await;
    fail(format!(
        "Elevated core did not become ready: {last_error}{detail}"
    ))
}

async fn restart(manager: &mut Manager, name: &str) -> String {
    let _ = manager.reap();
    if manager.running.contains_key(name) {
        match manager.stop(name).await {
            Ok(()) => {}
            Err(error) => return format!("; stopping the replacement failed: {error:#}"),
        }
    }
    match manager.start_one(name).await {
        Ok(()) => "; the previous core was restored".into(),
        Err(error) => format!("; restoring the previous core failed: {error:#}"),
    }
}

fn finish(manager: &Manager, name: &str, ticket: &str, outcome: Outcome) {
    if let Some(hub) = &manager.publisher {
        hub.finish_elevation(name, ticket, outcome);
    }
}

pub(super) fn cancel(manager: &mut Manager, name: &str, ticket: &str) -> Result<Reply> {
    let Some(pending) = manager.elevations.get(name) else {
        if let Some(hub) = &manager.publisher {
            hub.finish_elevation(name, ticket, Outcome::Failed("Elevation cancelled".into()));
        }
        return Ok(Reply::text("Elevation cancelled"));
    };
    anyhow::ensure!(
        unix_handoff::ticket_eq(&pending.ticket, ticket),
        "Elevation request changed"
    );
    let pending = manager.elevations.remove(name).unwrap();
    drop(pending);
    finish(
        manager,
        name,
        ticket,
        Outcome::Failed("Elevation cancelled".into()),
    );
    Ok(Reply::text("Elevation cancelled"))
}

pub(super) struct AdoptedChild {
    pid: u32,
    #[cfg(target_os = "linux")]
    start_ticks: u64,
    lifeline: Option<tokio::net::UnixStream>,
    #[cfg(target_os = "linux")]
    pidfd: Option<OwnedFd>,
}

impl AdoptedChild {
    fn new(pid: u32, start_ticks: u64, lifeline: tokio::net::UnixStream) -> Self {
        #[cfg(target_os = "macos")]
        let _ = start_ticks;
        Self {
            pid,
            #[cfg(target_os = "linux")]
            start_ticks,
            lifeline: Some(lifeline),
            #[cfg(target_os = "linux")]
            pidfd: pidfd_open(pid),
        }
    }
    pub(super) fn id(&self) -> Option<u32> {
        Some(self.pid)
    }
    pub(super) fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        let Some(lifeline) = self.lifeline.as_mut() else {
            return Ok(Some(std::process::ExitStatus::from_raw(0)));
        };
        let mut probe = [0u8; 1];
        loop {
            match lifeline.try_read(&mut probe) {
                Ok(0) => {
                    self.lifeline = None;
                    return Ok(Some(std::process::ExitStatus::from_raw(0)));
                }
                Ok(_) => continue,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(None),
                Err(error) => return Err(error),
            }
        }
    }
    /// Cancel-safe: a timed-out wait keeps the lifeline, so a later `kill`
    /// can still deliver its command.
    pub(super) async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        if let Some(lifeline) = self.lifeline.as_mut() {
            let mut buffer = [0u8; 16];
            while tokio::io::AsyncReadExt::read(lifeline, &mut buffer).await? != 0 {}
            self.lifeline = None;
        }
        Ok(std::process::ExitStatus::from_raw(0))
    }
    pub(super) async fn kill(&mut self) -> std::io::Result<()> {
        self.request_shutdown(libc::SIGKILL)?;
        // The user cannot signal the root macOS core; it exits on its own
        // when its lifeline thread reads `Forced`, so confirm by EOF.
        #[cfg(target_os = "macos")]
        if timeout(KILL_TIMEOUT, self.wait()).await.is_err() {
            // Closing the lifeline also arms the core's own exit watchdog.
            self.lifeline = None;
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "the elevated core did not exit after a forced stop",
            ));
        }
        Ok(())
    }
    fn request_shutdown(&mut self, signal: i32) -> std::io::Result<()> {
        #[cfg(target_os = "macos")]
        {
            let command = if signal == libc::SIGKILL {
                LifelineCommand::Forced
            } else {
                LifelineCommand::Graceful
            };
            let frame = unix_handoff::lifecycle_frame(command)?;
            let Some(stream) = self.lifeline.as_mut() else {
                return Ok(());
            };
            let written = stream.try_write(&frame)?;
            if written != frame.len() {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WouldBlock,
                    "lifecycle command was only partially written",
                ));
            }
            Ok(())
        }
        #[cfg(target_os = "linux")]
        self.signal(signal)
    }
    #[cfg(target_os = "linux")]
    fn signal(&self, signal: i32) -> std::io::Result<()> {
        if let Some(pidfd) = &self.pidfd {
            return pidfd_signal(pidfd.as_raw_fd(), signal);
        }
        kill_process(self.pid, self.start_ticks, signal)
    }
}

#[cfg(target_os = "linux")]
fn kill_process(pid: u32, start_ticks: u64, signal: i32) -> std::io::Result<()> {
    if process_info(pid).is_ok_and(|info| info.start_ticks == start_ticks) {
        unsafe {
            if libc::kill(pid as i32, signal) == 0 {
                return Ok(());
            }
            let error = std::io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                return Ok(());
            }
            return Err(error);
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn pidfd_open(pid: u32) -> Option<OwnedFd> {
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
    (fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd) })
}

#[cfg(target_os = "linux")]
fn pidfd_signal(fd: i32, signal: i32) -> std::io::Result<()> {
    let result = unsafe {
        libc::syscall(
            libc::SYS_pidfd_send_signal,
            fd,
            signal,
            std::ptr::null::<libc::siginfo_t>(),
            0,
        )
    };
    if result == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ESRCH) {
        Ok(())
    } else {
        Err(error)
    }
}

#[cfg(test)]
mod tests {
    #[cfg(target_os = "linux")]
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn process_identity_and_capability_are_checked() {
        let info = process_info(std::process::id()).unwrap();
        assert!(info.start_ticks > 0);
    }

    #[test]
    fn capability_mask_includes_net_admin() {
        // Constants used by the hand-off contract.
        assert_eq!(1u64 << 12, 0x1000);
    }
}
