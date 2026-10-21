use crate::protocol::*;
use anyhow::{bail, Context, Result};
use astra_ipc::control::{self, Command};
use astra_ipc::platform::LocalListener as UnixListener;
use astra_ipc::platform::{set_private, DirBuilderExt, OpenOptionsExt};
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
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
struct Registry {
    #[serde(default)]
    instances: Vec<Instance>,
    #[serde(default)]
    log_levels: HashMap<String, LogLevel>,
    #[serde(default)]
    start_bindings: HashMap<String, Vec<String>>,
}
struct Running {
    child: ManagedChild,
    config: String,
    config_path: PathBuf,
}
enum ManagedChild {
    Normal(Child),
    #[cfg(windows)]
    Elevated(astra_ipc::elevation::ElevatedChild),
}
impl ManagedChild {
    fn id(&self) -> Option<u32> {
        match self {
            Self::Normal(child) => child.id(),
            #[cfg(windows)]
            Self::Elevated(child) => child.id(),
        }
    }
    fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        match self {
            Self::Normal(child) => child.try_wait(),
            #[cfg(windows)]
            Self::Elevated(child) => child.try_wait(),
        }
    }
    fn start_kill(&mut self) -> std::io::Result<()> {
        match self {
            Self::Normal(child) => child.start_kill(),
            #[cfg(windows)]
            Self::Elevated(child) => child.start_kill(),
        }
    }
    async fn wait(&mut self) -> std::io::Result<std::process::ExitStatus> {
        match self {
            Self::Normal(child) => child.wait().await,
            #[cfg(windows)]
            Self::Elevated(child) => child.wait().await,
        }
    }
    async fn kill(&mut self) -> std::io::Result<()> {
        match self {
            Self::Normal(child) => child.kill().await,
            #[cfg(windows)]
            Self::Elevated(child) => child.kill().await,
        }
    }
}
pub struct Manager {
    home: PathBuf,
    bin: PathBuf,
    registry: Registry,
    running: HashMap<String, Running>,
    exits: HashMap<String, String>,
}
const CONFIG_LIMIT: usize = 480 * 1024;

