use crate::protocol::*;
use anyhow::{bail, Context, Result};
use astra_ipc::control::{self, Command};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
    fs::{self, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};
use tokio::{
    net::UnixListener,
    process::{Child, Command as Process},
    time::{sleep, timeout},
};

#[derive(Default, Serialize, Deserialize)]
struct Registry {
    #[serde(default)]
    instances: Vec<Instance>,
    #[serde(default)]
    log_levels: HashMap<String, LogLevel>,
}
struct Running {
    child: Child,
    config: String,
    config_path: PathBuf,
}
pub struct Manager {
    home: PathBuf,
    bin: PathBuf,
    registry: Registry,
    running: HashMap<String, Running>,
    exits: HashMap<String, String>,
}
const CONFIG_LIMIT: usize = 480 * 1024;

pub fn private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path)?;
    anyhow::ensure!(
        !fs::symlink_metadata(path)?.file_type().is_symlink(),
        "State directory cannot be a symlink"
    );
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
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
        file.write_all(data)?;
        file.sync_all()?;
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
    use std::os::unix::fs::DirBuilderExt;
    let parent = path
        .parent()
        .context("Configuration needs a parent directory")?;
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
            Duration::from_secs(20),
            Process::new(self.bin.join(kind.binary()))
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
    async fn start(&mut self, name: &str) -> Result<()> {
        self.reap()?;
        anyhow::ensure!(
            !self.running.contains_key(name),
            "Instance is already running"
        );
        let instance = self.instance(name)?;
        self.validate(instance.kind, &instance.config).await?;
        let socket = self.socket(name);
        anyhow::ensure!(
            !socket.exists(),
            "Control socket exists: {}; check for an orphaned core before removing it",
            socket.display()
        );
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(self.log(name))?;
        let config = read_config(&instance.config)?;
        let child = Process::new(self.bin.join(instance.kind.binary()))
            .arg(&instance.config)
            .env("ASTRA_CONTROL_SOCKET", &socket)
            .env("ASTRA_LOG_LEVEL", self.level(name).as_str())
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .kill_on_drop(true)
            .spawn()?;
        self.running.insert(
            name.to_owned(),
            Running {
                child,
                config,
                config_path: instance.config.clone(),
            },
        );
        self.exits.remove(name);
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
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
                for i in &self.registry.instances {
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
                                            applied.path != i.config
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
                Ok(reply)
            }
            Action::Add(i) => self.add(i).await,
            Action::Create { name, kind } => self.create(name, kind).await,
            Action::FollowLogs { .. } => bail!("Log subscriptions require a dedicated connection"),
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
            Action::Remove(name) => {
                anyhow::ensure!(
                    !self.running.contains_key(&name),
                    "Stop the instance before removing it"
                );
                self.instance(&name)?;
                let old = self.registry.instances.clone();
                self.registry.instances.retain(|i| i.name != name);
                if let Err(e) = self.persist() {
                    self.registry.instances = old;
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

struct SocketGuard(PathBuf);
impl Drop for SocketGuard {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

pub async fn serve(home: PathBuf, bin: PathBuf) -> Result<()> {
    private_dir(&home)?;
    let socket = home.join("manager.sock");
    let listener = UnixListener::bind(&socket).context("Manager socket exists or path is too long; never remove a socket belonging to a running manager")?;
    let _guard = SocketGuard(socket.clone());
    fs::set_permissions(&socket, fs::Permissions::from_mode(0o600))?;
    let mut manager = Manager::load(home, bin)?;
    let mut followers = tokio::task::JoinSet::new();
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
            }
        }
        Ok(())
    }.await;
    manager.stop_all().await;
    followers.abort_all();
    while followers.join_next().await.is_some() {}
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
            let path = PathBuf::from("/tmp").join(format!(
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
        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o600
        );
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
        let mut stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
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
        async fn until(stream: &mut tokio::net::UnixStream, needle: &str) -> String {
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
                level: LogLevel::Debug,
            },
        )
        .await
        .unwrap();
        assert_eq!(pid, call(&socket, Action::List).await.unwrap().rows[0].pid);
        let route = PathBuf::from(rule_config["socket"].as_str().unwrap());
        let mut bad = tokio::net::UnixStream::connect(&route).await.unwrap();
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
        let mut bad = tokio::net::UnixStream::connect(&route).await.unwrap();
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
        assert!(call(&socket, Action::Start("proxy".into())).await.is_err());
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
