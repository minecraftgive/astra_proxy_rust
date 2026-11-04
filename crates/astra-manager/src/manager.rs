#[cfg(any(target_os = "linux", target_os = "macos"))]
mod elevation;
mod recovery;
mod server;
mod updates;
use crate::protocol::*;
use anyhow::{bail, Context, Result};
use astra_ipc::control;
use astra_model::control::Command;
use astra_platform::platform::set_private;
use astra_platform::platform::DirBuilderExt;
use astra_platform::platform::LocalListener as UnixListener;
use astra_platform::platform::OpenOptionsExt;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    fs::{self, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    process::{Child, Command as Process},
    time::{sleep, timeout},
};

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Registry {
    #[serde(default)]
    instances: Vec<Instance>,
    #[serde(default, skip_serializing)]
    log_levels: HashMap<String, LogLevel>,
    #[serde(default)]
    start_bindings: HashMap<String, Vec<String>>,
}
#[derive(Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct RuntimeState {
    #[serde(default)]
    log_levels: HashMap<String, LogLevel>,
}
struct Running {
    child: ManagedChild,
    config: String,
    config_path: PathBuf,
}
struct OwnedChild {
    child: Child,
    committed: bool,
}
impl OwnedChild {
    fn new(child: Child) -> Self {
        Self {
            child,
            committed: false,
        }
    }
}
impl std::ops::Deref for OwnedChild {
    type Target = Child;
    fn deref(&self) -> &Child {
        &self.child
    }
}
impl std::ops::DerefMut for OwnedChild {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.child
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        if !self.committed {
            let _ = self.child.start_kill();
        }
    }
}
enum ManagedChild {
    Reattached(recovery::Reattached),
    Normal(Box<OwnedChild>),
    #[cfg(windows)]
    Elevated(astra_platform::elevation::ElevatedChild),
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    Adopted(elevation::AdoptedChild),
}
impl ManagedChild {
    fn stop_command(&self) -> Command {
        match self {
            Self::Reattached(child) => Command::StopIdentified {
                boot_id: child.identity.boot_id.clone(),
                force: false,
            },
            _ => Command::Stop,
        }
    }
    fn id(&self) -> Option<u32> {
        match self {
            Self::Reattached(child) => Some(child.identity.process.pid),
            Self::Normal(child) => child.id(),
            #[cfg(windows)]
            Self::Elevated(child) => child.id(),
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            Self::Adopted(child) => child.id(),
        }
    }
    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        match self {
            Self::Reattached(child) => child.try_wait(),
            Self::Normal(child) => child.try_wait(),
            #[cfg(windows)]
            Self::Elevated(child) => child.try_wait(),
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            Self::Adopted(child) => child.try_wait(),
        }
    }
    async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        match self {
            Self::Reattached(child) => child.wait().await,
            Self::Normal(child) => child.wait().await,
            #[cfg(windows)]
            Self::Elevated(child) => child.wait().await,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            Self::Adopted(child) => child.wait().await,
        }
    }
    async fn kill(&mut self) -> std::io::Result<()> {
        match self {
            Self::Reattached(child) => child.kill().await,
            Self::Normal(child) => child.kill().await,
            #[cfg(windows)]
            Self::Elevated(child) => child.kill().await,
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            Self::Adopted(child) => child.kill().await,
        }
    }
}
pub struct Manager {
    updates: updates::Updates,
    publisher: Option<std::sync::Arc<server::Hub>>,
    home: PathBuf,
    config_home: PathBuf,
    runtime_dir: PathBuf,
    runtime: RuntimeState,
    bin: PathBuf,
    registry: Registry,
    running: HashMap<String, Running>,
    blocked: HashMap<String, String>,
    exits: HashMap<String, String>,
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    elevations: HashMap<String, elevation::Pending>,
    elevation_events: Option<tokio::sync::mpsc::UnboundedSender<ElevationEvent>>,
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) type ElevationEvent = elevation::Event;
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
pub(crate) type ElevationEvent = std::convert::Infallible;

pub(crate) async fn apply_elevation(manager: &mut Manager, event: ElevationEvent) {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    elevation::handle(manager, event).await;
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    let _ = (manager, event);
}
const CONFIG_LIMIT: usize = 480 * 1024;

pub(crate) fn validate_name(name: &str) -> Result<()> {
    anyhow::ensure!(
        !name.is_empty()
            && name.len() <= 24
            && !name.eq_ignore_ascii_case("manager")
            && !name.eq_ignore_ascii_case("management")
            && !name.to_ascii_lowercase().starts_with("server-")
            && name.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'),
        "instance name must contain 1..24 ASCII letters, digits, dash or underscore; manager, management and server-* are reserved"
    );
    Ok(())
}

#[cfg(test)]
mod runtime_directory_tests {
    use super::*;
    #[test]
    fn configured_runtime_directories_isolate_state_and_child_environment() {
        let directory = tempfile::tempdir().unwrap();
        let profile = directory.path().join("profile.toml");
        fs::write(&profile, "standalone=true").unwrap();
        let mut managers = Vec::new();
        for name in ["one", "two"] {
            let home = directory.path().join(format!("state-{name}"));
            private_dir(&home).unwrap();
            managers.push(
                Manager::load_with_runtime(
                    home,
                    directory.path().join(format!("config-{name}")),
                    directory.path().into(),
                    Some(directory.path().join(format!("runtime-{name}"))),
                )
                .unwrap(),
            );
        }
        let state = astra_model::runtime::Overrides {
            tun: Some(true),
            ..Default::default()
        };
        let first = astra_platform::storage::runtime_path_in(
            &managers[0].runtime_dir,
            &profile,
            "overrides",
        )
        .unwrap();
        astra_platform::storage::write_json(&first, &state).unwrap();
        assert_eq!(
            astra_platform::runtime::load_overrides_in(&managers[0].runtime_dir, &profile)
                .unwrap()
                .tun,
            Some(true)
        );
        assert_eq!(
            astra_platform::runtime::load_overrides_in(&managers[1].runtime_dir, &profile)
                .unwrap()
                .tun,
            Some(false)
        );
        for manager in managers {
            let process = core_process(manager.bin.join("astra-proxy"), &manager.runtime_dir);
            assert!(process
                .as_std()
                .get_envs()
                .any(|(key, value)| key == "ASTRA_RUNTIME_DIR"
                    && value == Some(manager.runtime_dir.as_os_str())));
        }
    }
}

/// Core logs are appended across restarts; once one passes this size it is
/// moved to `<name>.log.1` (replacing the previous one) when the core starts.
const LOG_ROTATE_BYTES: u64 = 32 * 1024 * 1024;
fn rotate_log(path: &Path) {
    if fs::metadata(path).is_ok_and(|metadata| metadata.len() > LOG_ROTATE_BYTES) {
        let mut old = path.as_os_str().to_owned();
        old.push(".1");
        let _ = fs::remove_file(&old);
        if let Err(error) = fs::rename(path, &old) {
            eprintln!("Log rotation skipped for {}: {error}", path.display());
        }
    }
}

/// The last bytes of a core log, for startup errors shown in the UI.
fn log_tail(path: &Path, limit: u64) -> String {
    let read = || -> std::io::Result<String> {
        let mut file = fs::File::open(path)?;
        let size = file.metadata()?.len();
        file.seek(SeekFrom::Start(size.saturating_sub(limit)))?;
        let mut bytes = Vec::new();
        file.take(limit).read_to_end(&mut bytes)?;
        Ok(String::from_utf8_lossy(&bytes).trim().to_owned())
    };
    read().unwrap_or_else(|_| format!("see {}", path.display()))
}

async fn prepare_control_endpoint(socket: &Path) -> Result<()> {
    if socket.exists() {
        match astra_platform::platform::recover_stale_endpoint(socket) {
            Ok(true) => eprintln!("Recovered stale control endpoint: {}", socket.display()),
            Ok(false) => {}
            Err(error) => {
                if let Ok(status) = control::request(socket, Command::Status).await {
                    bail!("Control endpoint {} belongs to running core PID {}; it is not owned by this manager", socket.display(), status.pid);
                }
                return Err(error).with_context(|| format!(
                    "Control endpoint could not be safely recovered: {}; it may be active or inaccessible; the marker was retained. Use Force start to replace the previous session", socket.display()));
            }
        }
    }
    anyhow::ensure!(
        !socket.exists(),
        "Control socket exists: {}; check for an orphaned core before removing it",
        socket.display()
    );
    Ok(())
}

