use super::*;
use crate::package_choice::Package;
use anyhow::{ensure, Context};
use astra_update::installation as engine;

pub(super) fn scope(scope: v::InstallationScope) -> engine::InstallScope {
    match scope {
        v::InstallationScope::User => engine::InstallScope::User,
        v::InstallationScope::System => engine::InstallScope::System,
    }
}
fn destination(package: &Package, scope: v::InstallationScope) -> v::InstallationDestination {
    match engine::status(&package.directory, self::scope(scope)) {
        Ok(s) => v::InstallationDestination {
            directory: Some(s.directory.unwrap_or(s.root)),
            entry: Some(s.entry),
            installed: s.installed,
            requires_elevation: s.requires_local_action,
            unavailable_reason: None,
        },
        Err(error) => v::InstallationDestination {
            directory: None,
            entry: None,
            installed: false,
            requires_elevation: false,
            unavailable_reason: Some(format!("{error:#}")),
        },
    }
}
pub(super) fn candidate(package: &Package) -> Result<v::InstallationPackage> {
    package.verify()?;
    let manifest =
        astra_update::verify_installation_directory(&package.directory, &Default::default())?;
    Ok(v::InstallationPackage {
        id: package.id(),
        release_id: package.release_id.clone(),
        role: engine::role_name(manifest.role).into(),
        source: package.directory.clone(),
        user: destination(package, v::InstallationScope::User),
        system: destination(package, v::InstallationScope::System),
    })
}
pub(super) fn progress(update: engine::InstallProgress) -> astra_ipc::progress::Progress {
    use engine::InstallStep as E;
    use v::InstallationStep as V;
    let payload = v::InstallationProgress {
        step: match update.step {
            E::Preparing => V::Preparing,
            E::Copying => V::Copying,
            E::Verifying => V::Verifying,
            E::Publishing => V::Publishing,
            E::Complete => V::Complete,
        },
        step_index: update.step.index(),
        step_count: E::COUNT,
        current: update.current.to_string(),
        total: update.total.to_string(),
    };
    astra_ipc::progress::Progress::new(
        "installation_event",
        serde_json::to_string(&payload).unwrap(),
    )
}
pub(super) fn install(
    package: &Package,
    request: &Request,
    context: &crate::windows::Context,
    progress: watch::Sender<Option<astra_ipc::progress::Progress>>,
) -> Result<v::InstallationResult> {
    ensure!(
        !astra_management::maintenance::pending(&context.home)?,
        "Finish or recover maintenance before installing"
    );
    package.verify()?;
    ensure!(
        package.id() == request.package_id && package.release_id == request.expected_release_id,
        "Installation target changed"
    );
    let before = engine::status(&package.directory, scope(request.scope))?;
    let outcome = engine::install(
        &package.directory,
        &package.release_id,
        &package.manifest_sha256,
        scope(request.scope),
        |update| {
            progress.send_replace(Some(self::progress(update)));
        },
    )?;
    let engine::InstallOutcome::Installed(installed) = outcome else {
        anyhow::bail!("Administrator permission is required for this location.");
    };
    ensure!(
        installed.manifest_sha256 == package.manifest_sha256,
        "Installed package identity changed"
    );
    let directory = installed.directory.context("Missing installed package")?;
    let executable = directory.join(context.executable.file_name().context("Unknown frontend")?);
    let mut target = context.clone();
    target.executable = executable.clone();
    target.bin = directory.clone();
    // This is a display hint only; process launches always use argument arrays.
    let mut args = vec![executable.into_os_string()];
    args.extend(target.arguments(&Default::default()));
    let command = args
        .iter()
        .map(|s| {
            #[cfg(windows)]
            return format!("'{}'", s.to_string_lossy().replace('\'', "''"));
            #[cfg(not(windows))]
            return crate::shell::shell_quote(&s.to_string_lossy());
        })
        .collect::<Vec<_>>()
        .join(" ");
    #[cfg(windows)]
    let launch_command = format!("& {command}");
    #[cfg(not(windows))]
    let launch_command = command;
    Ok(v::InstallationResult {
        role: engine::role_name(installed.role).into(),
        directory,
        entry: installed.entry,
        already_installed: before.installed,
        message: if before.installed {
            "This package is already installed."
        } else {
            "Package installed; waiting to switch the running version."
        }
        .into(),
        launch_command,
    })
}

/// Local frontend packages do not require a new API on the resident old host.
pub async fn prepare_elevation(
    socket: &std::path::Path,
    request: &Request,
) -> Result<v::InstallationLaunch> {
    if let Origin::FrontendPackage(package) = &request.origin {
        ensure!(
            crate::protocol::target().is_none(),
            "Local package installation is unavailable for remote targets"
        );
        ensure!(
            request.scope == v::InstallationScope::System,
            "Only system installation needs elevation"
        );
        let package = package.clone();
        let id = request.package_id.clone();
        let release = request.expected_release_id.clone();
        return tokio::task::spawn_blocking(move || {
            package.verify()?;
            ensure!(
                package.id() == id && package.release_id == release,
                "Installation target changed"
            );
            let program = package
                .directory
                .join(astra_platform::platform::executable("astra-host"));
            ensure!(
                program.is_file(),
                "Complete package lacks installation helper"
            );
            Ok(v::InstallationLaunch {
                program,
                arguments: vec![
                    "--install-system".into(),
                    package
                        .directory
                        .to_str()
                        .context("Installation path is not UTF-8")?
                        .into(),
                    package.release_id,
                    package.manifest_sha256,
                ],
            })
        })
        .await?;
    }
    match crate::protocol::request(
        socket,
        v::RequestBody::Local(v::LocalCommand::PrepareInstallation {
            package_id: request.package_id.clone(),
            expected_release_id: request.expected_release_id.clone(),
            scope: request.scope,
        }),
    )
    .await?
    {
        v::ResponseBody::InstallationLaunch(launch) => Ok(launch),
        _ => anyhow::bail!("Unexpected installation response"),
    }
}
