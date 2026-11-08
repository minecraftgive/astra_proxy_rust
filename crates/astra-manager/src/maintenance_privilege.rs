//! One operation-scoped privileged peer, authenticated by OS process identity.
use anyhow::{ensure, Context, Result};
#[cfg(unix)]
use astra_management::v2::*;
use astra_management::{
    client::{receive, send},
    maintenance::{self as m, Record},
};
use astra_platform::platform::{LocalListener, LocalStream};
use serde::{Deserialize, Serialize};
use std::{
    path::PathBuf,
    process::{Command, Stdio},
    time::Duration,
};

#[derive(Serialize, Deserialize)]
enum Request {
    UpdateService,
    WaitStopped,
    #[cfg(unix)]
    Tun(astra_platform::unix_handoff::Offer),
    #[cfg(windows)]
    StartTun,
    Finish,
}
#[derive(Serialize, Deserialize)]
struct Ready {
    operation_id: String,
    process: astra_platform::lifecycle::ProcessIdentity,
}
pub struct Privilege {
    stream: Option<LocalStream>,
}
impl Privilege {
    async fn call(&mut self, request: Request) -> Result<()> {
        let stream = self
            .stream
            .as_mut()
            .context("Administrator authorization is unavailable")?;
        send(stream, &request).await?;
        let result: Result<(), String> =
            tokio::time::timeout(Duration::from_secs(180), receive(stream))
                .await
                .context("Privilege helper timed out")??;
        result.map_err(anyhow::Error::msg)
    }
    pub async fn update_service(&mut self, record: &Record) -> Result<()> {
        if record.service_update {
            self.call(Request::UpdateService).await?;
        }
        Ok(())
    }
    pub async fn wait_stopped(&mut self, record: &Record) -> Result<()> {
        if self.stream.is_some() {
            self.call(Request::WaitStopped).await?;
        } else {
            for instance in &record.instances {
                crate::maintenance::wait_gone(&instance.process, 30).await?;
            }
        }
        Ok(())
    }
    pub async fn before_restore(&mut self, _record: &Record) -> Result<()> {
        #[cfg(windows)]
        if self.stream.is_some() && !_record.service_update {
            self.call(Request::StartTun).await?;
        }
        Ok(())
    }
    pub async fn restore_tun(&mut self, _record: &Record) -> Result<()> {
        #[cfg(unix)]
        for instance in _record
            .instances
            .iter()
            .filter(|i| i.runtime.as_ref().is_some_and(|r| r.tun == Some(true)))
        {
            let command = |ticket| {
                RequestBody::Local(LocalCommand::MaintenanceTun {
                    operation_id: _record.operation_id.clone(),
                    instance: instance.name.clone(),
                    ticket,
                })
            };
            let reply = astra_management::ManagerClient::new(_record.config.endpoint())
                .request_v2(astra_management::v2::Request::new(
                    uuid::Uuid::new_v4().to_string(),
                    command(None),
                ))
                .await?;
            let ResponseBody::Local(value) =
                reply.result.map_err(|e| anyhow::anyhow!(e.message))?
            else {
                anyhow::bail!("Invalid TUN maintenance response")
            };
            let offer: astra_platform::unix_handoff::Offer = serde_json::from_value(value)?;
            let ticket = offer.ticket.clone();
            self.call(Request::Tun(offer)).await?;
            astra_management::ManagerClient::new(_record.config.endpoint())
                .request_v2(astra_management::v2::Request::new(
                    uuid::Uuid::new_v4().to_string(),
                    command(Some(ticket)),
                ))
                .await?
                .result
                .map_err(|e| anyhow::anyhow!(e.message))?;
        }
        if self.stream.is_some() {
            self.call(Request::Finish).await?;
        }
        Ok(())
    }
}
fn endpoint(record: &Record) -> PathBuf {
    m::directory(&record.config.state_dir).join("privilege.sock")
}