struct DiagnosticPlan {
    runtime_dir: PathBuf,
    executable: PathBuf,
    config_path: PathBuf,
    text: String,
    running: bool,
    control: Option<(PathBuf, Option<u32>)>,
}
impl DiagnosticPlan {
    async fn refresh(&mut self) -> Result<()> {
        if let Some((socket, pid)) = &self.control {
            let snapshot = control::config_snapshot(socket)
                .await
                .context("Read active configuration")?;
            anyhow::ensure!(
                Some(snapshot.pid) == *pid,
                "Core PID changed; retry diagnostics"
            );
            anyhow::ensure!(
                snapshot.text.len() <= CONFIG_LIMIT,
                "Configuration exceeds 480 KiB"
            );
            self.config_path = snapshot.path;
            self.text = snapshot.text;
        }
        Ok(())
    }
    async fn stream(
        mut self,
        targets: Vec<String>,
        output: &mut (impl tokio::io::AsyncWrite + Unpin),
    ) -> Result<()> {
        use astra_model::diagnostics::Event;
        self.refresh().await?;
        let payload = serde_json::to_vec(&astra_model::diagnostics::Request {
            config: self.text,
            targets: Some(targets),
            list_targets: false,
        })?;
        let running = self.running;
        pipe_events(
            (self.executable, self.runtime_dir),
            "--diagnose-stream",
            self.config_path,
            payload,
            output,
            "Update proxy core to use live node tests",
            |event: &mut Event| {
                if let Event::Started { running_config, .. } = event {
                    *running_config = running;
                }
                matches!(event, Event::Done)
            },
        )
        .await
    }
    async fn nettool(
        mut self,
        mut request: astra_model::nettool::Request,
        output: &mut (impl tokio::io::AsyncWrite + Unpin),
    ) -> Result<()> {
        use astra_model::nettool::Event;
        self.refresh().await?;
        request.config = self.text;
        let payload = serde_json::to_vec(&request)?;
        let running = self.running;
        pipe_events(
            (self.executable, self.runtime_dir),
            "--nettool-stream",
            self.config_path,
            payload,
            output,
            "Update proxy core to use network tools",
            |event: &mut Event| {
                if let Event::Started { running_config, .. } = event {
                    *running_config = running;
                }
                matches!(event, Event::Done)
            },
        )
        .await
    }
    async fn run(self) -> Result<Reply> {
        self.run_selection(None, false).await
    }
    async fn run_selection(self, targets: Option<Vec<String>>, list: bool) -> Result<Reply> {
        self.run_job(targets, list, false).await
    }
    async fn run_refresh(self) -> Result<Reply> {
        self.run_job(None, false, true).await
    }
    async fn run_job(
        mut self,
        targets: Option<Vec<String>>,
        list: bool,
        refresh: bool,
    ) -> Result<Reply> {
        use tokio::io::AsyncWriteExt;
        timeout(Duration::from_secs(70), async {
            if let Some((socket, pid)) = &self.control {
                let snapshot = control::config_snapshot(socket).await
                    .context("Read active configuration; update/restart the proxy core if snapshots are unavailable")?;
                anyhow::ensure!(Some(snapshot.pid) == *pid, "Core PID changed; retry diagnostics");
                anyhow::ensure!(snapshot.text.len() <= CONFIG_LIMIT, "Configuration exceeds 480 KiB");
                self.config_path = snapshot.path;
                self.text = snapshot.text;
            }
            let selected = targets.is_some() || list;
            let payload = if selected {
                serde_json::to_vec(&astra_model::diagnostics::Request { config: self.text, targets, list_targets: list })?
            } else { self.text.into_bytes() };
            let mut child = core_process(self.executable, &self.runtime_dir)
                .arg(if refresh { "--refresh-subscriptions-stdin" } else if selected { "--diagnose-request" } else { "--diagnose-stdin" }).arg(self.config_path)
                .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true).spawn()?;
            let mut input = child.stdin.take().context("Missing diagnostic input")?;
            input.write_all(&payload).await?;
            drop(input);
            let output = child.wait_with_output().await?;
            anyhow::ensure!(
                output.status.success(),
                "Diagnostic core failed: {}",
                String::from_utf8_lossy(&output.stderr)
                    .chars()
                    .take(4096)
                    .collect::<String>()
            );
            anyhow::ensure!(
                output.stdout.len() < 900 * 1024,
                "Diagnostic report exceeds limit"
            );
            if refresh { return Ok(Reply::text(String::from_utf8(output.stdout)?)); }
            if list {
                let targets: Vec<String> = serde_json::from_slice(&output.stdout)?;
                return Ok(Reply::text(serde_json::to_string(&targets)?));
            }
            let mut report: astra_model::diagnostics::Report = serde_json::from_slice(&output.stdout)
                .context("Update the proxy core to use diagnostics")?;
            report.running_config = self.running;
            Ok(Reply::text(serde_json::to_string(&report)?))
        })
        .await
        .context("Diagnostic core timed out")?
    }
}

/// Relay one JSON event per line from a short-lived core; `done` marks the final event.
async fn pipe_events<E: serde::de::DeserializeOwned + Serialize>(
    (executable, runtime_dir): (PathBuf, PathBuf),
    flag: &str,
    config_path: PathBuf,
    payload: Vec<u8>,
    output: &mut (impl tokio::io::AsyncWrite + Unpin),
    outdated: &'static str,
    mut done: impl FnMut(&mut E) -> bool,
) -> Result<()> {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
    let mut child = core_process(executable, &runtime_dir)
        .arg(flag)
        .arg(config_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let mut input = child.stdin.take().context("Missing diagnostic input")?;
    input.write_all(&payload).await?;
    drop(input);
    let stderr = child.stderr.take().context("Missing diagnostic errors")?;
    let errors = tokio::spawn(async move {
        let mut bytes = Vec::new();
        let _ = stderr.take(4096).read_to_end(&mut bytes).await;
        bytes
    });
    let mut reader = BufReader::new(child.stdout.take().context("Missing diagnostic output")?);
    let mut last = None;
    for _ in 0..20000 {
        let mut line = String::new();
        if (&mut reader).take(1024 * 1024).read_line(&mut line).await? == 0 {
            break;
        }
        anyhow::ensure!(line.ends_with('\n'), "Diagnostic event exceeds limit");
        let mut event: E = serde_json::from_str(&line).context(outdated)?;
        if done(&mut event) {
            last = Some(event);
            break;
        }
        timeout(
            Duration::from_secs(2),
            astra_ipc::send_management(output, &Reply::text(serde_json::to_string(&event)?)),
        )
        .await??;
    }
    let status = child.wait().await?;
    let errors = String::from_utf8_lossy(&errors.await?).into_owned();
    anyhow::ensure!(
        status.success(),
        "Diagnostic core failed: {}",
        errors.trim()
    );
    let last = last.context("Diagnostic stream ended before completion")?;
    astra_ipc::send_management(output, &Reply::text(serde_json::to_string(&last)?)).await?;
    Ok(())
}

fn core_process(path: PathBuf, runtime_dir: &Path) -> Process {
    let mut command = astra_platform::platform::background_core(path);
    command.env("ASTRA_RUNTIME_DIR", runtime_dir);
    command
}

pub use astra_platform::storage::{atomic_write_default as atomic_write, private_dir};

fn read_config(path: &Path) -> Result<String> {
    let mut text = String::new();
    fs::File::open(path)?
        .take(CONFIG_LIMIT as u64 + 1)
        .read_to_string(&mut text)?;
    anyhow::ensure!(text.len() <= CONFIG_LIMIT, "Configuration exceeds 480 KiB");
    Ok(text)
}

/// Only NotFound creates a template; permission errors and existing files are preserved.
fn ensure_config(kind: Kind, path: &Path) -> Result<bool> {
    anyhow::ensure!(
        kind != Kind::Server,
        "Server instances require the v2 server adapter"
    );
    match fs::symlink_metadata(path) {
        Ok(_) => return Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(error).with_context(|| format!("Inspect configuration {}", path.display()))
        }
    }
    let parent = path
        .parent()
        .context("Configuration needs a parent directory")?;
    let parent_existed = parent.exists();
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(parent)
        .with_context(|| format!("Create configuration directory {}", parent.display()))?;
    let mut file = match OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Ok(false),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("Create default configuration {}", path.display()))
        }
    };
    if !parent_existed {
        set_private(parent, 0o700)?;
    }
    set_private(path, 0o600)?;
    let template = match kind {
        Kind::Proxy => include_str!("../../../configs/proxy.toml"),
        Kind::Rule => include_str!("../../../configs/rule.toml"),
        Kind::Server => unreachable!("server kind rejected before opening a file"),
    };
    if let Err(error) = file
        .write_all(template.as_bytes())
        .and_then(|()| file.sync_all())
    {
        let _ = fs::remove_file(path);
        return Err(error).context("Write default configuration");
    }
    Ok(true)
}