async fn prepare_control_endpoint(socket: &Path) -> Result<()> {
    #[cfg(windows)]
    if socket.exists() {
        match astra_ipc::platform::recover_stale_endpoint(socket) {
            Ok(true) => eprintln!("Recovered stale control endpoint: {}", socket.display()),
            Ok(false) => {}
            Err(error) => {
                if let Ok(status) = control::request(socket, Command::Status).await {
                    bail!("Control endpoint {} belongs to running core PID {}; it is not owned by this manager", socket.display(), status.pid);
                }
                return Err(error).with_context(|| format!(
                    "Control endpoint could not be safely recovered: {}; it may be active or inaccessible; the marker was retained", socket.display()));
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
    executable: PathBuf,
    config_path: PathBuf,
    text: String,
    running: bool,
    control: Option<(PathBuf, Option<u32>)>,
}
impl DiagnosticPlan {
    async fn stream(
        mut self,
        targets: Vec<String>,
        output: &mut (impl tokio::io::AsyncWrite + Unpin),
    ) -> Result<()> {
        use astra_ipc::diagnostics::Event;
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
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
        let payload = serde_json::to_vec(&astra_ipc::diagnostics::Request {
            config: self.text,
            targets: Some(targets),
            list_targets: false,
        })?;
        let mut child = core_process(self.executable)
            .arg("--diagnose-stream")
            .arg(self.config_path)
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
        let mut done = false;
        for _ in 0..20000 {
            let mut line = String::new();
            if (&mut reader).take(1024 * 1024).read_line(&mut line).await? == 0 {
                break;
            }
            anyhow::ensure!(line.ends_with('\n'), "Diagnostic event exceeds limit");
            let mut event: Event =
                serde_json::from_str(&line).context("Update proxy core to use live node tests")?;
            if let Event::Started { running_config, .. } = &mut event {
                *running_config = self.running;
            }
            if matches!(event, Event::Done) {
                done = true;
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
        anyhow::ensure!(status.success(), "Diagnostic core failed: {errors}");
        anyhow::ensure!(done, "Diagnostic stream ended before completion");
        astra_ipc::send_management(output, &Reply::text(serde_json::to_string(&Event::Done)?))
            .await?;
        Ok(())
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
                serde_json::to_vec(&astra_ipc::diagnostics::Request { config: self.text, targets, list_targets: list })?
            } else { self.text.into_bytes() };
            let mut child = core_process(self.executable)
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
            let mut report: astra_ipc::diagnostics::Report = serde_json::from_slice(&output.stdout)
                .context("Update the proxy core to use diagnostics")?;
            report.running_config = self.running;
            Ok(Reply::text(serde_json::to_string(&report)?))
        })
        .await
        .context("Diagnostic core timed out")?
    }
}

fn core_process(path: PathBuf) -> Process {
    astra_ipc::platform::background_core(path)
}

pub fn private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    anyhow::ensure!(
        !fs::symlink_metadata(path)?.file_type().is_symlink(),
        "State directory cannot be a symlink"
    );
    set_private(path, 0o700)?;
    Ok(())
}

pub(crate) fn atomic_write(path: &Path, data: &[u8]) -> Result<()> {
    let tmp = path.with_file_name(format!(
        ".astra-{}-{}.tmp",
        std::process::id(),
        path.file_name()
            .context("Missing filename")?
            .to_string_lossy()
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&tmp)?;
    let result = (|| {
        set_private(&tmp, 0o600)?;
        file.write_all(data)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path)?;
        Ok(())
    })();
    let _ = fs::remove_file(tmp);
    result
}

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
                tags.extend(groups.keys().cloned());
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
    fn load(home: PathBuf, bin: PathBuf) -> Result<Self> {
        let path = home.join("instances.toml");
        let registry = if path.exists() {
            toml::from_str(&fs::read_to_string(path)?)?
        } else {
            Registry::default()
        };
        Ok(Self {
            home,
            bin,
            registry,
            running: HashMap::new(),
            exits: HashMap::new(),
        })
    }
    fn persist(&self) -> Result<()> {
        atomic_write(
            &self.home.join("instances.toml"),
            toml::to_string_pretty(&self.registry)?.as_bytes(),
        )
    }
    fn instance(&self, name: &str) -> Result<Instance> {
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
            self.exits.insert(name.clone(), format!("Exited: {status}"));
            // This socket belongs to the child we just reaped, never to an arbitrary PID.
            let _ = fs::remove_file(self.socket(&name));
        }
        Ok(())
    }
    async fn validate(&self, kind: Kind, path: &Path) -> Result<()> {
        read_config(path)?;
        let output = timeout(
            Duration::from_secs(if kind == Kind::Rule { 90 } else { 20 }),
            core_process(self.bin.join(kind.binary()))
                .arg("--check")
                .arg(path)
                .env_remove("ASTRA_CONTROL_SOCKET")
                .kill_on_drop(true)
                .output(),
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
        fn visit(
            manager: &Manager,
            name: &str,
            active: &mut HashSet<String>,
            done: &mut HashSet<String>,
            plan: &mut Vec<String>,
        ) -> Result<()> {
            manager.instance(name)?;
            if done.contains(name) {
                return Ok(());
            }
            anyhow::ensure!(
                active.insert(name.to_owned()),
                "Startup binding cycle: {name}"
            );
            if let Some(targets) = manager.registry.start_bindings.get(name) {
                for target in targets {
                    visit(manager, target, active, done, plan)?;
                }
            }
            active.remove(name);
            done.insert(name.to_owned());
            plan.push(name.to_owned());
            Ok(())
        }
        let mut plan = Vec::new();
        let mut active = HashSet::new();
        let mut done = HashSet::new();
        visit(self, name, &mut active, &mut done, &mut plan)?;
        // Reverse links are expanded only for an explicitly started rule.
        // Starting one proxy never starts its sibling proxies.
        if self.instance(name)?.kind == Kind::Rule {
            for proxy in self.bound_proxies(name) {
                visit(self, &proxy, &mut active, &mut done, &mut plan)?;
            }
        }
        Ok(plan)
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
        self.reap()?;
        let plan = self.start_plan(name)?;
        // Validate the entire group before starting its first new process.
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
                let instance = self.instance(item)?;
                self.validate(instance.kind, &instance.config).await?;
            }
        }
        let mut started: Vec<String> = Vec::new();
        for item in plan {
            if self.running.contains_key(&item) {
                continue;
            }
            if let Err(error) = self.start_one(&item).await {
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
            started.push(item);
        }
        Ok(())
    }
    async fn start_one(&mut self, name: &str) -> Result<()> {
        self.reap()?;
        anyhow::ensure!(
            !self.running.contains_key(name),
            "Instance is already running"
        );
        let instance = self.instance(name)?;
        let socket = self.socket(name);
        prepare_control_endpoint(&socket).await?;
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(self.log(name))?;
        let config = read_config(&instance.config)?;
        #[cfg(windows)]
        let elevate = instance.kind == Kind::Proxy
            && toml::from_str::<toml::Value>(&config)?.get("tun").is_some()
            && !astra_ipc::elevation::is_elevated()?;
        #[cfg(not(windows))]
        let elevate = false;
        #[cfg(windows)]
        let elevated = if elevate {
            let exe = self.bin.join(instance.kind.binary());
            let config = fs::canonicalize(&instance.config)?;
            let args = astra_ipc::elevation::core_arguments(
                &config,
                Some(&socket),
                &self.log(name),
                self.level(name).as_str(),
            )?;
            Some(ManagedChild::Elevated(
                tokio::task::spawn_blocking(move || astra_ipc::elevation::spawn(&exe, &args))
                    .await??,
            ))
        } else {
            None
        };
        #[cfg(not(windows))]
        let elevated: Option<ManagedChild> = None;
        let child = if let Some(child) = elevated {
            child
        } else {
            ManagedChild::Normal(
                core_process(self.bin.join(instance.kind.binary()))
                    .arg(&instance.config)
                    .env("ASTRA_CONTROL_SOCKET", &socket)
                    .env("ASTRA_LOG_LEVEL", self.level(name).as_str())
                    .stdin(Stdio::null())
                    .stdout(log.try_clone()?)
                    .stderr(log)
                    .kill_on_drop(true)
                    .spawn()?,
            )
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
        let deadline = tokio::time::Instant::now()
            + Duration::from_secs(if instance.kind == Kind::Rule {
                60
            } else if elevate {
                30
            } else {
                10
            });
        while tokio::time::Instant::now() < deadline {
            self.reap()?;
            let run = self
                .running
                .get(name)
                .context("Core exited during startup; open logs for details")?;
            if let Ok(status) = control::request(&socket, Command::Status).await {
                anyhow::ensure!(Some(status.pid) == run.child.id(), "Core PID mismatch");
                return Ok(());
            }
            sleep(Duration::from_millis(50)).await;
        }
        self.stop(name).await?;
        bail!("Core did not become ready; open logs for details")
    }
    async fn reload(&mut self, name: &str, path: &Path) -> Result<()> {
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
        Ok(())
    }
    async fn stop(&mut self, name: &str) -> Result<()> {
        self.reap()?;
        self.instance(name)?;
        if !self.running.contains_key(name) {
            return Ok(());
        }
        // Acknowledged IPC stop uses the same drain path as SIGTERM.
        let response = control::request(&self.socket(name), Command::Stop).await;
        let run = self.running.get_mut(name).unwrap();
        if response.is_err() {
            // Only terminate an owned child, never a persisted PID.
            run.child.start_kill()?;
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
        let _ = fs::remove_file(self.socket(name));
        Ok(())
    }
    async fn stop_all(&mut self) {
        // Drain proxies while their rule cores are still available.
        for kind in [Kind::Proxy, Kind::Rule] {
            let names: Vec<_> = self
                .registry
                .instances
                .iter()
                .filter(|i| i.kind == kind)
                .map(|i| i.name.clone())
                .collect();
            for name in names {
                let _ = self.stop(&name).await;
            }
        }
    }
    fn level(&self, name: &str) -> LogLevel {
        self.registry
            .log_levels
            .get(name)
            .copied()
            .unwrap_or_default()
    }
    fn check_name(&self, name: &str) -> Result<()> {
        anyhow::ensure!(
            !name.is_empty()
                && name.len() <= 24
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
            "Name: 1–24 ASCII letters, digits, '-' or '_'"
        );
        anyhow::ensure!(
            name != "manager",
            "The name manager is reserved for the management service"
        );
        anyhow::ensure!(self.instance(name).is_err(), "Instance name already exists");
        anyhow::ensure!(self.registry.instances.len() < 64, "Maximum 64 instances");
        Ok(())
    }
    async fn add(&mut self, mut i: Instance) -> Result<Reply> {
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
        self.check_name(&name)?;
        let directory = self.home.join("configs");
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
    async fn action(&mut self, action: Action) -> Result<Reply> {
        self.reap()?;
        match action {
            Action::List => {
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
            Action::Add(i) => self.add(i).await,
            Action::Create { name, kind } => self.create(name, kind).await,
            Action::FollowLogs { .. } => bail!("Log subscriptions require a dedicated connection"),
            Action::Diagnostics(_)
            | Action::DiagnosticTargets(_)
            | Action::DiagnosticsSelected { .. } => {
                bail!("Diagnostics require a dedicated connection")
            }
            Action::DiagnosticsStream { .. } => bail!("Diagnostics require a dedicated connection"),
            Action::NodeTags(name) => {
                let json = serde_json::to_string(&self.node_tags(&name)?)?;
                anyhow::ensure!(
                    json.len() < 900 * 1024,
                    "Node suggestions exceed response limit"
                );
                Ok(Reply::text(json))
            }
            Action::SubscriptionNodes(name) => {
                let instance = self.instance(&name)?;
                anyhow::ensure!(instance.kind == Kind::Proxy, "Select a proxy instance");
                Ok(Reply::text(serde_json::to_string(
                    &crate::subscription_view::read(&instance.config)?,
                )?))
            }
            Action::ExplainRoute { name, request } => {
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
            Action::GetLogLevel(name) => {
                self.instance(&name)?;
                Ok(Reply::text(self.level(&name).as_str()))
            }
            Action::SetLogLevel { name, level } => {
                self.instance(&name)?;
                let previous = self.level(&name);
                if self.running.contains_key(&name) {
                    control::request(&self.socket(&name), Command::SetLogLevel(level))
                        .await
                        .context("Update core log level")?;
                }
                self.registry.log_levels.insert(name.clone(), level);
                if let Err(error) = self.persist() {
                    self.registry.log_levels.insert(name.clone(), previous);
                    if self.running.contains_key(&name) {
                        let _ =
                            control::request(&self.socket(&name), Command::SetLogLevel(previous))
                                .await;
                    }
                    return Err(error);
                }
                Ok(Reply::text(level.as_str()))
            }
            Action::BindStart { name, targets } => {
                self.bind_start(&name, targets)?;
                Ok(Reply::text("Startup bindings saved"))
            }
            Action::Remove(name) => {
                anyhow::ensure!(
                    !self.running.contains_key(&name),
                    "Stop the instance before removing it"
                );
                self.instance(&name)?;
                let old = self.registry.clone();
                self.registry.instances.retain(|i| i.name != name);
                self.registry.log_levels.remove(&name);
                self.registry.start_bindings.remove(&name);
                for targets in self.registry.start_bindings.values_mut() {
                    targets.retain(|target| target != &name);
                }
                if let Err(e) = self.persist() {
                    self.registry = old;
                    return Err(e);
                }
                Ok(Reply::text("Instance removed; configuration retained"))
            }
            Action::Start(name) => {
                self.start(&name).await?;
                Ok(Reply::text("Started"))
            }
            Action::Stop(name) => {
                self.stop(&name).await?;
                Ok(Reply::text("Stopped"))
            }
            Action::UpdateSubscriptions(name) => {
                anyhow::ensure!(
                    self.instance(&name)?.kind == Kind::Proxy,
                    "Select a proxy instance"
                );
                if !self.running.contains_key(&name) {
                    return self.diagnostic_plan(&name)?.run_refresh().await;
                }
                astra_ipc::control::request(&self.socket(&name), Command::UpdateSubscriptions)
                    .await?;
                Ok(Reply::text("Subscription update scheduled"))
            }
            Action::Reload(name) => {
                let i = self.instance(&name)?;
                self.reload(&name, &i.config).await?;
                Ok(Reply::text(
                    "Configuration hot reloaded; existing sessions retained",
                ))
            }
            Action::Restart(name) => {
                let i = self.instance(&name)?;
                self.validate(i.kind, &i.config).await?;
                for target in self.start_plan(&name)? {
                    if !self.running.contains_key(&target) {
                        let instance = self.instance(&target)?;
                        self.validate(instance.kind, &instance.config).await?;
                    }
                }
                self.stop(&name).await?;
                self.start(&name).await?;
                Ok(Reply::text("Restarted with current configuration"))
            }
            Action::SetConfig(name, path) => {
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
            Action::Read(name) => {
                let i = self.instance(&name)?;
                ensure_config(i.kind, &i.config)?;
                Ok(Reply::text(read_config(&i.config)?))
            }
            Action::PrepareConfig(name) => {
                let i = self.instance(&name)?;
                let created = ensure_config(i.kind, &i.config)?;
                Ok(Reply::text(if created {
                    read_config(&i.config)?
                } else {
                    String::new()
                }))
            }
            Action::Save {
                name,
                original,
                text,
            } => {
                anyhow::ensure!(text.len() <= CONFIG_LIMIT, "Configuration exceeds 480 KiB");
                let i = self.instance(&name)?;
                anyhow::ensure!(
                    read_config(&i.config)? == original,
                    "File changed on disk; reopen before editing"
                );
                // Validate beside the original, preserving relative TLS/CA paths.
                let temp = i
                    .config
                    .with_file_name(format!(".astra-check-{}.toml", std::process::id()));
                let mut file = OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .mode(0o600)
                    .open(&temp)?;
                let checked = async {
                    file.write_all(text.as_bytes())?;
                    file.sync_all()?;
                    self.validate(i.kind, &temp).await
                }
                .await;
                let _ = fs::remove_file(&temp);
                checked?;
                anyhow::ensure!(
                    read_config(&i.config)? == original,
                    "File changed during validation; reopen before editing"
                );
                let backup = i.config.with_extension("toml.bak");
                atomic_write(&backup, original.as_bytes())?;
                atomic_write(&i.config, text.as_bytes())?;
                if self.running.contains_key(&name) {
                    if let Err(error) = self.reload(&name, &i.config).await {
                        atomic_write(&i.config, original.as_bytes())
                            .context("restore file after rejected reload")?;
                        return Err(error).context("hot reload rejected; original file restored");
                    }
                }
                Ok(Reply::text(
                    "Validated and saved; hot applied when running; previous file in .toml.bak",
                ))
            }
            Action::CloneConfig(name, destination) => {
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
            Action::Validate(name) => {
                let i = self.instance(&name)?;
                self.validate(i.kind, &i.config).await?;
                Ok(Reply::text("Configuration valid"))
            }
            Action::Logs(name) => {
                self.instance(&name)?;
                let mut file = fs::File::open(self.log(&name))
                    .context("No log yet; start this instance first")?;
                let size = file.metadata()?.len();
                file.seek(SeekFrom::Start(size.saturating_sub(24 * 1024)))?;
                let mut bytes = vec![];
                file.take(24 * 1024).read_to_end(&mut bytes)?;
                Ok(Reply::text(String::from_utf8_lossy(&bytes)))
            }
            Action::Shutdown => {
                self.stop_all().await;
                Ok(Reply::text("Manager and all instances stopped"))
            }
        }
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
        let mut details = InstanceDetails {
            node_tags_supported: true,
            subscription_nodes_supported: true,
            diagnostics_supported: true,
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
            for (key, label) in [("socks5_listen", "SOCKS5"), ("http_listen", "HTTP")] {
                if let Some(address) = config.get(key).and_then(toml::Value::as_str) {
                    details
                        .listeners
                        .push(format!("{label} {}", preview(address, 128)));
                }
            }
            for (key, mode) in [
                ("tun", "TUN"),
                ("system_proxy", "System proxy"),
                ("tproxy", "TPROXY"),
                ("redirect", "REDIRECT"),
            ] {
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

#[cfg_attr(windows, allow(dead_code))]
struct SocketGuard(PathBuf);
impl Drop for SocketGuard {
    fn drop(&mut self) {
        astra_ipc::platform::remove_socket(&self.0);
    }
}

pub async fn serve(home: PathBuf, bin: PathBuf) -> Result<()> {
    private_dir(&home)?;
    let socket = home.join("manager.sock");
    let listener = UnixListener::bind(&socket).context("Manager socket exists or path is too long; never remove a socket belonging to a running manager")?;
    let _guard = SocketGuard(socket.clone());
    set_private(&socket, 0o600)?;
    let mut manager = Manager::load(home, bin)?;
    let mut followers = tokio::task::JoinSet::new();
    let mut diagnostics = tokio::task::JoinSet::new();
    let signal = astra_ipc::shutdown_signal();
    tokio::pin!(signal);
    let result: Result<()> = async {
        loop {
            tokio::select! {
                result = &mut signal => { result?; break; }
                connection = listener.accept() => {
                    let (mut stream, _) = connection?;
                    let request = timeout(Duration::from_secs(2), astra_ipc::receive_management::<Request>(&mut stream)).await;
                    let request = match request { Ok(Ok(r)) => r, _ => continue };
                    if request.version == astra_ipc::API_VERSION {
                        if let Action::DiagnosticsStream {name, targets} = &request.action {
                            let plan = (|| { anyhow::ensure!(diagnostics.len() < 2, "Diagnostics are busy; retry shortly"); manager.diagnostic_plan(name) })();
                            let targets = targets.clone();
                            let (mut input, mut output) = tokio::io::split(stream);
                            diagnostics.spawn(async move {
                                use tokio::io::AsyncReadExt;
                                let result = tokio::select! {
                                    result = async { timeout(Duration::from_secs(45), plan?.stream(targets, &mut output)).await.context("Diagnostic stream timed out")? } => result,
                                    _ = input.read_u8() => return,
                                };
                                if let Err(error) = result {
                                    let mut reply = Reply::text(""); reply.error = Some(format!("{error:#}"));
                                    let _ = timeout(Duration::from_secs(2), astra_ipc::send_management(&mut output, &reply)).await;
                                }
                            });
                            continue;
                        }
                        let offline_refresh = matches!(&request.action, Action::UpdateSubscriptions(name) if !manager.running.contains_key(name));
                        if matches!(&request.action, Action::Diagnostics(_) | Action::DiagnosticTargets(_) | Action::DiagnosticsSelected {..}) || offline_refresh {
                            let name = match &request.action { Action::Diagnostics(name) | Action::DiagnosticTargets(name) | Action::DiagnosticsSelected {name, ..} | Action::UpdateSubscriptions(name) => name, _ => unreachable!() };
                            let list = matches!(&request.action, Action::DiagnosticTargets(_));
                            let targets = if let Action::DiagnosticsSelected {targets, ..} = &request.action { Some(targets.clone()) } else { None };
                            let plan = (|| {
                                anyhow::ensure!(diagnostics.len() < 2, "Diagnostics are busy; retry shortly");
                                manager.diagnostic_plan(name)
                            })();
                            match plan {
                                Ok(plan) => {
                                    diagnostics.spawn(async move {
                                        use tokio::io::AsyncReadExt;
                                        let result = tokio::select! {
                                            result = async { if offline_refresh { plan.run_refresh().await } else if !list && targets.is_none() { plan.run().await } else { plan.run_selection(targets, list).await } } => result,
                                            _ = stream.read_u8() => return,
                                        };
                                        let reply = result.unwrap_or_else(|error| {
                                            let mut reply = Reply::text("");
                                            reply.error = Some(format!("{error:#}")); reply
                                        });
                                        let _ = timeout(Duration::from_secs(2), astra_ipc::send_management(&mut stream, &reply)).await;
                                    });
                                }
                                Err(error) => {
                                    let mut reply = Reply::text(""); reply.error = Some(format!("{error:#}"));
                                    let _ = timeout(Duration::from_secs(2), astra_ipc::send_management(&mut stream, &reply)).await;
                                }
                            }
                            continue;
                        }
                        if let Action::FollowLogs{name,cursor} = &request.action {
                            let result = manager.instance(name).and_then(|_| {
                                anyhow::ensure!(followers.len() < 16,"Too many log subscribers"); Ok(manager.log(name))
                            });
                            match result {
                                Ok(path) => {
                                    if matches!(timeout(Duration::from_secs(2),astra_ipc::send_management(&mut stream,&Reply::text("Streaming logs"))).await,Ok(Ok(()))) {
                                        followers.spawn(crate::log_stream::serve(stream,path,*cursor));
                                    }
                                }
                                Err(error) => { let mut reply = Reply::text(""); reply.error = Some(format!("{error:#}")); let _ = timeout(Duration::from_secs(2),astra_ipc::send_management(&mut stream,&reply)).await; }
                            }
                            continue;
                        }
                    }
                    let shutdown = request.version == astra_ipc::API_VERSION && matches!(request.action, Action::Shutdown);
                    let result = if request.version != astra_ipc::API_VERSION { Err(anyhow::anyhow!("Unsupported manager version")) } else { manager.action(request.action).await };
                    let reply = result.unwrap_or_else(|e| { let mut r = Reply::text(""); r.error = Some(format!("{e:#}")); r });
                    let _ = timeout(Duration::from_secs(2), astra_ipc::send_management(&mut stream, &reply)).await;
                    if shutdown { break; }
                }
                _ = sleep(Duration::from_millis(500)) => { manager.reap()?; }
                Some(_) = followers.join_next(), if !followers.is_empty() => {},
                Some(_) = diagnostics.join_next(), if !diagnostics.is_empty() => {},
            }
        }
        Ok(())
    }.await;
    manager.stop_all().await;
    followers.abort_all();
    while followers.join_next().await.is_some() {}
    diagnostics.abort_all();
    while diagnostics.join_next().await.is_some() {}
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    struct Temp(PathBuf);
    impl Temp {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "at-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Temp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }
    #[cfg(windows)]
    #[tokio::test]
    async fn control_endpoint_recovery_preserves_live_and_unknown_owners() {
        let temp = Temp::new();
        let path = temp.0.join("t2.sock");
        prepare_control_endpoint(&path).await.unwrap();
        fs::write(&path, b"").unwrap();
        prepare_control_endpoint(&path).await.unwrap();
        assert!(!path.exists());
        fs::write(&path, b"not a marker").unwrap();
        assert!(prepare_control_endpoint(&path).await.is_err());
        assert_eq!(fs::read(&path).unwrap(), b"not a marker");
        fs::remove_file(&path).unwrap();
        let control = control::Control::bind(Some(path.clone())).unwrap();
        let server = tokio::spawn(async move { control.shutdown().await });
        let error = prepare_control_endpoint(&path).await.unwrap_err();
        assert!(format!("{error:#}").contains(&format!("running core PID {}", std::process::id())));
        assert!(path.exists());
        assert_eq!(
            control::request(&path, Command::Status).await.unwrap().pid,
            std::process::id()
        );
        control::request(&path, Command::Stop).await.unwrap();
        server.await.unwrap().unwrap();
        let unresponsive = UnixListener::bind(&path).unwrap();
        assert!(prepare_control_endpoint(&path).await.is_err());
        assert!(
            path.exists(),
            "an unresponsive server still owns its endpoint"
        );
        drop(unresponsive);
    }
    #[tokio::test]
    async fn node_suggestions_use_full_tags_from_bound_proxies_and_never_include_credentials() {
        let temp = Temp::new();
        let mut manager = Manager::load(temp.0.clone(), PathBuf::new()).unwrap();
        for (name, kind) in [
            ("r1", Kind::Rule),
            ("r2", Kind::Rule),
            ("p1", Kind::Proxy),
            ("p2", Kind::Proxy),
        ] {
            manager.registry.instances.push(Instance {
                name: name.into(),
                kind,
                config: temp.0.join(format!("{name}.toml")),
            });
        }
        let mut config = String::from("standalone=true\n");
        for i in 0..9 {
            config.push_str(&format!("[[outbounds]]\nkind='socks5'\ntag='节点-{i}'\nserver='127.0.0.1:1080'\npassword='secret-should-not-leak'\n"));
        }
        fs::write(temp.0.join("p1.toml"), config).unwrap();
        fs::write(
            temp.0.join("p2.toml"),
            "[[outbounds]]\nkind='socks5'\ntag='other'\nserver='127.0.0.1:1080'\n",
        )
        .unwrap();
        assert_eq!(manager.node_tags("r1").unwrap().len(), 11);
        manager.bind_start("p1", vec!["r1".into()]).unwrap();
        manager.bind_start("p2", vec!["r2".into()]).unwrap();
        let reply = manager.action(Action::NodeTags("r1".into())).await.unwrap();
        assert!(!reply.text.contains("secret-should-not-leak"));
        let tags: Vec<String> = serde_json::from_str(&reply.text).unwrap();
        assert_eq!(tags.len(), 10);
        assert!(tags.contains(&"节点-8".into()));
        assert!(!tags.contains(&"other".into()));
        assert_eq!(manager.node_tags("r2").unwrap(), ["PROXY", "other"]);
        assert_eq!(manager.node_tags("p2").unwrap(), ["PROXY", "other"]);
        assert!(manager.node_tags("missing").is_err());
        let path = temp.0.join("p1.toml");
        let mut text = fs::read_to_string(&path).unwrap();
        text.push_str("\n[proxy_groups.choose]\nmode='select'\nmembers=['节点-0','节点-8']\n");
        fs::write(&path, text).unwrap();
        assert!(manager.node_tags("r1").unwrap().contains(&"choose".into()));

        fs::write(temp.0.join("p2.toml"), "invalid TOML").unwrap();
        assert!(manager.node_tags("r2").unwrap().is_empty());
    }
    #[test]
    fn diagnostic_plan_freezes_saved_configuration_and_rejects_rule_instances() {
        let temp = Temp::new();
        let path = temp.0.join("proxy.toml");
        fs::write(&path, "standalone=true\n").unwrap();
        let mut manager = Manager::load(temp.0.clone(), PathBuf::from("unused")).unwrap();
        manager.registry.instances.push(Instance {
            name: "proxy".into(),
            kind: Kind::Proxy,
            config: path.clone(),
        });
        let plan = manager.diagnostic_plan("proxy").unwrap();
        fs::write(&path, "changed after request").unwrap();
        assert_eq!(plan.text, "standalone=true\n");
        assert!(!plan.running);
        manager.registry.instances[0].kind = Kind::Rule;
        assert!(manager.diagnostic_plan("proxy").is_err());
    }
    #[tokio::test]
    #[ignore = "requires freshly built core binaries"]
    async fn live_diagnostics_stream_updates_early_and_cancel_closes_core() {
        use astra_ipc::diagnostics::Event;
        use tokio::io::AsyncReadExt;
        let temp = Temp::new();
        let home = temp.0.join("state");
        let socket = home.join("manager.sock");
        let bin = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_owned();
        let slow = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = slow.local_addr().unwrap();
        let fast = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fast_address = fast.local_addr().unwrap();
        let peer = tokio::spawn(async move {
            let (mut stream, _) = slow.accept().await.unwrap();
            let mut greeting = [0; 3];
            stream.read_exact(&mut greeting).await.unwrap();
            assert!(stream.read_u8().await.is_err());
        });
        let quick = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut stream, _) = fast.accept().await.unwrap();
                let mut greeting = [0; 3];
                stream.read_exact(&mut greeting).await.unwrap();
            }
        });
        let source = temp.0.join("proxy.toml");
        fs::write(&source,format!("standalone=true\nstandalone_outbound='slow'\nsocks5_listen='127.0.0.1:1080'\n[[outbounds]]\nkind='socks5'\ntag='slow'\nserver='{address}'\n[[outbounds]]\nkind='socks5'\ntag='fast'\nserver='{fast_address}'\n")).unwrap();
        let manager = tokio::spawn(serve(home, bin));
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
        call(
            &socket,
            Action::Add(Instance {
                name: "proxy".into(),
                kind: Kind::Proxy,
                config: source,
            }),
        )
        .await
        .unwrap();
        let mut stream = astra_ipc::platform::LocalStream::connect(&socket)
            .await
            .unwrap();
        astra_ipc::send_management(
            &mut stream,
            &Request {
                version: astra_ipc::API_VERSION,
                action: Action::DiagnosticsStream {
                    name: "proxy".into(),
                    targets: vec!["slow".into(), "fast".into()],
                },
            },
        )
        .await
        .unwrap();
        timeout(Duration::from_secs(2), async {
            loop {
                let reply: Reply = astra_ipc::receive_management(&mut stream).await.unwrap();
                assert!(reply.error.is_none(), "{:?}", reply.error);
                if let Event::Finished { probe } =
                    serde_json::from_str::<Event>(&reply.text).unwrap()
                {
                    assert_eq!(probe.outbound, "fast");
                    break;
                }
            }
        })
        .await
        .expect("fast result must arrive while slow node is still pending");
        timeout(Duration::from_secs(1), call(&socket, Action::List))
            .await
            .unwrap()
            .unwrap();
        drop(stream);
        timeout(Duration::from_secs(2), peer)
            .await
            .expect("cancel must close the diagnostic child tunnel")
            .unwrap();
        let mut stream = astra_ipc::platform::LocalStream::connect(&socket)
            .await
            .unwrap();
        astra_ipc::send_management(
            &mut stream,
            &Request {
                version: astra_ipc::API_VERSION,
                action: Action::DiagnosticsStream {
                    name: "proxy".into(),
                    targets: vec!["fast".into()],
                },
            },
        )
        .await
        .unwrap();
        timeout(Duration::from_secs(3), async {
            loop {
                let reply: Reply = astra_ipc::receive_management(&mut stream).await.unwrap();
                assert!(reply.error.is_none(), "{:?}", reply.error);
                if matches!(
                    serde_json::from_str::<Event>(&reply.text).unwrap(),
                    Event::Done
                ) {
                    break;
                }
            }
        })
        .await
        .unwrap();
        quick.await.unwrap();
        call(&socket, Action::Shutdown).await.unwrap();
        manager.await.unwrap().unwrap();
    }
    #[tokio::test]
    #[ignore = "requires freshly built core binaries"]
    async fn live_stopped_subscription_update_keeps_manager_responsive() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let temp = Temp::new();
        let home = temp.0.join("state");
        let socket = home.join("manager.sock");
        let bin = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_owned();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let fake = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                request.push(stream.read_u8().await.unwrap());
            }
            tx.send(()).unwrap();
            sleep(Duration::from_millis(300)).await;
            let body = "proxies: [{name: fetched, type: http, server: 127.0.0.1, port: 1234}]";
            stream
                .write_all(
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .as_bytes(),
                )
                .await
                .unwrap();
        });
        let source = temp.0.join("proxy.toml");
        let text = format!("standalone=true\nsocks5_listen='127.0.0.1:1080'\n[subscriptions.remote]\nurl='http://{address}/sub'\n");
        fs::write(&source, &text).unwrap();
        let manager = tokio::spawn(serve(home, bin));
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
        call(
            &socket,
            Action::Add(Instance {
                name: "proxy".into(),
                kind: Kind::Proxy,
                config: source.clone(),
            }),
        )
        .await
        .unwrap();
        let first_socket = socket.clone();
        let update = tokio::spawn(async move {
            call(&first_socket, Action::UpdateSubscriptions("proxy".into())).await
        });
        timeout(Duration::from_secs(5), rx).await.unwrap().unwrap();
        let status = timeout(Duration::from_millis(200), call(&socket, Action::List))
            .await
            .unwrap()
            .unwrap();
        assert!(status.rows[0].pid.is_none());
        assert!(update.await.unwrap().unwrap().text.contains("Updated 1"));
        let nodes = call(&socket, Action::SubscriptionNodes("proxy".into()))
            .await
            .unwrap();
        assert!(nodes.text.contains("fetched"));
        assert_eq!(fs::read_to_string(source).unwrap(), text);
        call(&socket, Action::Shutdown).await.unwrap();
        fake.await.unwrap();
        manager.await.unwrap().unwrap();
    }
    #[tokio::test]
    #[ignore = "requires freshly built core binaries"]
    async fn live_diagnostics_use_running_snapshot_keep_manager_responsive_and_cancel_child() {
        use tokio::io::AsyncReadExt;
        let temp = Temp::new();
        let home = temp.0.join("state");
        let socket = home.join("manager.sock");
        let bin = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream = listener.local_addr().unwrap();
        let (accepted_tx, mut accepted_rx) = tokio::sync::mpsc::channel(2);
        let fake = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut greeting = [0; 3];
                stream.read_exact(&mut greeting).await.unwrap();
                accepted_tx.send(()).await.unwrap();
                assert!(stream.read_u8().await.is_err()); // timeout/cancel closes the outbound.
            }
        });
        let source = temp.0.join("proxy.toml");
        let inbound = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = inbound.local_addr().unwrap();
        drop(inbound);
        fs::write(&source, format!("standalone=true\nsocks5_listen='{address}'\n[[outbounds]]\nkind='socks5'\ntag='edge'\nserver='{upstream}'\n")).unwrap();
        let manager = tokio::spawn(serve(home, bin));
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
        call(
            &socket,
            Action::Add(Instance {
                name: "proxy".into(),
                kind: Kind::Proxy,
                config: source.clone(),
            }),
        )
        .await
        .unwrap();
        call(&socket, Action::Start("proxy".into())).await.unwrap();
        let reloaded = fs::read_to_string(&source)
            .unwrap()
            .replace("tag='edge'", "tag='live-edge'");
        fs::write(&source, reloaded).unwrap();
        // Reload the core directly, bypassing the manager's cached configuration.
        control::request(
            &socket.with_file_name("proxy.sock"),
            Command::Reload { path: None },
        )
        .await
        .unwrap();
        fs::write(
            &source,
            "invalid modified file; running core must still be used",
        )
        .unwrap();
        let catalog = call(&socket, Action::DiagnosticTargets("proxy".into()))
            .await
            .unwrap();
        let targets: Vec<String> = serde_json::from_str(&catalog.text).unwrap();
        assert_eq!(targets, ["DIRECT", "live-edge", "PROXY"]);
        assert!(
            accepted_rx.try_recv().is_err(),
            "Listing nodes must not start probes"
        );
        let first_socket = socket.clone();
        let first = tokio::spawn(async move {
            call(
                &first_socket,
                Action::DiagnosticsSelected {
                    name: "proxy".into(),
                    targets: vec!["live-edge".into()],
                },
            )
            .await
        });
        timeout(Duration::from_secs(5), accepted_rx.recv())
            .await
            .unwrap()
            .unwrap();
        let list = timeout(Duration::from_secs(1), call(&socket, Action::List))
            .await
            .unwrap()
            .unwrap();
        assert!(list.rows[0].pid.is_some());
        let reply = timeout(Duration::from_secs(12), first)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let report: astra_ipc::diagnostics::Report = serde_json::from_str(&reply.text).unwrap();
        assert!(report.running_config);
        assert_eq!(report.probes.len(), 1);
        assert_eq!(report.probes[0].outbound, "live-edge");
        assert!(!report.probes[0].direct);
        assert!(report.probes[0].ip.is_none());
        assert!(report.probes[0].error.is_some());
        let second_socket = socket.clone();
        let second = tokio::spawn(async move {
            call(
                &second_socket,
                Action::DiagnosticsSelected {
                    name: "proxy".into(),
                    targets: vec!["live-edge".into()],
                },
            )
            .await
        });
        timeout(Duration::from_secs(5), accepted_rx.recv())
            .await
            .unwrap()
            .unwrap();
        second.abort();
        let _ = second.await;
        timeout(Duration::from_secs(3), fake)
            .await
            .unwrap()
            .unwrap();
        call(&socket, Action::Shutdown).await.unwrap();
        manager.await.unwrap().unwrap();
    }
    #[test]
    fn startup_bindings_are_proxy_to_one_rule_and_persistent() {
        let temp = Temp::new();
        let mut manager = Manager::load(temp.0.clone(), PathBuf::from("/unused")).unwrap();
        for (name, kind) in [
            ("proxy", Kind::Proxy),
            ("other-proxy", Kind::Proxy),
            ("rules-a", Kind::Rule),
            ("rules-b", Kind::Rule),
        ] {
            manager.registry.instances.push(Instance {
                name: name.into(),
                kind,
                config: temp.0.join(format!("{name}.toml")),
            });
        }
        manager
            .bind_start("proxy", vec!["rules-a".into(), "rules-a".into()])
            .unwrap();
        manager
            .bind_start("other-proxy", vec!["rules-a".into()])
            .unwrap();
        assert_eq!(manager.start_plan("proxy").unwrap(), ["rules-a", "proxy"]);
        assert!(manager
            .bind_start("proxy", vec!["rules-a".into(), "rules-b".into()])
            .is_err());
        assert!(manager
            .bind_start("proxy", vec!["other-proxy".into()])
            .is_err());
        assert!(manager
            .bind_start("rules-b", vec!["rules-a".into()])
            .is_err());
        assert!(manager.bind_start("proxy", vec!["missing".into()]).is_err());
        let restored = Manager::load(temp.0.clone(), PathBuf::from("/unused")).unwrap();
        assert_eq!(restored.start_plan("proxy").unwrap(), ["rules-a", "proxy"]);
        manager
            .registry
            .start_bindings
            .insert("rules-a".into(), vec!["proxy".into()]);
        assert!(manager.start_plan("proxy").is_err());
        manager.bind_start("rules-a", Vec::new()).unwrap();
        manager.bind_start("proxy", Vec::new()).unwrap();
        assert_eq!(manager.start_plan("proxy").unwrap(), ["proxy"]);
    }
    #[test]
    fn rule_bindings_edit_the_same_relation_and_start_rule_before_all_proxies() {
        let temp = Temp::new();
        let mut manager = Manager::load(temp.0.clone(), PathBuf::from("/unused")).unwrap();
        for (name, kind) in [
            ("rule-a", Kind::Rule),
            ("rule-b", Kind::Rule),
            ("proxy-a", Kind::Proxy),
            ("proxy-b", Kind::Proxy),
        ] {
            manager.registry.instances.push(Instance {
                name: name.into(),
                kind,
                config: temp.0.join(format!("{name}.toml")),
            });
        }
        manager
            .bind_start(
                "rule-a",
                vec!["proxy-b".into(), "proxy-a".into(), "proxy-a".into()],
            )
            .unwrap();
        assert_eq!(
            manager.start_plan("rule-a").unwrap(),
            ["rule-a", "proxy-a", "proxy-b"]
        );
        assert_eq!(
            manager.start_plan("proxy-a").unwrap(),
            ["rule-a", "proxy-a"]
        );
        assert_eq!(
            manager
                .details(&manager.instance("rule-a").unwrap())
                .bindings,
            ["proxy-a", "proxy-b"]
        );
        assert_eq!(
            manager
                .details(&manager.instance("proxy-a").unwrap())
                .bindings,
            ["rule-a"]
        );
        assert!(manager.bind_start("rule-a", vec!["rule-b".into()]).is_err());
        assert!(manager
            .bind_start("rule-a", vec!["missing".into()])
            .is_err());
        assert_eq!(manager.bound_proxies("rule-a"), ["proxy-a", "proxy-b"]);
        manager
            .bind_start("rule-b", vec!["proxy-b".into()])
            .unwrap();
        assert_eq!(manager.bound_proxies("rule-a"), ["proxy-a"]);
        let restored = Manager::load(temp.0.clone(), PathBuf::from("/unused")).unwrap();
        assert_eq!(
            restored.start_plan("rule-b").unwrap(),
            ["rule-b", "proxy-b"]
        );
        manager.bind_start("rule-a", Vec::new()).unwrap();
        assert_eq!(manager.start_plan("proxy-a").unwrap(), ["proxy-a"]);
        assert_eq!(
            manager.start_plan("proxy-b").unwrap(),
            ["rule-b", "proxy-b"]
        );
    }
    #[test]
    fn instance_summary_contains_endpoints_modes_tags_and_no_credentials() {
        let temp = Temp::new();
        let config = temp.0.join("proxy.toml");
        fs::write(&config, "standalone=true\nhttp_listen='127.0.0.1:8080'\n[tun]\nname='Astra'\n[[outbounds]]\ntag='edge'\nkind='socks5'\nserver='127.0.0.1:1080'\npassword='do-not-expose'\n").unwrap();
        let mut manager = Manager::load(temp.0.clone(), PathBuf::from("/unused")).unwrap();
        manager
            .registry
            .start_bindings
            .insert("proxy".into(), vec!["rules".into()]);
        let instance = Instance {
            name: "proxy".into(),
            kind: Kind::Proxy,
            config,
        };
        let details = manager.details(&instance);
        assert_eq!(details.modes, ["Standalone", "TUN"]);
        assert_eq!(details.listeners, ["HTTP 127.0.0.1:8080"]);
        assert_eq!(details.outbounds, ["edge", "PROXY [select]"]);
        assert_eq!(details.outbound_count, 2);
        assert_eq!(details.bindings, ["rules"]);
        assert!(!serde_json::to_string(&details)
            .unwrap()
            .contains("do-not-expose"));
        let large = format!(
            "standalone=true\n{}",
            format!("[[outbounds]]\ntag='{}'\n", "a".repeat(40000)).repeat(12)
        );
        fs::write(&instance.config, large).unwrap();
        let summary = manager.details(&instance);
        assert_eq!(summary.outbound_count, 13);
        assert!(
            serde_json::to_vec(&summary).unwrap().len() < 4096,
            "List metadata must stay bounded even with very long node tags"
        );
    }
    #[tokio::test]
    #[ignore = "requires freshly built core binaries"]
    async fn live_bound_start_reuses_running_dependencies_and_rolls_back_new_ones() {
        let temp = Temp::new();
        let bin = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let route = temp.0.join("rule.r");
        let rules = temp.0.join("rule.toml");
        fs::write(
            &rules,
            format!(
                "socket={:?}\ndefault_action='direct'\n",
                route.to_string_lossy()
            ),
        )
        .unwrap();
        let occupied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy = temp.0.join("proxy.toml");
        fs::write(
            &proxy,
            format!(
                "rule_socket={:?}\nsocks5_listen='{}'\n",
                route.to_string_lossy(),
                occupied.local_addr().unwrap()
            ),
        )
        .unwrap();
        let mut manager = Manager::load(temp.0.clone(), bin).unwrap();
        manager.registry.instances = vec![
            Instance {
                name: "rules".into(),
                kind: Kind::Rule,
                config: rules,
            },
            Instance {
                name: "proxy".into(),
                kind: Kind::Proxy,
                config: proxy,
            },
        ];
        manager.bind_start("proxy", vec!["rules".into()]).unwrap();
        assert!(manager.start("proxy").await.is_err());
        assert!(
            manager.running.is_empty(),
            "Newly started dependency must be rolled back"
        );
        manager.start_one("rules").await.unwrap();
        let pid = manager.running["rules"].child.id();
        assert!(manager.start("proxy").await.is_err());
        assert_eq!(
            manager.running["rules"].child.id(),
            pid,
            "Existing dependency must be kept"
        );
        drop(occupied);
        manager.start("proxy").await.unwrap();
        let proxy_pid = manager.running["proxy"].child.id();
        let rows = manager.action(Action::List).await.unwrap().rows;
        assert!(
            rows.iter().all(|row| !row.changed),
            "Canonical path spelling must not create a pending reload"
        );
        manager.start("proxy").await.unwrap();
        assert_eq!(manager.running["rules"].child.id(), pid);
        assert_eq!(manager.running["proxy"].child.id(), proxy_pid);
        manager.stop("proxy").await.unwrap();
        assert!(manager.running.contains_key("rules"));
        let proxy_two = temp.0.join("proxy-two.toml");
        let blocked = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        fs::write(
            &proxy_two,
            format!(
                "rule_socket={:?}\nsocks5_listen='{}'\n",
                route.to_string_lossy(),
                blocked.local_addr().unwrap()
            ),
        )
        .unwrap();
        manager.registry.instances.push(Instance {
            name: "proxy-two".into(),
            kind: Kind::Proxy,
            config: proxy_two,
        });
        manager
            .bind_start("rules", vec!["proxy".into(), "proxy-two".into()])
            .unwrap();
        assert!(manager.start("rules").await.is_err());
        assert_eq!(
            manager.running.len(),
            1,
            "A failed second proxy rolls back the first newly started proxy"
        );
        assert_eq!(manager.running["rules"].child.id(), pid);
        drop(blocked);
        manager.start("rules").await.unwrap();
        assert_eq!(manager.running["rules"].child.id(), pid);
        assert_eq!(manager.running.len(), 3);
        let pids: Vec<_> = ["rules", "proxy", "proxy-two"]
            .iter()
            .map(|name| manager.running[*name].child.id())
            .collect();
        manager.start("rules").await.unwrap();
        assert_eq!(
            pids,
            ["rules", "proxy", "proxy-two"]
                .iter()
                .map(|name| manager.running[*name].child.id())
                .collect::<Vec<_>>()
        );
        manager.stop_all().await;
        #[cfg(windows)]
        for name in ["rules", "proxy", "proxy-two"] {
            fs::write(manager.socket(name), b"").unwrap();
        }
        manager.start("rules").await.unwrap();
        assert_eq!(manager.running.len(), 3);
        manager.stop_all().await;
    }
    #[test]
    fn defaults_create_parents_are_private_and_preserve_existing_files() {
        let temp = Temp::new();
        for kind in [Kind::Proxy, Kind::Rule] {
            let path = temp.0.join(kind.binary()).join("config.toml");
            assert!(ensure_config(kind, &path).unwrap());
            let document: toml::Value = toml::from_str(&read_config(&path).unwrap()).unwrap();
            assert!(document
                .get(if kind == Kind::Proxy {
                    "rule_socket"
                } else {
                    "socket"
                })
                .is_some());
            #[cfg(unix)]
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
            fs::write(&path, "# user configuration").unwrap();
            assert!(!ensure_config(kind, &path).unwrap());
            assert_eq!(read_config(&path).unwrap(), "# user configuration");
        }
    }
    #[test]
    fn default_creation_errors_can_be_fixed_and_retried_without_overwrite() {
        let temp = Temp::new();
        let directory = temp.0.join("blocked");
        fs::write(&directory, "existing file").unwrap();
        let path = directory.join("rule.toml");
        assert!(ensure_config(Kind::Rule, &path).is_err());
        assert_eq!(fs::read_to_string(&directory).unwrap(), "existing file");
        fs::remove_file(&directory).unwrap();
        assert!(ensure_config(Kind::Rule, &path).unwrap());
        assert!(!ensure_config(Kind::Rule, &path).unwrap());
    }
    #[test]
    fn atomic_save_and_private_permissions() {
        let temp = Temp::new();
        private_dir(&temp.0).unwrap();
        let path = temp.0.join("config.toml");
        atomic_write(&path, b"old").unwrap();
        atomic_write(&path, b"new").unwrap();
        assert_eq!(fs::read(&path).unwrap(), b"new");
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        #[cfg(unix)]
        assert_eq!(
            fs::metadata(&temp.0).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }
    #[tokio::test]
    async fn manager_rejects_version_and_cleans_socket() {
        let temp = Temp::new();
        let home = temp.0.clone();
        let socket = home.join("manager.sock");
        let task = tokio::spawn(serve(home, PathBuf::from("/nonexistent")));
        for _ in 0..50 {
            if socket.exists() {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
        let mut stream = astra_ipc::platform::LocalStream::connect(&socket)
            .await
            .unwrap();
        astra_ipc::send_management(
            &mut stream,
            &Request {
                version: 999,
                action: Action::Shutdown,
            },
        )
        .await
        .unwrap();
        let reply: Reply = astra_ipc::receive_management(&mut stream).await.unwrap();
        assert!(reply.error.unwrap().contains("version"));
        assert!(call(&socket, Action::List).await.unwrap().rows.is_empty());
        call(&socket, Action::Shutdown).await.unwrap();
        task.await.unwrap().unwrap();
        assert!(!socket.exists());
    }
    #[tokio::test]
    #[ignore = "run after cargo build --workspace"]
    async fn live_bound_configs_streaming_and_runtime_log_level() {
        use tokio::io::AsyncWriteExt;
        let temp = Temp::new();
        let home = temp.0.join("state");
        let socket = home.join("manager.sock");
        let bin = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let task = tokio::spawn(serve(home.clone(), bin));
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
        for (name, kind) in [("rules", Kind::Rule), ("proxy", Kind::Proxy)] {
            call(
                &socket,
                Action::Create {
                    name: name.into(),
                    kind,
                },
            )
            .await
            .unwrap();
        }
        let canonical_home = fs::canonicalize(&home).unwrap();
        let rows = call(&socket, Action::List).await.unwrap().rows;
        assert_eq!(
            rows[0].instance.config,
            canonical_home.join("configs/rules.toml")
        );
        assert_eq!(
            rows[1].instance.config,
            canonical_home.join("configs/proxy.toml")
        );
        let rule_config: toml::Value =
            toml::from_str(&fs::read_to_string(&rows[0].instance.config).unwrap()).unwrap();
        let proxy_config: toml::Value =
            toml::from_str(&fs::read_to_string(&rows[1].instance.config).unwrap()).unwrap();
        assert_eq!(
            rule_config["socket"].as_str(),
            proxy_config["rule_socket"].as_str()
        );
        assert!(call(
            &socket,
            Action::Create {
                name: "rules".into(),
                kind: Kind::Rule
            }
        )
        .await
        .is_err());
        let mut stream = crate::log_stream::connect(&socket, "rules".into(), None)
            .await
            .unwrap();
        assert!(matches!(
            astra_ipc::receive_management::<LogEvent>(&mut stream)
                .await
                .unwrap(),
            LogEvent::Waiting
        ));
        call(&socket, Action::Start("rules".into())).await.unwrap();
        async fn until(stream: &mut astra_ipc::platform::LocalStream, needle: &str) -> String {
            timeout(Duration::from_secs(5), async {
                let mut output = String::new();
                loop {
                    if let LogEvent::Data { bytes, .. } =
                        astra_ipc::receive_management::<LogEvent>(stream)
                            .await
                            .unwrap()
                    {
                        output.push_str(&String::from_utf8_lossy(&bytes));
                        if output.contains(needle) {
                            return output;
                        }
                    }
                }
            })
            .await
            .unwrap()
        }
        until(&mut stream, "Rule Core ready").await;
        let pid = call(&socket, Action::List).await.unwrap().rows[0].pid;
        call(
            &socket,
            Action::SetLogLevel {
                name: "rules".into(),
                level: LogLevel::Route,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            call(&socket, Action::GetLogLevel("rules".into()))
                .await
                .unwrap()
                .text,
            "route"
        );
        assert_eq!(pid, call(&socket, Action::List).await.unwrap().rows[0].pid);
        call(
            &socket,
            Action::SetLogLevel {
                name: "rules".into(),
                level: LogLevel::Debug,
            },
        )
        .await
        .unwrap();
        assert_eq!(pid, call(&socket, Action::List).await.unwrap().rows[0].pid);
        let route = PathBuf::from(rule_config["socket"].as_str().unwrap());
        let mut bad = astra_ipc::platform::LocalStream::connect(&route)
            .await
            .unwrap();
        bad.write_all(&[0, 0, 0, 0]).await.unwrap();
        drop(bad);
        let text = until(&mut stream, "IPC peer disconnected").await;
        assert!(text.contains("DEBUG"));
        assert!(!text.contains('\x1b'));
        call(
            &socket,
            Action::SetLogLevel {
                name: "rules".into(),
                level: LogLevel::Error,
            },
        )
        .await
        .unwrap();
        let previous = fs::metadata(home.join("rules.log")).unwrap().len();
        let mut bad = astra_ipc::platform::LocalStream::connect(&route)
            .await
            .unwrap();
        bad.write_all(&[0, 0, 0, 0]).await.unwrap();
        drop(bad);
        sleep(Duration::from_millis(250)).await;
        assert_eq!(
            fs::metadata(home.join("rules.log")).unwrap().len(),
            previous
        );
        call(&socket, Action::Shutdown).await.unwrap();
        drop(stream);
        task.await.unwrap().unwrap();
        let manager = Manager::load(home.clone(), PathBuf::new()).unwrap();
        assert_eq!(manager.level("rules"), LogLevel::Error);
        assert_eq!(
            manager.instance("rules").unwrap().config,
            canonical_home.join("configs/rules.toml")
        );
    }
    /// Requires freshly built core binaries; exercises the actual management IPC.
    #[tokio::test]
    #[ignore = "run after cargo build --workspace"]
    async fn live_cores_and_config_lifecycle() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let temp = Temp::new();
        let home = temp.0.join("state");
        let bin = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let route_socket = temp.0.join("rule.sock");
        let rule_path = temp.0.join("rule.toml");
        let proxy_path = temp.0.join("proxy.toml");
        let rule_text = format!(
            "socket = {:?}\ndefault_action = \"direct\"\n",
            route_socket.to_str().unwrap()
        );
        fs::write(&rule_path, &rule_text).unwrap();
        let reserve = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy_addr = reserve.local_addr().unwrap();
        drop(reserve);
        fs::write(&proxy_path, format!("rule_socket = {:?}\nsocks5_listen = \"{proxy_addr}\"\ndecision_cache_ttl_secs = 0\n", route_socket.to_str().unwrap())).unwrap();
        let task = tokio::spawn(serve(home.clone(), bin));
        let socket = home.join("manager.sock");
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
        for (name, kind, config) in [
            ("rules", Kind::Rule, rule_path.clone()),
            ("proxy", Kind::Proxy, proxy_path.clone()),
        ] {
            call(
                &socket,
                Action::Add(Instance {
                    name: name.into(),
                    kind,
                    config,
                }),
            )
            .await
            .unwrap();
            call(&socket, Action::Start(name.into())).await.unwrap();
        }
        let rows = call(&socket, Action::List).await.unwrap().rows;
        assert!(rows.iter().all(|r| r.state == "Running" && r.pid.is_some()));
        let proxy_pid = rows
            .iter()
            .find(|row| row.instance.name == "proxy")
            .unwrap()
            .pid;
        call(&socket, Action::Start("proxy".into())).await.unwrap();
        assert_eq!(
            call(&socket, Action::List)
                .await
                .unwrap()
                .rows
                .iter()
                .find(|row| row.instance.name == "proxy")
                .unwrap()
                .pid,
            proxy_pid
        );
        assert!(call(&socket, Action::Remove("rules".into())).await.is_err());
        assert!(call(
            &socket,
            Action::Save {
                name: "rules".into(),
                original: rule_text.clone(),
                text: "invalid = [".into()
            }
        )
        .await
        .is_err());
        assert_eq!(fs::read_to_string(&rule_path).unwrap(), rule_text);
        let modified = format!("{rule_text}# edited\n");
        call(
            &socket,
            Action::Save {
                name: "rules".into(),
                original: rule_text.clone(),
                text: modified.clone(),
            },
        )
        .await
        .unwrap();
        assert_eq!(
            fs::read_to_string(rule_path.with_extension("toml.bak")).unwrap(),
            rule_text
        );
        assert!(!call(&socket, Action::List).await.unwrap().rows[0].changed);
        assert!(call(
            &socket,
            Action::Save {
                name: "rules".into(),
                original: rule_text,
                text: modified
            }
        )
        .await
        .is_err());
        call(&socket, Action::Restart("rules".into()))
            .await
            .unwrap();
        assert!(!call(&socket, Action::List).await.unwrap().rows[0].changed);
        // Real SOCKS5 CONNECT + echo after independently restarting Rule Core.
        let echo = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let echo_port = echo.local_addr().unwrap().port();
        let echo_task = tokio::spawn(async move {
            let (mut stream, _) = echo.accept().await.unwrap();
            let mut bytes = [0; 4];
            stream.read_exact(&mut bytes).await.unwrap();
            stream.write_all(&bytes).await.unwrap();
        });
        let mut stream = tokio::net::TcpStream::connect(proxy_addr).await.unwrap();
        stream.write_all(&[5, 1, 0]).await.unwrap();
        let mut greeting = [0; 2];
        stream.read_exact(&mut greeting).await.unwrap();
        assert_eq!(greeting, [5, 0]);
        let [hi, lo] = echo_port.to_be_bytes();
        stream
            .write_all(&[5, 1, 0, 1, 127, 0, 0, 1, hi, lo])
            .await
            .unwrap();
        let mut connected = [0; 10];
        stream.read_exact(&mut connected).await.unwrap();
        assert_eq!(connected[1], 0);
        stream.write_all(b"test").await.unwrap();
        let mut bytes = [0; 4];
        stream.read_exact(&mut bytes).await.unwrap();
        assert_eq!(&bytes, b"test");
        drop(stream);
        echo_task.await.unwrap();
        call(
            &socket,
            Action::Add(Instance {
                name: "conflict".into(),
                kind: Kind::Proxy,
                config: proxy_path,
            }),
        )
        .await
        .unwrap();
        assert!(call(&socket, Action::Start("conflict".into()))
            .await
            .is_err());
        let log = call(&socket, Action::Logs("conflict".into()))
            .await
            .unwrap()
            .text
            .to_lowercase();
        assert!(
            log.contains("bind") || log.contains("address already in use"),
            "{log}"
        );
        call(&socket, Action::Shutdown).await.unwrap();
        task.await.unwrap().unwrap();
        assert!(!route_socket.exists());
        assert!(!home.join("proxy.sock").exists());
        let manager = Manager::load(home, PathBuf::new()).unwrap();
        assert_eq!(manager.registry.instances.len(), 3);
        assert!(manager.running.is_empty());
    }
}