pub async fn prepare(record: &mut Record) -> Result<Privilege> {
    #[cfg(windows)]
    {
        record.service_update |= record.target.join("astra-service.exe").is_file()
            && astra_platform::windows_service::installed()?;
    }
    let tun = record
        .instances
        .iter()
        .any(|i| i.runtime.as_ref().is_some_and(|r| r.tun == Some(true)));
    if !tun && !record.service_update {
        return Ok(Privilege { stream: None });
    }
    m::write(record)?;
    let path = endpoint(record);
    if path.exists() {
        astra_platform::platform::recover_stale_endpoint(&path)?;
    }
    let listener = LocalListener::bind(&path)?;
    let program = m::coordinator(record)?;
    let arguments = [
        std::ffi::OsString::from("--maintenance-privileged"),
        record.config.state_dir.as_os_str().to_owned(),
        record.operation_id.clone().into(),
    ];
    #[cfg(windows)]
    let mut child = astra_platform::elevation::LaunchMethod::Uac.spawn(&program, &arguments)?;
    #[cfg(unix)]
    let mut child = {
        let mut command = authorization_command(&program, &arguments, record)?;
        if terminal_authorization() {
            command
                .stdin(Stdio::inherit())
                .stdout(Stdio::inherit())
                .stderr(Stdio::inherit());
        } else {
            command
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
        }
        command
            .spawn()
            .context("Request maintenance administrator authorization")?
    };
    let accepted = tokio::time::timeout(Duration::from_secs(180), async {
        loop {
            tokio::select! {
                result = listener.accept() => return result.map_err(anyhow::Error::from),
                _ = tokio::time::sleep(Duration::from_millis(100)) => {
                    ensure!(child.try_wait()?.is_none(), "Administrator authorization was denied or unavailable; old backend retained");
                }
            }
        }
    }).await;
    let (mut stream, _) = match accepted {
        Ok(Ok(peer)) => peer,
        failed => {
            #[cfg(unix)]
            {
                let _ = child.kill();
                let _ = child.wait();
            }
            return Err(match failed {
                Ok(Err(error)) => error,
                Err(error) => anyhow::anyhow!(error)
                    .context("Administrator authorization timed out; old backend retained"),
                _ => unreachable!(),
            });
        }
    };
    let ready: Ready = tokio::time::timeout(Duration::from_secs(5), receive(&mut stream)).await??;
    ensure!(
        ready.operation_id == record.operation_id
            && ready.process.pid == astra_platform::platform::peer_process_id(&stream)?,
        "Invalid privileged helper identity"
    );
    #[cfg(unix)]
    ensure!(
        stream.peer_cred()?.uid() == 0,
        "Maintenance helper is not privileged"
    );
    #[cfg(windows)]
    {
        ensure!(
            child.id() == Some(ready.process.pid),
            "Unexpected UAC helper process"
        );
        child.detach();
    }
    #[cfg(unix)]
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    #[cfg(unix)]
    if terminal_authorization() {
        // Authorization is complete. The coordinator now survives closure of
        // the initiating terminal without running the manager as root.
        ensure!(
            unsafe { libc::setsid() } >= 0,
            "Could not detach authorized maintenance coordinator"
        );
    }
    Ok(Privilege {
        stream: Some(stream),
    })
}

#[cfg(unix)]
fn terminal_authorization() -> bool {
    std::env::var_os("ASTRA_MAINTENANCE_TTY").is_some()
}