impl Manager {
    fn notify(&self) {
        if let Some(publisher) = &self.publisher {
            publisher.publish(self);
        }
    }
    fn node_tags(&self, name: &str) -> Result<Vec<String>> {
        let instance = self.instance(name)?;
        let bindings = if instance.kind == Kind::Rule {
            self.bound_proxies(name)
        } else {
            vec![name.to_owned()]
        };
        let mut tags = std::collections::BTreeSet::new();
        for proxy in self.registry.instances.iter().filter(|i| {
            i.kind == Kind::Proxy && (bindings.is_empty() || bindings.contains(&i.name))
        }) {
            let Ok(config) = read_config(&proxy.config)
                .and_then(|text| Ok(toml::from_str::<toml::Value>(&text)?))
            else {
                continue;
            };
            if let Some(nodes) = config.get("outbounds").and_then(toml::Value::as_array) {
                tags.extend(
                    nodes
                        .iter()
                        .filter_map(|node| node.get("tag").and_then(toml::Value::as_str))
                        .filter(|tag| !tag.is_empty())
                        .map(str::to_owned),
                );
            }
            if let Some(groups) = config.get("proxy_groups").and_then(toml::Value::as_table) {
                tags.extend(
                    groups
                        .keys()
                        .filter(|name| !astra_model::groups::builtin(name))
                        .cloned(),
                );
            }
            tags.insert("PROXY".into());
        }
        Ok(tags.into_iter().collect())
    }
    fn diagnostic_plan(&self, name: &str) -> Result<DiagnosticPlan> {
        let instance = self.instance(name)?;
        anyhow::ensure!(
            instance.kind == Kind::Proxy,
            "Select a proxy instance for diagnostics"
        );
        let running = self.running.get(name);
        Ok(DiagnosticPlan {
            runtime_dir: self.runtime_dir.clone(),
            executable: self.bin.join(Kind::Proxy.binary()),
            config_path: running
                .map(|r| r.config_path.clone())
                .unwrap_or_else(|| instance.config.clone()),
            text: match running {
                Some(r) => r.config.clone(),
                None => read_config(&instance.config)?,
            },
            running: running.is_some(),
            control: running.map(|r| (self.socket(name), r.child.id())),
        })
    }
    #[cfg(test)]
    fn load(home: PathBuf, bin: PathBuf) -> Result<Self> {
        Self::load_with_config(home.clone(), home.join("config"), bin)
    }
    fn load_with_config(home: PathBuf, config_home: PathBuf, bin: PathBuf) -> Result<Self> {
        Self::load_with_runtime(home, config_home, bin, None)
    }
    fn load_with_runtime(
        home: PathBuf,
        config_home: PathBuf,
        bin: PathBuf,
        runtime_dir: Option<PathBuf>,
    ) -> Result<Self> {
        let runtime_dir = runtime_dir.map_or_else(astra_platform::storage::runtime_root, Ok)?;
        private_dir(&runtime_dir)?;
        let runtime_dir = fs::canonicalize(runtime_dir)?;
        private_dir(&config_home)?;
        private_dir(&config_home.join("configs"))?;
        let path = config_home.join("instances.toml");
        let legacy = home.join("instances.toml");
        let registry_path = if path.exists() { &path } else { &legacy };
        let mut registry: Registry = if registry_path.exists() {
            toml::from_str(&fs::read_to_string(registry_path)?)?
        } else {
            Registry::default()
        };
        anyhow::ensure!(registry.instances.len() <= 64, "Maximum 64 instances");
        let mut names = HashSet::new();
        for instance in &registry.instances {
            validate_name(&instance.name).with_context(|| {
                format!(
                    "Invalid registry entry in {}; original file is retained",
                    registry_path.display()
                )
            })?;
            anyhow::ensure!(instance.kind != Kind::Server, "Server entries cannot be loaded from {}; register them through the server adapter; original file is retained", registry_path.display());
            anyhow::ensure!(
                names.insert(instance.name.to_ascii_lowercase()),
                "Duplicate instance name in {}; original file is retained",
                registry_path.display()
            );
        }
        let runtime_path = home.join("runtime.json");
        let runtime = if runtime_path.exists() {
            serde_json::from_slice(&fs::read(runtime_path)?)?
        } else {
            RuntimeState {
                log_levels: std::mem::take(&mut registry.log_levels),
            }
        };
        let manager = Self {
            updates: updates::Updates::load(&home, &bin)?,
            publisher: None,
            home,
            config_home,
            runtime,
            runtime_dir,
            bin,
            registry,
            running: HashMap::new(),
            blocked: HashMap::new(),
            exits: HashMap::new(),
            #[cfg(any(target_os = "linux", target_os = "macos"))]
            elevations: HashMap::new(),
            elevation_events: None,
        };
        // Copy legacy metadata once; never move a file still referenced by a core.
        if !path.exists() {
            manager.persist()?;
        }
        manager.persist_runtime()?;
        Ok(manager)
    }
    fn persist_runtime(&self) -> Result<()> {
        Ok(atomic_write(
            &self.home.join("runtime.json"),
            &serde_json::to_vec(&self.runtime)?,
        )?)
    }
    fn persist(&self) -> Result<()> {
        Ok(atomic_write(
            &self.config_home.join("instances.toml"),
            toml::to_string_pretty(&self.registry)?.as_bytes(),
        )?)
    }
    fn instance(&self, name: &str) -> Result<Instance> {
        if let Some(reason) = self.blocked.get(name) {
            bail!("Instance {name} awaits identity verification: {reason}; its process and record were retained. Use Force start to replace the previous session");
        }
        self.registered_instance(name)
    }
    fn registered_instance(&self, name: &str) -> Result<Instance> {
        self.registry
            .instances
            .iter()
            .find(|i| i.name == name)
            .cloned()
            .context("Unknown instance")
    }
    fn socket(&self, name: &str) -> PathBuf {
        self.home.join(format!("{name}.sock"))
    }
    fn log(&self, name: &str) -> PathBuf {
        self.home.join(format!("{name}.log"))
    }
    fn reap(&mut self) -> Result<()> {
        let mut exited = vec![];
        for (name, run) in &mut self.running {
            if let Some(status) = run.child.try_wait()? {
                exited.push((name.clone(), status));
            }
        }
        for (name, status) in exited {
            self.running.remove(&name);
            self.forget_process(&name)?;
            self.exits.insert(name.clone(), format!("Exited: {status}"));
            // This socket belongs to the child we just reaped, never to an arbitrary PID.
            let _ = astra_platform::platform::recover_stale_endpoint(&self.socket(&name));
        }
        Ok(())
    }
    async fn validate(&self, kind: Kind, path: &Path) -> Result<()> {
        self.validate_text(kind, path, &read_config(path)?).await
    }
    async fn validate_text(&self, kind: Kind, path: &Path, text: &str) -> Result<()> {
        astra_ipc::progress::report("Validate configuration", path.display().to_string());
        use tokio::io::AsyncWriteExt;
        let output = timeout(
            Duration::from_secs(if kind == Kind::Rule { 90 } else { 20 }),
            async {
                let mut child = core_process(self.bin.join(kind.binary()), &self.runtime_dir)
                    .arg("--check-stdin")
                    .arg(path)
                    .env_remove("ASTRA_CONTROL_SOCKET")
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped())
                    .kill_on_drop(true)
                    .spawn()?;
                let mut stdin = child.stdin.take().context("Missing validation input")?;
                stdin.write_all(text.as_bytes()).await?;
                drop(stdin);
                child
                    .wait_with_output()
                    .await
                    .context("Configuration check failed")
            },
        )
        .await
        .context("Configuration check timed out")??;
        anyhow::ensure!(
            output.status.success(),
            "Configuration invalid:\n{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        Ok(())
    }
    fn start_plan(&self, name: &str) -> Result<Vec<String>> {
        self.start_plan_with(name, false)
    }
    fn start_plan_with(&self, name: &str, force: bool) -> Result<Vec<String>> {
        fn visit(
            manager: &Manager,
            name: &str,
            force: bool,
            active: &mut HashSet<String>,
            done: &mut HashSet<String>,
            plan: &mut Vec<String>,
        ) -> Result<()> {
            let instance = if force {
                manager.registered_instance(name)?
            } else {
                manager.instance(name)?
            };
            if done.contains(name) {
                return Ok(());
            }
            anyhow::ensure!(
                active.insert(name.to_owned()),
                "Startup binding cycle: {name}"
            );
            if let Some(targets) = manager.registry.start_bindings.get(name) {
                for target in targets {
                    visit(manager, target, force, active, done, plan)?;
                }
            } else if let Some(rule) = manager.inferred_rule(&instance) {
                // No explicit binding: start the Rule Core this proxy uses.
                visit(manager, &rule, force, active, done, plan)?;
            }
            active.remove(name);
            done.insert(name.to_owned());
            plan.push(name.to_owned());
            Ok(())
        }
        let mut plan = Vec::new();
        let mut active = HashSet::new();
        let mut done = HashSet::new();
        visit(self, name, force, &mut active, &mut done, &mut plan)?;
        // Reverse links are expanded only for an explicitly started rule.
        // Starting one proxy never starts its sibling proxies.
        if self.registered_instance(name)?.kind == Kind::Rule {
            for proxy in self.bound_proxies(name) {
                visit(self, &proxy, force, &mut active, &mut done, &mut plan)?;
            }
        }
        Ok(plan)
    }
    /// A dual-core proxy without an explicit start binding depends on the
    /// registered Rule Core whose socket its configuration names, so starting
    /// the proxy alone is enough; `b` remains available to override this.
    fn inferred_rule(&self, proxy: &Instance) -> Option<String> {
        if proxy.kind != Kind::Proxy {
            return None;
        }
        let table = |path: &Path| {
            read_config(path)
                .ok()
                .and_then(|text| toml::from_str::<toml::Table>(&text).ok())
        };
        let config = table(&proxy.config)?;
        if config.get("standalone").and_then(toml::Value::as_bool) == Some(true) {
            return None;
        }
        let wanted = astra_platform::platform::endpoint_path(config.get("rule_socket")?.as_str()?);
        self.registry
            .instances
            .iter()
            .filter(|rule| rule.kind == Kind::Rule)
            .find(|rule| {
                table(&rule.config)
                    .and_then(|rule| {
                        rule.get("socket")
                            .and_then(toml::Value::as_str)
                            .map(astra_platform::platform::endpoint_path)
                    })
                    .is_some_and(|socket| socket == wanted)
            })
            .map(|rule| rule.name.clone())
    }
    fn bound_proxies(&self, rule: &str) -> Vec<String> {
        self.registry
            .instances
            .iter()
            .filter(|instance| {
                instance.kind == Kind::Proxy
                    && self
                        .registry
                        .start_bindings
                        .get(&instance.name)
                        .is_some_and(|targets| targets.iter().any(|target| target == rule))
            })
            .map(|instance| instance.name.clone())
            .collect()
    }
    fn bind_start(&mut self, name: &str, targets: Vec<String>) -> Result<()> {
        let instance = self.instance(name)?;
        let mut unique = HashSet::new();
        let targets: Vec<_> = targets
            .into_iter()
            .filter(|target| unique.insert(target.clone()))
            .collect();
        let kind = instance.kind;
        for target in &targets {
            anyhow::ensure!(
                self.instance(target)?.kind != kind,
                "Bind proxies to a rule instance"
            );
        }
        if kind == Kind::Proxy {
            anyhow::ensure!(targets.len() <= 1, "Select one rule instance");
        }
        let previous = self.registry.start_bindings.clone();
        if kind == Kind::Rule {
            // Store one canonical proxy -> rule relation for both editing directions.
            for proxy in self.bound_proxies(name) {
                self.registry.start_bindings.remove(&proxy);
            }
            self.registry.start_bindings.remove(name);
            for proxy in targets {
                self.registry
                    .start_bindings
                    .insert(proxy, vec![name.to_owned()]);
            }
        } else if targets.is_empty() {
            self.registry.start_bindings.remove(name);
        } else {
            self.registry
                .start_bindings
                .insert(name.to_owned(), targets);
        }
        if let Err(error) = self.start_plan(name).and_then(|_| self.persist()) {
            self.registry.start_bindings = previous;
            return Err(error);
        }
        Ok(())
    }
    async fn start(&mut self, name: &str) -> Result<()> {
        self.start_with_recovery(name, false).await
    }
    async fn start_with_recovery(&mut self, name: &str, force: bool) -> Result<()> {
        self.reap()?;
        let plan = self.start_plan_with(name, force)?;
        // Validate the group before starting its first new process. Checks run
        // concurrently; a Rule Core is not pre-checked because its own start
        // compiles the same index (a failed start reports its log and rolls back).
        let mut checks = Vec::new();
        for item in &plan {
            if let Some(running) = self.running.get(item) {
                let status = control::request(&self.socket(item), Command::Status)
                    .await
                    .with_context(|| format!("Bound instance is unavailable: {item}"))?;
                anyhow::ensure!(
                    Some(status.pid) == running.child.id(),
                    "Core PID mismatch: {item}"
                );
            } else {
                let instance = if force {
                    self.registered_instance(item)?
                } else {
                    self.instance(item)?
                };
                if instance.kind != Kind::Rule {
                    checks.push((instance.kind, instance.config));
                }
            }
        }
        for result in futures_util::future::join_all(
            checks
                .iter()
                .map(|(kind, config)| self.validate(*kind, config)),
        )
        .await
        {
            result?;
        }
        let total = plan.len() as u64;
        let mut started: Vec<String> = Vec::new();
        for (index, item) in plan.into_iter().enumerate() {
            astra_ipc::progress::counted("Start dependency", item.clone(), index as u64, total);
            if self.running.contains_key(&item) {
                continue;
            }
            let result = if force {
                self.start_with_options(&item, None, true).await
            } else {
                self.start_one(&item).await
            };
            if let Err(error) = result {
                // Only processes created by this attempt are rolled back.
                let mut rollback_errors = Vec::new();
                if self.running.contains_key(&item) {
                    if let Err(error) = self.stop(&item).await {
                        rollback_errors.push(format!("{item}: {error:#}"));
                    }
                }
                for started in started.iter().rev() {
                    if let Err(error) = self.stop(started).await {
                        rollback_errors.push(format!("{started}: {error:#}"));
                    }
                }
                if !rollback_errors.is_empty() {
                    return Err(error).with_context(|| {
                        format!(
                            "Start {item} failed; rollback incomplete: {}",
                            rollback_errors.join("; ")
                        )
                    });
                }
                return Err(error).with_context(|| format!("Start bound instance {item}"));
            }
            astra_ipc::progress::counted("Dependency ready", item.clone(), index as u64 + 1, total);
            started.push(item);
        }
        Ok(())
    }
    /// Launch a Windows TUN proxy through UAC. `replaces` is a running core
    /// the new one waits for, so the prompt is answered before that core stops.
    #[cfg(windows)]
    async fn spawn_elevated(&self, name: &str, replaces: Option<u32>) -> Result<ManagedChild> {
        let instance = self.instance(name)?;
        let exe = self.bin.join(instance.kind.binary());
        let mut args = astra_platform::elevation::core_arguments_in(
            &fs::canonicalize(&instance.config)?,
            Some(&self.socket(name)),
            &self.log(name),
            self.level(name).as_str(),
            replaces,
            &self.runtime_dir,
        )?;
        args.push(
            serde_json::to_string(&astra_platform::lifecycle::capture(std::process::id())?)?.into(),
        );
        astra_ipc::progress::report(
            "Waiting for administrator approval",
            format!("{name} · approve the Windows UAC prompt to run TUN"),
        );
        Ok(ManagedChild::Elevated(
            tokio::task::spawn_blocking(move || astra_platform::elevation::spawn(&exe, &args))
                .await??,
        ))
    }
    async fn start_one(&mut self, name: &str) -> Result<()> {
        self.start_with(name, None).await
    }
    /// Start a core, or adopt `prepared`: an elevated replacement already
    /// approved through UAC that binds the endpoint once its predecessor exits.
    async fn start_with(&mut self, name: &str, prepared: Option<ManagedChild>) -> Result<()> {
        self.start_with_options(name, prepared, false).await
    }
    async fn start_with_options(
        &mut self,
        name: &str,
        prepared: Option<ManagedChild>,
        force: bool,
    ) -> Result<()> {
        self.reap()?;
        anyhow::ensure!(
            !self.running.contains_key(name),
            "Instance is already running"
        );
        astra_ipc::progress::report("Start core", name.to_owned());
        let instance = if force {
            self.registered_instance(name)?
        } else {
            self.instance(name)?
        };
        let socket = self.socket(name);
        let adopted = prepared.is_some();
        let config = read_config(&instance.config)?;
        if !adopted {
            if force {
                self.replace_session(name)?;
            } else {
                prepare_control_endpoint(&socket).await?;
            }
            // An adopted replacement already has the log open; never move it.
            rotate_log(&self.log(name));
        }
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(self.log(name))?;
        #[cfg(windows)]
        let elevate = adopted
            || instance.kind == Kind::Proxy
                && astra_platform::runtime::load_overrides_in(&self.runtime_dir, &instance.config)?
                    .tun
                    == Some(true)
                && !astra_platform::elevation::is_elevated()?;
        #[cfg(not(windows))]
        let elevate = false;
        #[cfg(windows)]
        let elevated = match prepared {
            Some(child) => Some(child),
            None if elevate => Some(self.spawn_elevated(name, None).await?),
            None => None,
        };
        #[cfg(not(windows))]
        let elevated = prepared;
        let child = if let Some(child) = elevated {
            child
        } else {
            ManagedChild::Normal(Box::new(OwnedChild::new(
                core_process(self.bin.join(instance.kind.binary()), &self.runtime_dir)
                    .arg(&instance.config)
                    .env("ASTRA_CONTROL_SOCKET", &socket)
                    .env("ASTRA_LOG_LEVEL", self.level(name).as_str())
                    .env(
                        "ASTRA_SERVICE_OWNER",
                        serde_json::to_string(&astra_platform::lifecycle::capture(
                            std::process::id(),
                        )?)?,
                    )
                    .stdin(Stdio::null())
                    .stdout(log.try_clone()?)
                    .stderr(log)
                    .kill_on_drop(false)
                    .spawn()?,
            )))
        };
        self.running.insert(
            name.to_owned(),
            Running {
                child,
                config,
                config_path: instance.config.clone(),
            },
        );
        self.exits.remove(name);
        self.notify();
        let ready_started = tokio::time::Instant::now();
        let mut last_progress = tokio::time::Instant::now() - Duration::from_secs(1);
        let deadline = tokio::time::Instant::now()
            + Duration::from_secs(if instance.kind == Kind::Rule {
                60
            } else if elevate {
                30
            } else {
                10
            });
        while tokio::time::Instant::now() < deadline {
            if last_progress.elapsed() >= Duration::from_millis(300) {
                astra_ipc::progress::report(
                    "Wait for core readiness",
                    format!("{} · {:.1}s", name, ready_started.elapsed().as_secs_f64()),
                );
                last_progress = tokio::time::Instant::now();
            }
            self.reap()?;
            let Some(run) = self.running.get(name) else {
                bail!(
                    "Core exited during startup:\n{}",
                    log_tail(&self.log(name), 2048)
                );
            };
            if let Ok(status) = control::request(&socket, Command::Status).await {
                anyhow::ensure!(Some(status.pid) == run.child.id(), "Core PID mismatch");
                if let Err(error) = self.commit_process(name).await {
                    let cleanup = self.stop(name).await;
                    return Err(error).context(format!(
                        "Core readiness could not be committed; candidate cleanup: {cleanup:?}"
                    ));
                }
                return Ok(());
            }
            sleep(Duration::from_millis(50)).await;
        }
        self.stop(name).await?;
        bail!(
            "Core did not become ready:\n{}",
            log_tail(&self.log(name), 2048)
        )
    }
    async fn reload(&mut self, name: &str, path: &Path) -> Result<()> {
        astra_ipc::progress::report("Reload core", name.to_owned());
        let i = self.instance(name)?;
        self.validate(i.kind, path).await?;
        let status = control::request(
            &self.socket(name),
            Command::Reload {
                path: Some(path.to_owned()),
            },
        )
        .await?;
        let run = self
            .running
            .get_mut(name)
            .context("Instance is not running")?;
        anyhow::ensure!(run.child.id() == Some(status.pid), "Core PID mismatch");
        run.config = read_config(path)?;
        run.config_path = path.to_owned();
        self.commit_process(name).await?;
        Ok(())
    }
    async fn stop(&mut self, name: &str) -> Result<()> {
        self.stop_core(name, true).await
    }
    /// `release_endpoint` is false when an approved replacement is waiting:
    /// it binds the endpoint as soon as this core exits, so the marker must
    /// not be removed after it (a stale one is recovered by its bind).
    async fn stop_core(&mut self, name: &str, release_endpoint: bool) -> Result<()> {
        astra_ipc::progress::report("Drain and stop core", name.to_owned());
        self.reap()?;
        self.instance(name)?;
        if !self.running.contains_key(name) {
            return Ok(());
        }
        // Acknowledged IPC stop uses the same drain path as SIGTERM.
        let command = self.running[name].child.stop_command();
        let response = control::request(&self.socket(name), command).await;
        let run = self.running.get_mut(name).unwrap();
        if response.is_err() {
            // Only terminate an owned child, never a persisted PID.
            run.child.kill().await?;
        }
        let state = match timeout(Duration::from_secs(30), run.child.wait()).await {
            Err(_) => {
                run.child.kill().await?;
                "Stopped (drain timeout; forced)"
            }
            Ok(result) => {
                result?;
                if response.is_err() {
                    "Stopped (IPC unavailable; forced)"
                } else {
                    "Stopped"
                }
            }
        };
        self.exits.insert(name.to_owned(), state.into());
        self.running.remove(name);
        self.forget_process(name)?;
        self.notify();
        if release_endpoint {
            let _ = astra_platform::platform::recover_stale_endpoint(&self.socket(name));
        }
        Ok(())
    }
    #[cfg(windows)]
    async fn elevate_runtime_tun(&mut self, name: &str) -> Result<Reply> {
        let previous_pid = self
            .running
            .get(name)
            .and_then(|running| running.child.id());
        let instance = self.instance(name)?;
        let active = control::config_snapshot(&self.socket(name)).await?;
        anyhow::ensure!(
            active.path == fs::canonicalize(&instance.config)?
                && active.text == read_config(&instance.config)?,
            "Configuration changed on disk; reload it before requesting an elevated TUN restart"
        );
        let previous =
            astra_platform::runtime::load_overrides_in(&self.runtime_dir, &instance.config)?;
        let mut next = previous.clone();
        next.tun = Some(true);
        let path = astra_platform::storage::runtime_path_in(
            &self.runtime_dir,
            &instance.config,
            "overrides",
        )?;
        // Ask for approval while the current core keeps serving; a cancelled
        // or failed prompt leaves it running with its runtime state unchanged.
        let replacement = self
            .spawn_elevated(name, previous_pid)
            .await
            .context("TUN was not enabled; the running core was left unchanged")?;
        astra_ipc::progress::report(
            "Restart with TUN",
            "Approved; handing over to the administrator core (the profile is unchanged)",
        );
        // The replacement reads the switches only after the old core exits.
        astra_platform::storage::write_json(&path, &next)?;
        let result: Result<Reply> = async {
            // An early return drops (and so terminates) the replacement.
            self.stop_core(name, false).await?;
            self.start_with(name, Some(replacement)).await?;
            let snapshot =
                control::runtime(&self.socket(name), astra_model::runtime::Command::Snapshot)
                    .await?;
            anyhow::ensure!(
                self.running.get(name).and_then(|r| r.child.id()) == Some(snapshot.pid),
                "Core PID changed during elevated restart"
            );
            Ok(Reply::text(serde_json::to_string(&snapshot)?))
        }
        .await;
        if let Err(error) = result {
            astra_platform::storage::write_json(&path, &previous)
                .context("Restore previous runtime switches after rejected TUN restart")?;
            self.recover_handover(name, previous_pid)
                .await
                .context("Restore previous proxy core after rejected TUN restart")?;
            return Err(error);
        }
        result
    }
    /// After a failed handover, stop a replacement that came up and bring the
    /// instance back if nothing is serving it.
    #[cfg(windows)]
    async fn recover_handover(&mut self, name: &str, previous_pid: Option<u32>) -> Result<()> {
        let _ = self.reap();
        if self.running.contains_key(name)
            && self
                .running
                .get(name)
                .and_then(|running| running.child.id())
                != previous_pid
        {
            self.stop(name).await.context("Stop the replacement core")?;
        }
        if !self.running.contains_key(name) {
            self.start_one(name).await?;
        }
        self.notify();
        Ok(())
    }
    /// Restart an elevated TUN proxy with the UAC prompt answered while the
    /// current core still serves, so its listeners close only for the drain
    /// and handover rather than for as long as the prompt is open. Returns
    /// false when the plain stop/start path applies.
    #[cfg(windows)]
    async fn restart_elevated(&mut self, name: &str) -> Result<bool> {
        self.reap()?;
        let instance = self.instance(name)?;
        let Some(previous_pid) = self.running.get(name).and_then(|r| r.child.id()) else {
            return Ok(false);
        };
        if instance.kind != Kind::Proxy
            || astra_platform::runtime::load_overrides_in(&self.runtime_dir,&instance.config)?.tun != Some(true)
            || astra_platform::elevation::is_elevated()?
            // Missing dependencies need start()'s ordered startup and rollback.
            || self
                .start_plan(name)?
                .iter()
                .any(|item| item != name && !self.running.contains_key(item))
        {
            return Ok(false);
        }
        let replacement = self
            .spawn_elevated(name, Some(previous_pid))
            .await
            .context("Restart was cancelled; the running core was left unchanged")?;
        astra_ipc::progress::report(
            "Restart with TUN",
            "Approved; handing over to the administrator core",
        );
        let result: Result<()> = async {
            // An early return drops (and so terminates) the replacement.
            self.stop_core(name, false).await?;
            self.start_with(name, Some(replacement)).await
        }
        .await;
        if let Err(error) = result {
            self.recover_handover(name, Some(previous_pid))
                .await
                .context("Restore proxy core after failed restart")?;
            return Err(error);
        }
        Ok(true)
    }
    async fn stop_all(&mut self) -> Result<()> {
        let mut failures: Vec<_> = self
            .blocked
            .iter()
            .map(|(name, reason)| {
                format!("{name}: identity unverified ({reason}); retained without signalling")
            })
            .collect();
        if let Err(error) = self.reap() {
            failures.push(format!("observe cores: {error:#}"));
        }
        for kind in [Kind::Proxy, Kind::Rule] {
            let names: Vec<_> = self
                .registry
                .instances
                .iter()
                .filter(|i| i.kind == kind && self.running.contains_key(&i.name))
                .map(|i| i.name.clone())
                .collect();
            let endpoints: Vec<_> = names
                .iter()
                .map(|name| (self.socket(name), self.running[name].child.stop_command()))
                .collect();
            let responses =
                futures_util::future::join_all(endpoints.into_iter().map(
                    |(socket, command)| async move { control::request(&socket, command).await },
                ))
                .await;
            let mut waits = Vec::new();
            for (name, response) in names.into_iter().zip(responses) {
                let Some(mut run) = self.running.remove(&name) else {
                    continue;
                };
                waits.push(async move {
                    let outcome = async {
                        if response.is_err() {
                            run.child.kill().await?;
                        }
                        match timeout(Duration::from_secs(30), run.child.wait()).await {
                            Ok(result) => {
                                result?;
                            }
                            Err(_) => run.child.kill().await?,
                        }
                        Ok::<(), std::io::Error>(())
                    }
                    .await;
                    (name, run, outcome)
                });
            }
            for (name, run, outcome) in futures_util::future::join_all(waits).await {
                match outcome {
                    Ok(()) => {
                        let _ =
                            astra_platform::platform::recover_stale_endpoint(&self.socket(&name));
                        if let Err(error) = self.forget_process(&name) {
                            failures.push(format!("{name} identity cleanup: {error:#}"));
                        }
                        self.exits.insert(name, "Stopped".into());
                    }
                    Err(error) => {
                        failures.push(format!("{name}: {error:#}"));
                        self.running.insert(name, run);
                    }
                }
            }
            self.notify();
        }
        if let Err(error) = self.stop_rule_generations().await {
            failures.push(format!("rule generations: {error:#}"));
        }
        anyhow::ensure!(
            failures.is_empty(),
            "Shutdown incomplete: {}",
            failures.join("; ")
        );
        Ok(())
    }
    fn level(&self, name: &str) -> LogLevel {
        self.runtime
            .log_levels
            .get(name)
            .copied()
            .unwrap_or_default()
    }
    fn check_name(&self, name: &str) -> Result<()> {
        validate_name(name)?;
        anyhow::ensure!(
            !self
                .registry
                .instances
                .iter()
                .any(|instance| instance.name.eq_ignore_ascii_case(name)),
            "Instance name already exists"
        );
        anyhow::ensure!(self.registry.instances.len() < 64, "Maximum 64 instances");
        Ok(())
    }
    async fn add(&mut self, mut i: Instance) -> Result<Reply> {
        anyhow::ensure!(
            i.kind != Kind::Server,
            "Server instances require the v2 server adapter"
        );
        self.check_name(&i.name)?;
        ensure_config(i.kind, &i.config)?;
        i.config = fs::canonicalize(&i.config)?;
        self.validate(i.kind, &i.config).await?;
        self.registry.instances.push(i);
        if let Err(e) = self.persist() {
            self.registry.instances.pop();
            return Err(e);
        }
        Ok(Reply::text("Instance added"))
    }
    async fn create(&mut self, name: String, kind: Kind) -> Result<Reply> {
        anyhow::ensure!(
            kind != Kind::Server,
            "Server instances require the v2 server adapter"
        );
        self.check_name(&name)?;
        let directory = self.config_home.join("configs");
        private_dir(&directory)?;
        let config = directory.join(format!("{name}.toml"));
        if ensure_config(kind, &config)? {
            let mut document: toml_edit::DocumentMut = read_config(&config)?.parse()?;
            let socket = if kind == Kind::Rule {
                if self.registry.instances.iter().any(|i| i.kind == Kind::Rule) {
                    self.home.join(format!("{name}.r"))
                } else {
                    self.home.join("rule.r")
                }
            } else {
                self.registry
                    .instances
                    .iter()
                    .filter(|i| i.kind == Kind::Rule)
                    .find_map(|i| {
                        read_config(&i.config)
                            .ok()
                            .and_then(|text| toml::from_str::<toml::Value>(&text).ok())
                            .and_then(|v| {
                                v.get("socket")
                                    .and_then(toml::Value::as_str)
                                    .map(PathBuf::from)
                            })
                    })
                    .unwrap_or_else(|| self.home.join("rule.r"))
            };
            document[if kind == Kind::Rule {
                "socket"
            } else {
                "rule_socket"
            }] = toml_edit::value(socket.to_str().context("Socket path is not UTF-8")?);
            atomic_write(&config, document.to_string().as_bytes())?;
        }
        self.add(Instance { name, kind, config }).await
    }
    async fn save_typed(
        &mut self,
        name: String,
        original: String,
        text: String,
    ) -> Result<astra_management::v2::SaveOutcome> {
        use crate::save::{failed, outcome};
        use astra_management::v2::SaveState;
        anyhow::ensure!(text.len() <= CONFIG_LIMIT, "Configuration exceeds 480 KiB");
        let instance = self.instance(&name)?;
        anyhow::ensure!(
            read_config(&instance.config)? == original,
            "Conflict: file changed on disk; reopen before editing"
        );
        self.validate_text(instance.kind, &instance.config, &text)
            .await?;
        anyhow::ensure!(
            read_config(&instance.config)? == original,
            "Conflict: file changed during validation; reopen before editing"
        );
        astra_ipc::progress::report(
            "Save configuration",
            "Validation passed; writing private backup and atomic replacement",
        );
        let backups = self.home.join("backups");
        private_dir(&backups)?;
        atomic_write(
            &backups.join(format!("{name}.toml.bak")),
            original.as_bytes(),
        )?;
        atomic_write(&instance.config, text.as_bytes())?;
        let running = self.running.contains_key(&name);
        if running {
            if let Err(error) = self.reload(&name, &instance.config).await {
                if let Err(restore) = atomic_write(&instance.config, original.as_bytes()) {
                    return Err(failed(SaveState::RecoveryFailed,None,None,format!("Reload failed: {error:#}; source restoration failed: {restore:#}. Draft and backup retained")));
                }
                if let Err(restore) = self.reload(&name, &instance.config).await {
                    return Err(failed(SaveState::OutcomeUnknown,Some(false),None,format!("Reload failed: {error:#}; original file restored, but runtime restoration could not be confirmed: {restore:#}. Draft retained")));
                }
                let mut failure=outcome(SaveState::Restored,Some(false),Some(false),format!("Reload failed: {error:#}; original file and runtime restored; draft retained"));
                failure.recovery = Some("Previous source and runtime confirmed".into());
                return Err(crate::save::SaveFailure { outcome: failure }.into());
            }
        }
        let message = if !running {
            "Validated and saved; instance stopped; configuration applies on next start. Previous file: state directory backups/"
        } else if instance.kind == Kind::Rule {
            "Validated and saved; rule source loaded. F6 shows synchronization for new connections. Previous file: state directory backups/"
        } else {
            "Validated and saved; new connections use the reloaded configuration. Previous file: state directory backups/"
        };
        Ok(outcome(
            if running {
                SaveState::Applied
            } else {
                SaveState::NextStart
            },
            Some(true),
            Some(running),
            message,
        ))
    }
    async fn action(&mut self, action: Action) -> Result<Reply> {
        self.reap()?;
        match action {
            Action::Monitor { name } => self.action_monitor(name).await,
            Action::UpdateTransaction { request } => self.update_transaction(request).await,
            Action::RecoverUpdate { operation_id } => self.recover_update(&operation_id).await,
            Action::WatchUpdates => bail!("Update subscriptions require a dedicated connection"),
            Action::UpdatePackage(path) => self.update_package(path).await,
            Action::UpdateStatus => self.update_status().await,
            Action::UpdateOperationStatus { operation_id } => Self::lookup_update_operation(
                &self.home,
                self.cached_update_status(),
                &operation_id,
            ),
            Action::Rollback => self.rollback_update().await,
            Action::StoragePaths => Ok(Reply::text(serde_json::to_string(&StoragePaths {
                config: self.config_home.clone(),
                state: self.home.clone(),
            })?)),
            Action::Runtime {
                name,
                command,
                expected_pid,
            } => self.action_runtime(name, command, expected_pid).await,
            Action::PrepareElevation { name, expected_pid } => {
                self.action_prepare_elevation(name, expected_pid).await
            }
            Action::CancelElevation { name, ticket } => {
                self.action_cancel_elevation(name, ticket).await
            }
            Action::WaitElevation { .. } => {
                bail!("Elevation status is served outside the mutation actor")
            }
            Action::List => self.action_list().await,
            Action::Add(i) => self.add(i).await,
            Action::Create { name, kind } => self.create(name, kind).await,
            Action::FollowLogs { .. } => bail!("Log subscriptions require a dedicated connection"),
            Action::Diagnostics(_)
            | Action::DiagnosticTargets(_)
            | Action::DiagnosticsSelected { .. } => {
                bail!("Diagnostics require a dedicated connection")
            }
            Action::DiagnosticsStream { .. } | Action::NetTool { .. } => {
                bail!("Diagnostics require a dedicated connection")
            }
            Action::Session | Action::WatchList | Action::WatchRuntime { .. } => {
                bail!("Transport action requires a dedicated connection")
            }
            Action::NodeTags(name) => self.action_node_tags(name).await,
            Action::SubscriptionNodes(name) => self.action_subscription_nodes(name).await,
            Action::ExplainRoute { name, request } => {
                self.action_explain_route(name, request).await
            }
            Action::GetLogLevel(name) => self.action_get_log_level(name).await,
            Action::SetLogLevel { name, level } => self.action_set_log_level(name, level).await,
            Action::BindStart { name, targets } => self.action_bind_start(name, targets).await,
            Action::Remove(name) => self.action_remove(name).await,
            Action::Start(name) => self.action_start(name).await,
            Action::ForceStart(name) => self.action_force_start(name).await,
            Action::Stop(name) => self.action_stop(name).await,
            Action::UpdateSubscriptions(name) => self.action_update_subscriptions(name).await,
            Action::Reload(name) => self.action_reload(name).await,
            Action::Restart(name) => self.action_restart(name).await,
            Action::SetConfig(name, path) => self.action_set_config(name, path).await,
            Action::Read(name) => self.action_read(name).await,
            Action::PrepareConfig(name) => self.action_prepare_config(name).await,
            Action::Save {
                name,
                original,
                text,
            } => Ok(Reply::text(
                self.save_typed(name, original, text).await?.message,
            )),
            Action::CloneConfig(name, destination) => {
                self.action_clone_config(name, destination).await
            }
            Action::Validate(name) => self.action_validate(name).await,
            Action::Logs(name) => self.action_logs(name).await,
            Action::Shutdown => self.action_shutdown().await,
        }
    }

