use crate::{i18n, protocol, ui, windows};
use anyhow::{Context, Result};
use astra_platform::platform::OpenOptionsExt;
use std::{
    fs::OpenOptions,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};

// Shared cadence for passive UI data, core subscriptions and log tailing.
pub(crate) const UI_REFRESH_INTERVAL: Duration = Duration::from_millis(500);

fn is_host_binary(executable: &Path) -> bool {
    executable
        .file_stem()
        .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case("astra-host"))
}

fn manager_executable(executable: &Path) -> PathBuf {
    let host = executable.with_file_name(if cfg!(windows) {
        "astra-host.exe"
    } else {
        "astra-host"
    });
    if host.is_file() {
        host
    } else {
        executable.to_owned()
    }
}

fn selected_startup_release(home: &Path, explicit_bin: bool) -> Result<Option<(PathBuf, PathBuf)>> {
    if explicit_bin {
        return Ok(None);
    }
    let Some(release) = astra_update::ReleaseStore::new(home).resolve_current()? else {
        return Ok(None);
    };
    let host = release
        .directory
        .join(astra_platform::platform::executable("astra-host"));
    anyhow::ensure!(release.manifest.components.contains_key("astra-host") && host.is_file(),
        "The selected release has no versioned astra-host; choose a complete client package or use an explicit --bin-dir for maintenance");
    Ok(Some((release.directory, host)))
}

fn redirect_host(
    executable: &Path,
    manager_binary: &Path,
    explicit_bin: bool,
    socket: &Path,
) -> Result<bool> {
    if explicit_bin || std::fs::canonicalize(manager_binary)? == std::fs::canonicalize(executable)?
    {
        return Ok(false);
    }
    anyhow::ensure!(
        !socket.exists(),
        "A manager endpoint already exists; a running host is never replaced during startup"
    );
    Ok(true)
}

fn selected_frontend(
    executable: &Path,
    bin: &Path,
    selected: bool,
    explicit_bin: bool,
) -> Result<Option<PathBuf>> {
    if !selected || explicit_bin {
        return Ok(None);
    }
    let frontend = bin.join(astra_platform::platform::executable("astra-tui"));
    if std::fs::canonicalize(&frontend)? == std::fs::canonicalize(executable)? {
        Ok(None)
    } else {
        Ok(Some(frontend))
    }
}

fn set_operation(operation: &mut Option<protocol::Action>, next: protocol::Action) -> Result<()> {
    anyhow::ensure!(operation.is_none(), "Specify only one update command");
    *operation = Some(next);
    Ok(())
}