#[cfg(unix)]
fn authorization_command(
    program: &std::path::Path,
    arguments: &[std::ffi::OsString],
    record: &Record,
) -> Result<Command> {
    if unsafe { libc::geteuid() } == 0 {
        let mut c = Command::new(program);
        c.args(arguments);
        return Ok(c);
    }
    if terminal_authorization() {
        let mut command = Command::new("/usr/bin/sudo");
        command.arg(program).args(arguments);
        return Ok(command);
    }
    if let Some(frontend) = record.frontends.iter().find(|frontend| {
        frontend
            .executable
            .file_stem()
            .is_some_and(|name| name == "astra-gui" || name == "astra-server-gui")
    }) {
        let gui = record.target.join(
            frontend
                .executable
                .file_name()
                .context("Unknown GUI executable")?,
        );
        let mut command = Command::new("/usr/bin/sudo");
        command
            .arg("-A")
            .arg(program)
            .args(arguments)
            .env("SUDO_ASKPASS", gui)
            .env("ASTRA_GUI_ASKPASS", "1");
        return Ok(command);
    }
    #[cfg(target_os = "macos")]
    {
        let quote = |s: &std::ffi::OsStr| -> Result<String> {
            Ok(format!(
                "'{}'",
                s.to_str()
                    .context("Authorization path is not UTF-8")?
                    .replace('\'', "'\\''")
            ))
        };
        let mut shell = quote(program.as_os_str())?;
        for arg in arguments {
            shell.push(' ');
            shell.push_str(&quote(arg)?);
        }
        let script = format!(
            "do shell script {} with administrator privileges",
            serde_json::to_string(&shell)?
        );
        let mut command = Command::new("/usr/bin/osascript");
        command.args(["-e", &script]);
        Ok(command)
    }
    #[cfg(target_os = "linux")]
    {
        // pkexec uses the desktop authorization agent. In a terminal session,
        // sudo uses an independent terminal so it never reads a raw TUI screen.
        if std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some() {
            if std::path::Path::new("/usr/bin/pkexec").is_file() {
                let mut c = Command::new("/usr/bin/pkexec");
                c.arg(program).args(arguments);
                return Ok(c);
            }
            for terminal in ["/usr/bin/x-terminal-emulator", "/usr/bin/xterm"] {
                if std::path::Path::new(terminal).is_file() {
                    let mut c = Command::new(terminal);
                    c.args(["-e", "/usr/bin/sudo"]).arg(program).args(arguments);
                    return Ok(c);
                }
            }
        }
        let mut c = Command::new("/usr/bin/sudo");
        if !terminal_authorization() {
            c.arg("-n");
        }
        c.arg(program).args(arguments);
        Ok(c)
    }
}