    async fn action_monitor(&mut self, name: String) -> Result<Reply> {
        let kind = self.instance(&name)?.kind;
        let pid = self.running.get(&name).and_then(|r| r.child.id());
        let snapshot = crate::monitor::core(&name, kind, pid, &self.socket(&name)).await?;
        Ok(Reply::text(serde_json::to_string(&snapshot)?))
    }

    async fn action_runtime(
        &mut self,
        name: String,
        command: astra_model::runtime::Command,
        expected_pid: Option<u32>,
    ) -> Result<Reply> {
        let instance = self.instance(&name)?;
        anyhow::ensure!(
            instance.kind == Kind::Proxy,
            "Runtime controls require a proxy instance"
        );
        let running = self.running.get(&name).context("Instance is not running")?;
        let expected = running.child.id();
        anyhow::ensure!(
            expected_pid.is_none() || expected_pid == expected,
            "Core PID changed; refresh before changing runtime state"
        );
        let response = control::runtime(&self.socket(&name), command.clone()).await;
        #[cfg(windows)]
        if matches!(command, astra_model::runtime::Command::Tun(true))
            && response
                .as_ref()
                .is_err_and(|error| error.to_string().contains("administrator permission"))
        {
            return self.elevate_runtime_tun(&name).await;
        }
        let snapshot = response?;
        anyhow::ensure!(
            Some(snapshot.pid) == expected,
            "Core PID changed; retry the operation"
        );
        Ok(Reply::text(serde_json::to_string(&snapshot)?))
    }

