use anyhow::{Context, Result};
use astra_management::{
    config::{AgentConfig, ManagerConfig, Mode},
    v2::*,
};
use std::{path::PathBuf, sync::Arc, time::Duration};

pub async fn run() -> Result<()> {
    if let Some(
        mode @ ("--maintenance-run"
        | "--maintenance-retry"
        | "--maintenance-previous"
        | "--maintenance-privileged"),
    ) = std::env::args().nth(1).as_deref()
    {
        let args: Vec<_> = std::env::args_os().skip(2).collect();
        anyhow::ensure!(
            args.len() == 2,
            "Maintenance requires STATE_DIRECTORY OPERATION_ID"
        );
        let home = PathBuf::from(&args[0]);
        let id = args[1]
            .to_str()
            .context("Invalid maintenance ID")?
            .to_owned();
        return if mode == "--maintenance-privileged" {
            crate::maintenance_privilege::run(home, id).await
        } else {
            crate::maintenance::run(home, id, mode.trim_start_matches("--maintenance-")).await
        };
    }
    if std::env::args_os().nth(1).as_deref() == Some(std::ffi::OsStr::new("--install-system")) {
        return crate::install::run_helper(std::env::args_os().skip(2).collect());
    }
    if let Some(command @ ("provision-agent" | "rotate-agent" | "revoke-agent")) =
        std::env::args().nth(1).as_deref()
    {
        return provision(command);
    }
    let mut args = std::env::args().skip(1);
    let mut shutdown = false;
    let mut status = false;
    let home = astra_platform::platform::home_dir()?;
    let mut config = ManagerConfig {
        mode: Mode::Agent,
        state_dir: home.join(".astra/tui"),
        config_dir: home.join(".astra/config"),
        bin_dir: std::env::current_exe()?
            .parent()
            .context("executable directory")?
            .to_path_buf(),
        controller: None,
        agent: AgentConfig {
            id: "local".into(),
            name: "Local host".into(),
            ..Default::default()
        },
        ..ManagerConfig::default()
    };
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => {
                let path = args.next().context("--config requires path")?;
                config = ManagerConfig::load(std::path::Path::new(&path))?;
            }
            "--state-dir" => {
                config.state_dir = args.next().context("--state-dir requires path")?.into()
            }
            "--config-dir" => {
                config.config_dir = args.next().context("--config-dir requires path")?.into()
            }
            "--bin-dir" => config.bin_dir = args.next().context("--bin-dir requires path")?.into(),
            "--agent-id" => config.agent.id = args.next().context("--agent-id requires value")?,
            "--manager" => {}
            "--shutdown" => shutdown = true,
            "--status" => status = true,
            "--lang" => {
                args.next().context("--lang requires value")?;
            }
            "--help" | "-h" => {
                println!("astra-manager --config manager.toml\nIndependent controller/agent service. Agents also accept private local management v2.\nastra-host is the local-agent compatibility executable.");
                return Ok(());
            }
            _ => anyhow::bail!("Unknown manager argument: {arg}"),
        }
    }
    if shutdown || status {
        let id = format!("cli-{}", uuid::Uuid::new_v4());
        let body = if shutdown {
            RequestBody::Execute {
                operation_id: id.clone(),
                command: Command::Shutdown,
            }
        } else {
            RequestBody::Query(Query::Agents)
        };
        let response = astra_management::ManagerClient::new(config.endpoint())
            .request_v2(Request::new(id, body))
            .await?;
        match response.result {
            Ok(value) => println!("{}", serde_json::to_string_pretty(&value)?),
            Err(error) => anyhow::bail!("{:?}: {}", error.code, error.message),
        };
        return Ok(());
    }
    run_config(config).await
}
fn provision(command: &str) -> Result<()> {
    use astra_platform::platform::OpenOptionsExt;
    use sha2::Digest;
    use std::io::Write;
    let mut args = std::env::args().skip(2);
    let mut config_path = None;
    let mut id = None;
    let mut output = None;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => {
                config_path = Some(PathBuf::from(
                    args.next().context("--config requires path")?,
                ))
            }
            "--agent-id" => id = Some(args.next().context("--agent-id requires value")?),
            "--token-out" => {
                output = Some(PathBuf::from(
                    args.next().context("--token-out requires path")?,
                ))
            }
            _ => anyhow::bail!(
                "provision-agent accepts --config FILE --agent-id ID --token-out NEW_PRIVATE_FILE"
            ),
        }
    }
    let config_path = config_path.context("--config required")?;
    let id = id.context("--agent-id required")?;
    anyhow::ensure!(
        !id.is_empty()
            && id.len() <= 80
            && id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'),
        "invalid agent identity"
    );
    let mut config: ManagerConfig = toml::from_str(&std::fs::read_to_string(&config_path)?)?;
    let controller = config
        .controller
        .as_mut()
        .context("controller configuration required")?;
    if command == "revoke-agent" {
        anyhow::ensure!(
            controller.agent_token_hashes.remove(&id).is_some(),
            "Agent identity is not registered"
        );
        crate::atomic_write(&config_path, toml::to_string_pretty(&config)?.as_bytes())?;
        println!("Revoked agent. The controller reloads credentials within two seconds; running cores remain active.");
        return Ok(());
    }
    anyhow::ensure!(
        command == "rotate-agent" || !controller.agent_token_hashes.contains_key(&id),
        "Agent already exists; use rotate-agent to replace its credential"
    );
    anyhow::ensure!(
        command != "rotate-agent" || controller.agent_token_hashes.contains_key(&id),
        "Agent identity is not registered"
    );
    let output = output.context("--token-out required")?;
    let token = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(&output)?;
    astra_platform::platform::set_private(&output, 0o600)?;
    file.write_all(token.as_bytes())?;
    file.sync_all()?;
    controller
        .agent_token_hashes
        .insert(id, format!("{:x}", sha2::Sha256::digest(token.as_bytes())));
    crate::atomic_write(&config_path, toml::to_string_pretty(&config)?.as_bytes())?;
    println!("Agent credential written once to {}. The controller reloads credentials within two seconds. Install the new credential privately on the agent.",output.display());
    Ok(())
}
pub async fn run_config(mut config: ManagerConfig) -> Result<()> {
    let _ = tokio_rustls::rustls::crypto::aws_lc_rs::default_provider().install_default();
    config.validate()?;
    crate::private_dir(&config.state_dir)?;
    crate::private_dir(&config.config_dir)?;
    config.state_dir = std::fs::canonicalize(&config.state_dir)?;
    config.config_dir = std::fs::canonicalize(&config.config_dir)?;
    config.bin_dir = std::fs::canonicalize(&config.bin_dir)?;
    anyhow::ensure!(
        !config.state_dir.starts_with(&config.config_dir)
            && !config.config_dir.starts_with(&config.state_dir),
        "Configuration and state directories must be separate, non-nested directories"
    );
    if let Some(runtime) = &config.runtime_dir {
        crate::private_dir(runtime)?;
    }
    if let Ok(id) = std::env::var("ASTRA_MAINTENANCE_OPERATION") {
        use astra_management::maintenance as m;
        let mut record =
            m::read(&config.state_dir)?.context("Missing maintenance launch record")?;
        let own = astra_platform::lifecycle::capture(std::process::id())?;
        anyhow::ensure!(
            record.operation_id == id
                && record.phase == m::Phase::Starting
                && record.target == config.bin_dir
                && record.new_process.as_ref().is_none_or(|p| p == &own),
            "Maintenance launch identity mismatch"
        );
        record.new_process = Some(own);
        m::write(&record)?;
    } else {
        anyhow::ensure!(
            !astra_management::maintenance::pending(&config.state_dir)?,
            "Unfinished maintenance: open the local interface and explicitly retry or restore the previous version"
        );
    }
    let endpoint = config.endpoint();
    if let Some(parent) = endpoint.parent() {
        crate::private_dir(parent)?;
    }
    let listener = astra_platform::platform::bind_listener(&endpoint)
        .context("Management endpoint exists; an existing service is never replaced")?;
    astra_platform::platform::set_private(&endpoint, 0o600)?;
    let _web = crate::web_process::supervise(&config)?;
    let ipc_connections = config.limits.ipc_connections;
    match config.mode {
        Mode::Controller => {
            let controller = crate::controller::Controller::open(
                config.controller.context("controller config")?,
                &config.state_dir,
                config.limits.event_buffer,
            )
            .await?;
            let credentials = watch_credentials(config.config_file, controller.clone());
            tokio::select! {result=controller.clone().listen()=>result,result=serve_local(listener,Service::Controller(controller.clone()),ipc_connections)=>result,error=controller.database_closed()=>Err(error),_=credentials=>Err(anyhow::anyhow!("credential watcher stopped unexpectedly")),result=astra_platform::platform::shutdown_signal()=>result.map_err(Into::into)}
        }
        Mode::Agent => {
            let agent = crate::agent::AgentService::new(config.clone())?;
            let mut native = agent.start_native().await?;
            let local = serve_local(listener, Service::Agent(agent.clone()), ipc_connections);
            let uplink = async {
                if config.agent.controller_url.is_some() {
                    crate::controller::uplink(agent.clone()).await
                } else {
                    std::future::pending::<Result<()>>().await
                }
            };
            let mut completed = false;
            let result = tokio::select! {
                result=&mut native=>{completed=true;result.context("native actor task")?},
                result=local=>result,
                result=uplink=>result,
                error=agent.database_closed()=>Err(error),
                result=agent.retain_network()=>result,
            };
            let diagnostic_cleanup = agent.stop_network().await;
            if !completed {
                native.abort();
                let _ = native.await;
            }
            let _diagnostic_pause = diagnostic_cleanup?;
            if result.is_ok() {
                agent.shutdown_servers().await?;
            }
            result
        }
    }
}
async fn watch_credentials(path: Option<PathBuf>, controller: Arc<crate::controller::Controller>) {
    let Some(path) = path else {
        std::future::pending::<()>().await;
        return;
    };
    let mut previous = Vec::new();
    loop {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let Ok(bytes) = tokio::fs::read(&path).await else {
            continue;
        };
        if bytes == previous {
            continue;
        }
        previous = bytes.clone();
        let parsed = std::str::from_utf8(&bytes)
            .ok()
            .and_then(|text| toml::from_str::<ManagerConfig>(text).ok());
        match parsed {
            Some(config) if config.mode==Mode::Controller && config.validate().is_ok()=>{
                if let Some(config)=config.controller {controller.replace_credentials(config.agent_token_hashes);}
            },
            _=>eprintln!("Controller credential reload rejected: invalid configuration; existing credentials retained"),
        }
    }
}
#[derive(Clone)]
enum Service {
    Agent(Arc<crate::agent::AgentService>),
    Controller(Arc<crate::controller::Controller>),
}
async fn serve_local(
    listener: astra_platform::platform::LocalListener,
    service: Service,
    max_connections: usize,
) -> Result<()> {
    let capacity = Arc::new(tokio::sync::Semaphore::new(max_connections));
    loop {
        let (mut stream, _) = match listener.accept().await {
            Ok(value) => value,
            Err(error) if astra_platform::supervision::transient_io(&error) => {
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        let service = service.clone();
        let Ok(permit) = capacity.clone().try_acquire_owned() else {
            continue;
        };
        tokio::spawn(async move {
            let _permit = permit;
            let request = match tokio::time::timeout(
                Duration::from_secs(5),
                astra_management::client::receive::<_, Request>(&mut stream),
            )
            .await
            {
                Ok(Ok(request)) => request,
                _ => return,
            };
            let response = match service {
                Service::Agent(agent) => agent.handle(request).await,
                Service::Controller(controller) => controller.handle(request).await,
            };
            let _ = tokio::time::timeout(
                Duration::from_secs(5),
                astra_management::client::send(&mut stream, &response),
            )
            .await;
        });
    }
}
#[cfg(test)]
#[path = "test/service_crash_test.rs"]
mod crash_tests;
#[cfg(test)]
#[path = "test/service_test.rs"]
mod tests;