pub async fn run(home: PathBuf, id: String) -> Result<()> {
    #[cfg(unix)]
    ensure!(
        unsafe { libc::geteuid() } == 0,
        "Administrator permission required"
    );
    #[cfg(windows)]
    ensure!(
        astra_platform::elevation::is_elevated()?,
        "Administrator permission required"
    );
    let home = std::fs::canonicalize(home)?;
    let record = m::read(&home)?.context("Missing maintenance record")?;
    ensure!(
        record.operation_id == id && !record.phase.terminal(),
        "Maintenance authorization expired"
    );
    ensure!(
        std::fs::canonicalize(std::env::current_exe()?)?.parent()
            == Some(record.coordinator.as_path()),
        "Authorization helper does not belong to the selected package"
    );
    m::coordinator(&record)?;
    crate::agent::maintenance::verify_target(&record.target, &record.manifest_sha256)?;
    #[cfg(windows)]
    if record
        .instances
        .iter()
        .any(|i| i.runtime.as_ref().is_some_and(|r| r.tun == Some(true)))
    {
        ensure!(
            record.target.join("wintun.dll").is_file(),
            "Target TUN driver is missing"
        );
    }
    #[cfg(windows)]
    if record.service_update {
        astra_platform::windows_service::maintenance_preflight(
            &record.target,
            &record
                .instances
                .iter()
                .map(|i| i.process.clone())
                .collect::<Vec<_>>(),
        )?;
    }
    let mut stream = astra_platform::platform::connect_trusted(endpoint(&record)).await?;
    let helper = record
        .helper
        .as_ref()
        .context("Missing maintenance owner")?;
    ensure!(
        astra_platform::platform::peer_process_id(&stream)? == helper.pid
            && astra_platform::lifecycle::matches_live(helper)?,
        "Maintenance owner identity mismatch"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(
            std::fs::metadata(&home)?.uid() == stream.peer_cred()?.uid(),
            "Maintenance owner does not own the state directory"
        );
    }
    send(
        &mut stream,
        &Ready {
            operation_id: id,
            process: astra_platform::lifecycle::capture(std::process::id())?,
        },
    )
    .await?;
    loop {
        let request: Request =
            match tokio::time::timeout(Duration::from_secs(600), receive(&mut stream)).await {
                Ok(Ok(request)) => request,
                _ => return Ok(()),
            };
        if matches!(request, Request::Finish) {
            send(&mut stream, &Result::<(), String>::Ok(())).await?;
            return Ok(());
        }
        let result = execute(&record, request)
            .await
            .map_err(|e| format!("{e:#}"));
        let failed = result.is_err();
        send(&mut stream, &result).await?;
        if failed {
            return Ok(());
        }
    }
}
async fn execute(record: &Record, request: Request) -> Result<()> {
    crate::agent::maintenance::verify_target(&record.target, &record.manifest_sha256)?;
    match request {
        Request::WaitStopped => {
            for instance in &record.instances {
                crate::maintenance::wait_gone(&instance.process, 30).await?;
            }
        }
        Request::UpdateService => {
            #[cfg(windows)]
            astra_platform::windows_service::maintenance_update(
                &record.target,
                record.service_update,
            )?;
            #[cfg(not(windows))]
            anyhow::bail!("Windows service update is unavailable on this platform");
        }
        #[cfg(unix)]
        Request::Tun(offer) => {
            ensure!(
                offer.version == astra_platform::unix_handoff::PROTOCOL_VERSION
                    && std::fs::canonicalize(&offer.binary)? == record.target.join("astra-proxy")
                    && offer.socket.parent()
                        == Some(record.config.state_dir.join("elevation").as_path())
                    && offer.ticket.len() <= 256,
                "Invalid maintenance elevation handoff"
            );
            let mut child = Command::new(&offer.binary)
                .arg("--unix-elevated")
                .arg(&offer.socket)
                .arg(&offer.ticket)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()?;
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
        #[cfg(windows)]
        Request::StartTun => {
            start_windows_tun(record).await?;
        }
        Request::Finish => unreachable!(),
    }
    Ok(())
}
#[cfg(windows)]
async fn start_windows_tun(record: &Record) -> Result<()> {
    let mut current = m::read(&record.config.state_dir)?.context("Missing maintenance state")?;
    let owner = current
        .new_process
        .clone()
        .context("New backend is not ready")?;
    ensure!(
        astra_platform::lifecycle::matches_live(&owner)?,
        "New backend exited"
    );
    for instance in record
        .instances
        .iter()
        .filter(|i| i.runtime.as_ref().is_some_and(|r| r.tun == Some(true)))
    {
        let runtime = record
            .config
            .runtime_dir
            .clone()
            .unwrap_or(astra_platform::storage::runtime_root()?);
        let mut args = astra_platform::elevation::core_arguments_in(
            &instance.config,
            Some(
                &record
                    .config
                    .state_dir
                    .join(format!("{}.sock", instance.name)),
            ),
            &record
                .config
                .state_dir
                .join(format!("{}.log", instance.name)),
            "info",
            None,
            &runtime,
        )?;
        let driver = record.target.join("wintun.dll");
        ensure!(driver.is_file(), "Target TUN driver is missing");
        args[5] = driver.into_os_string();
        args.push(serde_json::to_string(&owner)?.into());
        let mut child = Command::new(record.target.join("astra-proxy.exe"))
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
        current.candidates.insert(
            instance.name.clone(),
            astra_platform::lifecycle::capture(child.id())?,
        );
        m::write(&current)?;
        std::thread::spawn(move || {
            let _ = child.wait();
        });
    }
    Ok(())
}