    async fn action_prepare_elevation(
        &mut self,
        name: String,
        expected_pid: Option<u32>,
    ) -> Result<Reply> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            elevation::prepare(self, name, expected_pid).await
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = (name, expected_pid);
            bail!("TUN elevation is supported on Linux only")
        }
    }

    async fn action_cancel_elevation(&mut self, name: String, ticket: String) -> Result<Reply> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            elevation::cancel(self, &name, &ticket)
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let _ = (name, ticket);
            Ok(Reply::text("Elevation cancelled"))
        }
    }

    async fn action_list(&mut self) -> Result<Reply> {
        let mut reply = Reply::text("");
        let mut details = HashMap::new();
        for i in &self.registry.instances {
            details.insert(i.name.clone(), self.details(i));
            let status = if self.running.contains_key(&i.name) {
                control::request(&self.socket(&i.name), Command::Status)
                    .await
                    .ok()
            } else {
                None
            };
            let run = self.running.get(&i.name);
            reply.rows.push(Row {
                instance: i.clone(),
                pid: run.and_then(|r| r.child.id()),
                state: if run.is_some() {
                    if status.is_some() {
                        "Running"
                    } else {
                        "Unresponsive"
                    }
                    .into()
                } else {
                    self.exits.get(&i.name).cloned().unwrap_or("Stopped".into())
                },
                uptime: status.as_ref().map(|s| s.uptime_secs).unwrap_or(0),
                changed: run
                    .map(|r| {
                        read_config(&i.config)
                            .map(|text| {
                                if let Some(applied) =
                                    status.as_ref().and_then(|s| s.applied.as_ref())
                                {
                                    applied.path
                                        != fs::canonicalize(&i.config)
                                            .unwrap_or_else(|_| i.config.clone())
                                        || applied.digest != control::config_digest(&text)
                                } else {
                                    text != r.config || i.config != r.config_path
                                }
                            })
                            .unwrap_or(true)
                    })
                    .unwrap_or(false),
            });
        }
        reply.text = serde_json::to_string(&details)?;
        Ok(reply)
    }

    async fn action_node_tags(&mut self, name: String) -> Result<Reply> {
        let json = serde_json::to_string(&self.node_tags(&name)?)?;
        anyhow::ensure!(
            json.len() < 900 * 1024,
            "Node suggestions exceed response limit"
        );
        Ok(Reply::text(json))
    }

    async fn action_subscription_nodes(&mut self, name: String) -> Result<Reply> {
        let instance = self.instance(&name)?;
        anyhow::ensure!(instance.kind == Kind::Proxy, "Select a proxy instance");
        Ok(Reply::text(serde_json::to_string(
            &crate::subscription_view::read_in(&self.runtime_dir, &instance.config)?,
        )?))
    }

    async fn action_explain_route(
        &mut self,
        name: String,
        request: astra_model::RouteRequest,
    ) -> Result<Reply> {
        anyhow::ensure!(
            self.instance(&name)?.kind == Kind::Rule,
            "Select a rule instance"
        );
        let running = self
            .running
            .get(&name)
            .context("Start the rule instance before querying a route")?;
        let explanation = control::explain_route(&self.socket(&name), request).await?;
        anyhow::ensure!(
            Some(explanation.pid) == running.child.id(),
            "Core PID mismatch"
        );
        Ok(Reply::text(serde_json::to_string(&explanation)?))
    }

    async fn action_get_log_level(&mut self, name: String) -> Result<Reply> {
        self.instance(&name)?;
        Ok(Reply::text(self.level(&name).as_str()))
    }

    async fn action_set_log_level(
        &mut self,
        name: String,
        level: astra_model::logging::LogLevel,
    ) -> Result<Reply> {
        self.instance(&name)?;
        let previous = self.level(&name);
        if self.running.contains_key(&name) {
            control::request(&self.socket(&name), Command::SetLogLevel(level))
                .await
                .context("Update core log level")?;
        }
        self.runtime.log_levels.insert(name.clone(), level);
        if let Err(error) = self.persist_runtime() {
            self.runtime.log_levels.insert(name.clone(), previous);
            if self.running.contains_key(&name) {
                let _ = control::request(&self.socket(&name), Command::SetLogLevel(previous)).await;
            }
            return Err(error);
        }
        Ok(Reply::text(level.as_str()))
    }

    async fn action_bind_start(&mut self, name: String, targets: Vec<String>) -> Result<Reply> {
        self.bind_start(&name, targets)?;
        Ok(Reply::text("Startup bindings saved"))
    }

    async fn action_remove(&mut self, name: String) -> Result<Reply> {
        anyhow::ensure!(
            !self.running.contains_key(&name),
            "Stop the instance before removing it"
        );
        self.instance(&name)?;
        let old = self.registry.clone();
        self.registry.instances.retain(|i| i.name != name);
        let previous_level = self.runtime.log_levels.remove(&name);
        self.registry.start_bindings.remove(&name);
        for targets in self.registry.start_bindings.values_mut() {
            targets.retain(|target| target != &name);
        }
        if let Err(e) = self.persist() {
            self.registry = old;
            if let Some(level) = previous_level {
                self.runtime.log_levels.insert(name.clone(), level);
            }
            return Err(e);
        }
        self.persist_runtime()?;
        Ok(Reply::text("Instance removed; configuration retained"))
    }

    async fn action_start(&mut self, name: String) -> Result<Reply> {
        self.start(&name).await?;
        Ok(Reply::text("Started"))
    }

    async fn action_force_start(&mut self, name: String) -> Result<Reply> {
        self.start_with_recovery(&name, true).await?;
        Ok(Reply::text("Started after replacing previous session"))
    }

    async fn action_stop(&mut self, name: String) -> Result<Reply> {
        self.stop(&name).await?;
        Ok(Reply::text("Stopped"))
    }

    async fn action_update_subscriptions(&mut self, name: String) -> Result<Reply> {
        anyhow::ensure!(
            self.instance(&name)?.kind == Kind::Proxy,
            "Select a proxy instance"
        );
        if !self.running.contains_key(&name) {
            return self.diagnostic_plan(&name)?.run_refresh().await;
        }
        astra_ipc::control::request(&self.socket(&name), Command::UpdateSubscriptions).await?;
        Ok(Reply::text("Subscription update scheduled"))
    }

    async fn action_reload(&mut self, name: String) -> Result<Reply> {
        let i = self.instance(&name)?;
        self.reload(&name, &i.config).await?;
        Ok(Reply::text(
            "Configuration hot reloaded; existing sessions retained",
        ))
    }

    async fn action_restart(&mut self, name: String) -> Result<Reply> {
        let i = self.instance(&name)?;
        // Validate before stopping so a bad file never causes downtime;
        // start() checks the not-yet-running dependencies itself.
        self.validate(i.kind, &i.config).await?;
        self.start_plan(&name)?;
        #[cfg(windows)]
        if self.restart_elevated(&name).await? {
            return Ok(Reply::text("Restarted with current configuration"));
        }
        self.stop(&name).await?;
        self.start(&name).await?;
        Ok(Reply::text("Restarted with current configuration"))
    }

    async fn action_set_config(&mut self, name: String, path: PathBuf) -> Result<Reply> {
        let i = self.instance(&name)?;
        ensure_config(i.kind, &path)?;
        let path = fs::canonicalize(path)?;
        self.validate(i.kind, &path).await?;
        let index = self
            .registry
            .instances
            .iter()
            .position(|i| i.name == name)
            .unwrap();
        self.registry.instances[index].config = path.clone();
        if let Err(e) = self.persist() {
            self.registry.instances[index] = i;
            return Err(e);
        }
        if self.running.contains_key(&name) {
            if let Err(error) = self.reload(&name, &path).await {
                self.registry.instances[index] = i;
                self.persist()
                    .context("restore previous configuration selection")?;
                return Err(error);
            }
        }
        Ok(Reply::text(
            "Configuration selected and hot applied when running",
        ))
    }

    async fn action_read(&mut self, name: String) -> Result<Reply> {
        let i = self.instance(&name)?;
        ensure_config(i.kind, &i.config)?;
        Ok(Reply::text(read_config(&i.config)?))
    }

    async fn action_prepare_config(&mut self, name: String) -> Result<Reply> {
        let i = self.instance(&name)?;
        let created = ensure_config(i.kind, &i.config)?;
        Ok(Reply::text(if created {
            read_config(&i.config)?
        } else {
            String::new()
        }))
    }

    async fn action_clone_config(&mut self, name: String, destination: PathBuf) -> Result<Reply> {
        let i = self.instance(&name)?;
        // A clone in another directory could silently change relative asset paths.
        let parent = fs::canonicalize(
            destination
                .parent()
                .context("Missing destination directory")?,
        )?;
        anyhow::ensure!(
            Some(parent.as_path()) == i.config.parent(),
            "Clone into the same directory to preserve relative asset paths"
        );
        let mut file = OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(destination)?;
        file.write_all(read_config(&i.config)?.as_bytes())?;
        file.sync_all()?;
        Ok(Reply::text("Configuration copied; use c to select it"))
    }

    async fn action_validate(&mut self, name: String) -> Result<Reply> {
        let i = self.instance(&name)?;
        self.validate(i.kind, &i.config).await?;
        Ok(Reply::text("Configuration valid"))
    }

    async fn action_logs(&mut self, name: String) -> Result<Reply> {
        self.instance(&name)?;
        let mut file =
            fs::File::open(self.log(&name)).context("No log yet; start this instance first")?;
        let size = file.metadata()?.len();
        file.seek(SeekFrom::Start(size.saturating_sub(24 * 1024)))?;
        let mut bytes = vec![];
        file.take(24 * 1024).read_to_end(&mut bytes)?;
        Ok(Reply::text(String::from_utf8_lossy(&bytes)))
    }

    async fn action_shutdown(&mut self) -> Result<Reply> {
        self.stop_all().await?;
        Ok(Reply::text("Manager and all instances stopped"))
    }
}

