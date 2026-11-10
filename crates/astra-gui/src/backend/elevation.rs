//! Interactive elevation preparation and cleanup, outside the GPUI executor.
#[cfg(unix)]
use anyhow::Context as _;
use anyhow::Result;
use astra_client::{
    i18n::t,
    installation,
    protocol::{self, Action},
};
#[cfg(test)]
use astra_management::v2 as v;
use std::{
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex,
    },
};

#[cfg(unix)]
struct Launched(std::process::Child);
#[cfg(unix)]
impl Drop for Launched {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

static CONTROLS: Mutex<Vec<std::sync::Weak<Control>>> = Mutex::new(Vec::new());
static PENDING: AtomicUsize = AtomicUsize::new(0);
struct Pending;
impl Pending {
    fn new() -> Self {
        PENDING.fetch_add(1, Ordering::AcqRel);
        Self
    }
}
impl Drop for Pending {
    fn drop(&mut self) {
        PENDING.fetch_sub(1, Ordering::AcqRel);
    }
}
/// Runtime teardown must leave time for a late Prepare reply to be revoked.
/// Private IPC failures are bounded so a dead manager cannot hang application exit.
pub async fn shutdown() {
    let controls = CONTROLS
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .iter()
        .filter_map(std::sync::Weak::upgrade)
        .collect::<Vec<_>>();
    for control in controls {
        control.cancel();
    }
    let _ = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while PENDING.load(Ordering::Acquire) != 0 {
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    })
    .await;
}

pub enum Request {
    Tun {
        name: String,
        expected_pid: Option<u32>,
    },
    Install(installation::Request),
}
pub struct Prepared {
    pub warning: Option<String>,
    program: PathBuf,
    arguments: Vec<String>,
    #[cfg(unix)]
    sudo: PathBuf,
    completion: Completion,
}
enum Completion {
    #[cfg(unix)]
    Tun {
        name: String,
        ticket: String,
    },
    Install(installation::Request),
}
pub enum Outcome {
    #[cfg(unix)]
    Tun {
        name: String,
        snapshot: astra_model::runtime::Snapshot,
    },
    Install(installation::Request),
}
struct Ticket {
    name: String,
    ticket: String,
}
/// The host owns this guard while a request is visible. Even cancellation before
/// Prepare replies is remembered, so a late offer is always revoked.
pub struct Control {
    cancelled: AtomicBool,
    released: AtomicBool,
    active: Arc<AtomicBool>,
    ticket: Mutex<Option<Ticket>>,
    handle: tokio::runtime::Handle,
    socket: PathBuf,
}
impl Control {
    pub fn new(handle: tokio::runtime::Handle, socket: PathBuf, active: Arc<AtomicBool>) -> Self {
        Self {
            cancelled: AtomicBool::new(false),
            released: AtomicBool::new(false),
            active,
            ticket: Mutex::new(None),
            handle,
            socket,
        }
    }
    pub fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
    fn check(&self) -> Result<()> {
        anyhow::ensure!(!self.cancelled(), "{}", t("Cancelled"));
        Ok(())
    }
    fn revoke(&self, ticket: Ticket) {
        let socket = self.socket.clone();
        let pending = Pending::new();
        self.handle.spawn(async move {
            let _pending = pending;
            let _ = protocol::call(
                &socket,
                Action::CancelElevation {
                    name: ticket.name,
                    ticket: ticket.ticket,
                },
            )
            .await;
        });
    }
    #[cfg(any(unix, test))]
    fn register(&self, ticket: Ticket) {
        let mut current = self
            .ticket
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if self.cancelled() {
            self.revoke(ticket);
        } else {
            *current = Some(ticket);
        }
    }
    fn release(&self) {
        if !self.released.swap(true, Ordering::AcqRel) {
            self.active.store(false, Ordering::Release);
        }
    }
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.release();
        if let Some(ticket) = self
            .ticket
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
        {
            self.revoke(ticket);
        }
    }
    fn complete(&self) {
        self.ticket
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
        self.release();
    }
}
impl Drop for Control {
    fn drop(&mut self) {
        self.cancel();
    }
}