pub(crate) async fn run(default_kind: Option<protocol::Kind>) -> Result<()> {
    i18n::init()?;
    let executable = std::env::current_exe()?;
    let mut home = astra_platform::platform::home_dir()?.join(".astra/tui");
    let mut config_home = astra_platform::platform::home_dir()?.join(".astra/config");
    let mut bin = executable
        .parent()
        .context("Missing executable directory")?
        .to_path_buf();
    let mut explicit_bin = false;
    let mut daemon = is_host_binary(&executable);
    let mut shutdown = false;
    let mut endpoint = None;
    let mut agent_id = None;
    let mut operation = None;
    let mut launch = windows::Launch {
        kind: default_kind,
        ..Default::default()
    };
    if default_kind == Some(protocol::Kind::Server) {
        launch.view = Some(windows::View::Profiles);
    }
    let mut native_console = false;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--endpoint" => {
                endpoint = Some(PathBuf::from(
                    args.next()
                        .context("--endpoint requires a trusted local IPC path")?,
                ))
            }
            "--agent" => {
                agent_id = Some(
                    args.next()
                        .context("--agent requires a registered agent ID")?,
                )
            }
            "--lang" => {
                args.next().context("--lang requires en, zh-CN or zh-TW")?;
            }
            "--state-dir" => {
                home = PathBuf::from(args.next().context("--state-dir requires a path")?)
            }
            "--config-dir" => {
                config_home = PathBuf::from(args.next().context("--config-dir requires a path")?)
            }
            "--bin-dir" => {
                bin = PathBuf::from(args.next().context("--bin-dir requires a path")?);
                explicit_bin = true;
            }
            "--view" => {
                launch.view = Some(windows::View::parse(
                    &args.next().context("--view requires a page")?,
                )?)
            }
            "--instance" => {
                launch.instance = Some(args.next().context("--instance requires a name")?)
            }
            "--core" => {
                launch.kind = Some(
                    match args
                        .next()
                        .context("--core requires proxy or rule")?
                        .as_str()
                    {
                        "proxy" => protocol::Kind::Proxy,
                        "rule" => protocol::Kind::Rule,
                        "server" => protocol::Kind::Server,
                        _ => anyhow::bail!("--core requires proxy, rule or server"),
                    },
                )
            }
            "--window-id" => {
                launch.window_id = Some(args.next().context("--window-id requires an ID")?)
            }
            "--native-console" => native_console = true,
            "--manager" => daemon = true,
            "--shutdown" => shutdown = true,
            "update" => {
                let package = args
                    .next()
                    .context("update requires a local package path")?;
                set_operation(
                    &mut operation,
                    protocol::Action::UpdatePackage(PathBuf::from(package)),
                )?;
            }
            "update-status" => set_operation(&mut operation, protocol::Action::UpdateStatus)?,
            "rollback" => set_operation(&mut operation, protocol::Action::Rollback)?,
            "--help" | "-h" => {
                let program = if is_host_binary(&executable) {
                    "astra-host"
                } else {
                    "astra-tui"
                };
                println!("{program} [--lang en|zh-CN|zh-TW] [--state-dir DIR] [--config-dir DIR] [--bin-dir DIR]\n{}", i18n::t("--manager: run manager · --shutdown: stop all\nq: close UI; instances keep running"));
                println!("update PATH: apply a local update package\nupdate-status: show release and drain status\nrollback: return new sessions to the previous release\nastra-host runs the background manager by default; host updates take effect after an intentional restart");
                println!("--endpoint PATH [--agent ID]: connect to a trusted local manager/controller IPC endpoint; remote nodes are selected by ID");
                println!("--view overview|proxies|connections|rules|logs|profiles|tui-log|diagnostics [--instance NAME] [--core proxy|rule] [--window-id ID]\nF8: current page in a new window · Alt+1–6: page in a new window");
                return Ok(());
            }
            _ => anyhow::bail!("Unknown argument: {arg}"),
        }
    }
    anyhow::ensure!(
        !shutdown || operation.is_none(),
        "--shutdown cannot be combined with an update command"
    );
    if let Some(protocol::Action::UpdatePackage(path)) = &mut operation {
        *path = std::fs::canonicalize(&*path)
            .with_context(|| format!("Cannot open update package {}", path.display()))?;
    }
    #[cfg(windows)]
    if native_console {
        windows::attach_console_io()?;
        crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::SetTitle(format!(
                "Astra · {}",
                launch.view.unwrap_or(windows::View::Dashboard).name()
            ))
        )?;
    }
    #[cfg(not(windows))]
    let _ = native_console;
    if let (Some(view), None) = (launch.view, &launch.window_id) {
        launch.window_id = windows::Launch::detached(view, None, protocol::Kind::Proxy).window_id;
    }
    astra_platform::storage::private_dir(&home)?;
    home = std::fs::canonicalize(home)?;
    anyhow::ensure!(
        agent_id.is_none() || endpoint.is_some(),
        "--agent requires --endpoint"
    );
    if let Some(endpoint) = endpoint {
        anyhow::ensure!(!daemon, "--endpoint cannot start a manager");
        let endpoint = std::fs::canonicalize(endpoint).context(
            "Manager/controller endpoint does not exist; start its independent service first",
        )?;
        protocol::configure(endpoint.clone(), agent_id);
        let hello = protocol::request(
            &endpoint,
            astra_management::v2::RequestBody::Query(astra_management::v2::Query::Hello),
        )
        .await?;
        if let astra_management::v2::ResponseBody::Hello(hello) = hello {
            anyhow::ensure!(
                hello.role == "agent",
                "Select a registered node with --agent ID when connecting to a controller"
            );
        }
        if shutdown {
            println!(
                "{}",
                protocol::call(&endpoint, protocol::Action::Shutdown)
                    .await?
                    .text
            );
            return Ok(());
        }
        if let Some(action) = operation {
            println!("{}", protocol::call(&endpoint, action).await?.text);
            return Ok(());
        }
        let context = windows::Context {
            executable,
            home,
            config: config_home,
            bin,
            language: i18n::language(),
        };
        return ui::run(endpoint, context, launch).await;
    }
    astra_platform::storage::private_dir(&config_home)?;
    config_home = std::fs::canonicalize(config_home)?;
    anyhow::ensure!(
        config_home != home && !config_home.starts_with(&home) && !home.starts_with(&config_home),
        "Configuration and state directories must be separate, non-nested directories"
    );
    let selected = if shutdown {
        None
    } else {
        let home = home.clone();
        tokio::task::spawn_blocking(move || selected_startup_release(&home, explicit_bin)).await??
    };
    let has_selected_release = selected.is_some();
    let manager_binary = if let Some((directory, host)) = selected {
        bin = directory;
        host
    } else {
        manager_executable(&executable)
    };
    bin = std::fs::canonicalize(bin)?;
    let socket = home.join("management.sock");
    astra_platform::platform::validate_state_directory(&home)?;
    if !daemon && !shutdown && operation.is_none() {
        if let Some(frontend) =
            selected_frontend(&executable, &bin, has_selected_release, explicit_bin)?
        {
            // This is a new user launch, never an in-place UI restart. Unlike a
            // host handoff, a frontend can reconnect to an existing manager.
            let status = tokio::process::Command::new(frontend)
                .args(std::env::args_os().skip(1))
                .arg("--bin-dir")
                .arg(&bin)
                .status()
                .await
                .context("Start the selected interface")?;
            anyhow::ensure!(
                status.success(),
                "The selected interface exited with {status}"
            );
            return Ok(());
        }
    }
    if daemon && !shutdown && operation.is_none() {
        if redirect_host(&executable, &manager_binary, explicit_bin, &socket)? {
            let status = tokio::process::Command::new(&manager_binary)
                .arg("--manager")
                .arg("--state-dir")
                .arg(&home)
                .arg("--config-dir")
                .arg(&config_home)
                .arg("--bin-dir")
                .arg(&bin)
                .arg("--lang")
                .arg(match i18n::language() {
                    i18n::Language::English => "en",
                    i18n::Language::Simplified => "zh-CN",
                    i18n::Language::Traditional => "zh-TW",
                })
                .status()
                .await
                .context("Start the selected resident host")?;
            anyhow::ensure!(
                status.success(),
                "The selected resident host exited with {status}"
            );
            return Ok(());
        }
        anyhow::ensure!(
            manager_binary != executable,
            "The separate astra-host executable is required; install a complete package"
        );
        let status = tokio::process::Command::new(&manager_binary)
            .arg("--manager")
            .arg("--state-dir")
            .arg(&home)
            .arg("--config-dir")
            .arg(&config_home)
            .arg("--bin-dir")
            .arg(&bin)
            .status()
            .await
            .context("Start the independent manager")?;
        anyhow::ensure!(status.success(), "Manager exited with {status}");
        return Ok(());
    }
    if shutdown {
        let reply = protocol::call(&socket, protocol::Action::Shutdown).await?;
        println!("{}", i18n::t(&reply.text));
        return Ok(());
    }
    if protocol::call(&socket, protocol::Action::List)
        .await
        .is_err()
    {
        if socket.exists() {
            astra_platform::platform::recover_stale_manager_endpoint(&socket)
                .with_context(|| format!("Manager endpoint is unavailable and could not be safely recovered: {}. Inspect {} and running cores", socket.display(), home.join("manager.log").display()))?;
        }
        anyhow::ensure!(!socket.exists(), "Manager socket is present but unavailable. Inspect manager.log and running cores before removing a stale socket.");
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(home.join("manager.log"))?;
        let mut command = Command::new(&manager_binary);
        command
            .arg("--manager")
            .arg("--state-dir")
            .arg(&home)
            .arg("--config-dir")
            .arg(&config_home)
            .arg("--bin-dir")
            .arg(&bin)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        astra_platform::platform::background_command(&mut command, true);
        let mut child = command.spawn()?;
        let mut ready = false;
        // Poll quickly at first: the manager normally binds within a few ms.
        for attempt in 0..120 {
            if protocol::call(&socket, protocol::Action::List)
                .await
                .is_ok()
            {
                ready = true;
                break;
            }
            if let Some(status) = child.try_wait()? {
                anyhow::bail!(
                    "Manager exited ({status}); see {}",
                    home.join("manager.log").display()
                );
            }
            tokio::time::sleep(Duration::from_millis(if attempt < 20 { 10 } else { 50 })).await;
        }
        anyhow::ensure!(ready, "Manager did not become ready; inspect manager.log");
    }
    let storage = protocol::call(&socket, protocol::Action::StoragePaths).await
        .context("The running manager does not support separate configuration storage. Exit it and restart the updated astra-tui")?;
    let storage: protocol::StoragePaths = serde_json::from_str(&storage.text)?;
    anyhow::ensure!(storage.config == config_home && storage.state == home,
        "The manager for this state directory is using configuration directory {}. Use that --config-dir or a separate --state-dir", storage.config.display());
    if let Some(action) = operation {
        let reply = protocol::call(&socket, action).await?;
        println!("{}", reply.text);
        return Ok(());
    }
    let context = windows::Context {
        executable,
        home,
        config: config_home,
        bin,
        language: i18n::language(),
    };
    ui::run(socket, context, launch).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use astra_update::{
        ImportOptions, ProtocolRange, ReleaseFile, ReleaseManifest, ReleaseStore,
        HOST_PROTOCOL_VERSION, TARGET,
    };
    use sha2::{Digest, Sha256};

    struct Directory {
        path: PathBuf,
        parent: PathBuf,
    }
    impl Directory {
        fn new() -> Self {
            static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let parent = std::fs::canonicalize(std::env::temp_dir()).unwrap();
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let sequence = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let path = parent.join(format!(
                "astra-entry-{}-{nonce}-{sequence}",
                std::process::id()
            ));
            std::fs::create_dir(&path).unwrap();
            Self { path, parent }
        }
        fn install(&self, host: bool) -> (ReleaseStore, PathBuf) {
            let source = self.path.join("source");
            std::fs::create_dir(&source).unwrap();
            let mut manifest = ReleaseManifest {
                schema_version: 1,
                role: Default::default(),
                release_id: "selected".into(),
                target: TARGET.into(),
                host_protocol: ProtocolRange {
                    min: HOST_PROTOCOL_VERSION,
                    max: HOST_PROTOCOL_VERSION,
                },
                components: Default::default(),
                files: Vec::new(),
                profile: None,
                mode: None,
                cargo_profile: None,
            };
            let mut components = vec!["astra-proxy", "astra-rule", "astra-tui"];
            if host {
                components.push("astra-host");
            }
            for component in components {
                let name = astra_platform::platform::executable(component);
                std::fs::write(source.join(&name), b"fixture").unwrap();
                manifest.components.insert(component.into(), "0.1.0".into());
                manifest.files.push(ReleaseFile {
                    path: name,
                    size: 7,
                    sha256: format!("{:x}", Sha256::digest(b"fixture")),
                });
            }
            std::fs::write(
                source.join("manifest.json"),
                serde_json::to_vec(&manifest).unwrap(),
            )
            .unwrap();
            let home = self.path.join("state");
            let store = ReleaseStore::new(&home);
            store.import(source, &ImportOptions::default()).unwrap();
            store.commit_current("selected").unwrap();
            (store, home)
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            if let Ok(target) = std::fs::canonicalize(&self.path) {
                assert_eq!(target.parent(), Some(self.parent.as_path()));
                let _ = std::fs::remove_dir_all(target);
            }
        }
    }

    #[test]
    fn startup_uses_verified_current_host_and_explicit_bin_bypasses_selection() {
        let fixture = Directory::new();
        let (store, home) = fixture.install(true);
        let (directory, host) = selected_startup_release(&home, false).unwrap().unwrap();
        assert_eq!(directory, store.release_dir("selected").unwrap());
        assert_eq!(
            host,
            directory.join(astra_platform::platform::executable("astra-host"))
        );
        std::fs::write(store.root().join("current.json"), b"invalid").unwrap();
        assert!(selected_startup_release(&home, true).unwrap().is_none());
        assert!(selected_startup_release(&home, false).is_err());
    }

    #[test]
    fn startup_does_not_fall_back_when_selected_release_has_no_host() {
        let fixture = Directory::new();
        let (store, home) = fixture.install(true);
        let manifest_path = store.release_dir("selected").unwrap().join("manifest.json");
        let mut manifest: ReleaseManifest =
            serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
        manifest.components.remove("astra-host");
        std::fs::write(manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
        assert!(selected_startup_release(&home, false)
            .unwrap_err()
            .to_string()
            .contains("astra-host"));
        store.clear_current().unwrap();
        assert!(selected_startup_release(&home, false).unwrap().is_none());
    }

    #[test]
    fn daemon_redirect_requires_no_existing_endpoint_and_no_explicit_bin() {
        let fixture = Directory::new();
        let executable = fixture.path.join("old-host");
        let selected = fixture.path.join("new-host");
        let socket = fixture.path.join("manager.sock");
        std::fs::write(&executable, b"old").unwrap();
        std::fs::write(&selected, b"new").unwrap();
        assert!(redirect_host(&executable, &selected, false, &socket).unwrap());
        assert!(!redirect_host(&executable, &selected, true, &socket).unwrap());
        assert!(!redirect_host(&selected, &selected, false, &socket).unwrap());
        std::fs::write(&socket, b"existing endpoint").unwrap();
        assert!(redirect_host(&executable, &selected, false, &socket).is_err());
    }

    #[test]
    fn frontend_redirect_preserves_explicit_bin_and_allows_an_existing_manager() {
        let fixture = Directory::new();
        let (store, home) = fixture.install(true);
        let bin = store.release_dir("selected").unwrap();
        let original = fixture.path.join("old-tui");
        std::fs::write(&original, b"old").unwrap();
        std::fs::write(home.join("manager.sock"), b"live manager marker").unwrap();
        let selected = bin.join(astra_platform::platform::executable("astra-tui"));
        assert_eq!(
            selected_frontend(&original, &bin, true, false).unwrap(),
            Some(selected.clone())
        );
        assert!(selected_frontend(&original, &bin, true, true)
            .unwrap()
            .is_none());
        assert!(selected_frontend(&original, &bin, false, false)
            .unwrap()
            .is_none());
        assert!(selected_frontend(&selected, &bin, true, false)
            .unwrap()
            .is_none());
    }
}