impl Manager {
    fn details(&self, instance: &Instance) -> InstanceDetails {
        fn preview(value: &str, limit: usize) -> String {
            let mut text: String = value.chars().take(limit).collect();
            if value.chars().count() > limit {
                text.push('…');
            }
            text
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        let elevation_supported = instance.kind == Kind::Proxy
            && elevation::supported()
            && self
                .running
                .get(&instance.name)
                .is_none_or(|run| !matches!(&run.child, ManagedChild::Adopted(_)));
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        let elevation_supported = false;
        let mut details = InstanceDetails {
            network_recovery: match astra_platform::recovery::read(
                &self.socket(&instance.name).with_extension("recovery.json"),
            ) {
                Ok(notice) => notice,
                Err(error) => Some(astra_model::recovery::Notice {
                    state: astra_model::recovery::RecoveryState::Pending,
                    timestamp_ms: 0,
                    message: format!("Recovery status could not be read: {error}"),
                    journal: self.socket(&instance.name).with_extension("recovery.json"),
                    boot_id: String::new(),
                }),
            },
            node_tags_supported: true,
            subscription_nodes_supported: true,
            diagnostics_supported: true,
            runtime_supported: true,
            elevation_supported,
            explanation_supported: true,
            bindings: if instance.kind == Kind::Rule {
                self.bound_proxies(&instance.name)
            } else {
                self.registry
                    .start_bindings
                    .get(&instance.name)
                    .cloned()
                    .unwrap_or_default()
            },
            log_level: self.level(&instance.name),
            ..InstanceDetails::default()
        };
        let Ok(config) = read_config(&instance.config)
            .and_then(|text| Ok(toml::from_str::<toml::Value>(&text)?))
        else {
            details.config_unavailable = true;
            return details;
        };
        if instance.kind == Kind::Rule {
            details.modes.push("Rules".into());
            details.rules = config
                .get("rules")
                .and_then(toml::Value::as_array)
                .map_or(0, Vec::len);
            if let Some(socket) = config.get("socket").and_then(toml::Value::as_str) {
                details
                    .listeners
                    .push(format!("IPC {}", preview(socket, 128)));
            }
        } else {
            details.modes.push(
                if config
                    .get("standalone")
                    .and_then(toml::Value::as_bool)
                    .unwrap_or(false)
                {
                    "Standalone"
                } else {
                    "Rules"
                }
                .into(),
            );
            for (key, label) in [
                ("socks5_listen", "SOCKS5"),
                ("http_listen", "HTTP"),
                ("mixed_listen", "Mixed"),
            ] {
                if let Some(address) = config.get(key).and_then(toml::Value::as_str) {
                    details
                        .listeners
                        .push(format!("{label} {}", preview(address, 128)));
                }
            }
            for (key, mode) in [("tproxy", "TPROXY"), ("redirect", "REDIRECT")] {
                if config.get(key).is_some() {
                    details.modes.push(mode.into());
                }
            }
            if let Some(outbounds) = config.get("outbounds").and_then(toml::Value::as_array) {
                details.outbound_count = outbounds.len();
                details.outbounds = outbounds
                    .iter()
                    .filter_map(|outbound| {
                        outbound
                            .get("tag")
                            .and_then(toml::Value::as_str)
                            .map(|tag| preview(tag, 96))
                    })
                    .take(4)
                    .collect();
            }
            if let Some(groups) = config.get("proxy_groups").and_then(toml::Value::as_table) {
                details.outbound_count += groups.len();
                details
                    .outbounds
                    .extend(groups.iter().take(4).map(|(name, group)| {
                        let mode = group
                            .get("mode")
                            .and_then(toml::Value::as_str)
                            .unwrap_or("select");
                        format!("{} [{}]", preview(name, 64), mode)
                    }));
            }
            if !config
                .get("proxy_groups")
                .and_then(toml::Value::as_table)
                .is_some_and(|g| g.contains_key("PROXY"))
            {
                details.outbound_count += 1;
                details.outbounds.push("PROXY [select]".into());
            }
        }
        details
    }
}

pub async fn serve(home: PathBuf, bin: PathBuf) -> Result<()> {
    serve_with_config(home.clone(), home.join("config"), bin).await
}
pub async fn serve_with_config(home: PathBuf, config_home: PathBuf, bin: PathBuf) -> Result<()> {
    private_dir(&home)?;
    let socket = home.join("manager.sock");
    let listener = astra_platform::platform::bind_listener(&socket)
        .context("Manager endpoint is active or inaccessible")?;
    set_private(&socket, 0o600)?;
    let mut manager = Manager::load_with_config(home, config_home, bin)?;
    manager.recover_processes().await?;
    server::run(manager, Some(listener), None).await
}
pub(crate) use server::NativeClient;
pub(crate) async fn start_native(
    home: PathBuf,
    config_home: PathBuf,
    bin: PathBuf,
    runtime_dir: Option<PathBuf>,
) -> Result<(NativeClient, tokio::task::JoinHandle<Result<()>>)> {
    let mut manager = Manager::load_with_runtime(home, config_home, bin, runtime_dir)?;
    manager.recover_processes().await?;
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let result = server::run(manager, None, Some(sender)).await;
        tokio::time::sleep(Duration::from_millis(250)).await;
        result
    });
    let client = receiver
        .await
        .context("Native actor failed to initialize")?;
    Ok((client, task))
}

#[cfg(test)]
mod tests;