pub async fn prepare(request: Request, control: Arc<Control>) -> Result<Prepared> {
    let _pending = Pending::new();
    {
        let mut controls = CONTROLS.lock().unwrap_or_else(|error| error.into_inner());
        controls.retain(|control| control.strong_count() != 0);
        controls.push(Arc::downgrade(&control));
    }
    let prepared = prepare_inner(request, &control).await;
    if prepared.is_err() || control.cancelled() {
        control.cancel();
    }
    control.check()?;
    prepared
}
async fn prepare_inner(request: Request, control: &Control) -> Result<Prepared> {
    anyhow::ensure!(
        protocol::target().is_none_or(|(_, agent)| agent.is_none()),
        "{}",
        t("Run installation on the target machine.")
    );
    control.check()?;
    match request {
        Request::Tun { name, expected_pid } => {
            #[cfg(unix)]
            {
                let reply = protocol::call(
                    &control.socket,
                    Action::PrepareElevation {
                        name: name.clone(),
                        expected_pid,
                    },
                )
                .await?;
                let offer: astra_platform::unix_handoff::Offer = serde_json::from_str(&reply.text)?;
                control.register(Ticket {
                    name: name.clone(),
                    ticket: offer.ticket.clone(),
                });
                control.check()?;
                anyhow::ensure!(
                    offer.version == astra_platform::unix_handoff::PROTOCOL_VERSION,
                    "Unsupported elevation protocol"
                );
                // Keep the exact trust checks used by the terminal frontend.
                let program = astra_platform::unix_handoff::inspect_program(&offer.binary)?;
                let sudo = astra_platform::unix_handoff::ensure_root_controlled(&offer.sudo)?;
                let socket = offer
                    .socket
                    .to_str()
                    .context("Invalid elevation socket path")?
                    .to_owned();
                Ok(Prepared {
                    warning: program.warning,
                    program: program.path,
                    sudo,
                    arguments: vec!["--unix-elevated".into(), socket, offer.ticket.clone()],
                    completion: Completion::Tun {
                        name,
                        ticket: offer.ticket,
                    },
                })
            }
            #[cfg(not(unix))]
            {
                let _ = (name, expected_pid);
                anyhow::bail!("{}", t("TUN elevation is only available on Linux"));
            }
        }
        Request::Install(request) => {
            let launch = installation::prepare_elevation(&control.socket, &request).await?;
            control.check()?;
            #[cfg(unix)]
            {
                let program = astra_platform::unix_handoff::inspect_program(&launch.program)?;
                let sudo = ["/usr/bin/sudo", "/bin/sudo", "/usr/local/bin/sudo"].into_iter()
                    .find_map(|path| astra_platform::unix_handoff::ensure_root_controlled(std::path::Path::new(path)).ok())
                    .context(t("sudo is unavailable. Run installation on the target machine with administrator permission."))?;
                Ok(Prepared {
                    warning: program.warning,
                    program: program.path,
                    sudo,
                    arguments: launch.arguments,
                    completion: Completion::Install(request),
                })
            }
            #[cfg(not(unix))]
            {
                let program = std::fs::canonicalize(launch.program)?;
                anyhow::ensure!(
                    program.is_file(),
                    "Installation helper is not a regular file"
                );
                Ok(Prepared {
                    warning: None,
                    program,
                    arguments: launch.arguments,
                    completion: Completion::Install(request),
                })
            }
        }
    }
}

pub async fn execute(prepared: Prepared, control: Arc<Control>) -> Result<Outcome> {
    let _pending = Pending::new();
    let result = execute_inner(prepared, control.clone()).await;
    if result.is_err() || control.cancelled() {
        control.cancel();
    } else {
        control.complete();
    }
    control.check()?;
    result
}
async fn execute_inner(prepared: Prepared, control: Arc<Control>) -> Result<Outcome> {
    control.check()?;
    let completion = prepared.completion;
    let worker = control.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        worker.check()?;
        #[cfg(unix)]
        {
            // Re-check after the warning dialog; trust may have changed meanwhile.
            let program = astra_platform::unix_handoff::inspect_program(&prepared.program)?;
            let sudo = astra_platform::unix_handoff::ensure_root_controlled(&prepared.sudo)?;
            anyhow::ensure!(
                program.path == prepared.program && program.warning == prepared.warning,
                "Program trust changed; request administrator permission again"
            );
            let executable = std::env::current_exe()?;
            let mut command = std::process::Command::new(sudo);
            command
                .arg("-A")
                .arg(&program.path)
                .args(&prepared.arguments)
                .env("SUDO_ASKPASS", executable)
                .env("ASTRA_GUI_ASKPASS", "1")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null());
            worker.check()?;
            let mut child = Launched(command.spawn()?);
            loop {
                if worker.cancelled() {
                    let _ = child.0.kill();
                    let _ = child.0.wait();
                    anyhow::bail!("{}", t("Cancelled"));
                }
                if let Some(status) = child.0.try_wait()? {
                    anyhow::ensure!(
                        status.success(),
                        "{}",
                        t("Administrator permission was not granted")
                    );
                    break;
                }
                std::thread::sleep(std::time::Duration::from_millis(50));
            }
        }
        #[cfg(windows)]
        {
            // The operating system owns the UAC consent UI. This remains the
            // existing installation helper, never the TUN service path.
            astra_client::windows::installation::elevate(&prepared.program, &prepared.arguments)?;
        }
        worker.check()
    })
    .await??;
    control.check()?;
    match completion {
        #[cfg(unix)]
        Completion::Tun { name, ticket } => {
            let reply = protocol::call(
                &control.socket,
                Action::WaitElevation {
                    name: name.clone(),
                    ticket,
                },
            )
            .await?;
            control.check()?;
            let snapshot = serde_json::from_str(&reply.text)?;
            Ok(Outcome::Tun { name, snapshot })
        }
        Completion::Install(mut request) => {
            request.elevate = false;
            Ok(Outcome::Install(request))
        }
    }
}

#[cfg(test)]
#[path = "../test/elevation_backend_test.rs"]
mod tests;
