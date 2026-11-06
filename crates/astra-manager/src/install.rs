//! Installation always runs on the selected agent, using discovered packages.
//! Frontends never supply filesystem roots or choose a privileged destination.
use anyhow::{Context, Result};
use astra_management::{config::ManagerConfig, v2::*};
use astra_platform::installation::InstallScope;
use astra_update::{installation as engine, ImportOptions, ReleaseManifest};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    path::{Path, PathBuf},
};

const ROLES: [&str; 4] = ["client", "server", "controller", "web"];

struct Source {
    path: PathBuf,
    manifest: ReleaseManifest,
    digest: String,
    id: String,
}

fn role_name(manifest: &ReleaseManifest) -> String {
    serde_json::to_value(manifest.role)
        .unwrap()
        .as_str()
        .unwrap()
        .to_owned()
}

fn candidates(config: &ManagerConfig) -> Vec<PathBuf> {
    let mut paths = vec![config.bin_dir.clone()];
    if let Ok(executable) = std::env::current_exe() {
        if let Some(parent) = executable.parent() {
            paths.push(parent.to_owned());
        }
    }
    if let Some(binary) = &config.web.binary {
        if let Some(parent) = binary.parent() {
            paths.push(parent.to_owned());
        }
    }
    paths
}

fn inspect(path: &Path) -> Result<Source> {
    let path = std::fs::canonicalize(path)?;
    let manifest = astra_update::verify_installation_directory(&path, &ImportOptions::default())?;
    let bytes = astra_platform::storage::read_bounded(
        &path.join(astra_update::MANIFEST_FILE),
        astra_update::MANIFEST_LIMIT,
    )?;
    // Bind the UI selection to both its location and the exact manifest. A
    // package replaced without changing release_id still invalidates the ID.
    anyhow::ensure!(
        serde_json::from_slice::<ReleaseManifest>(&bytes)? == manifest,
        "Conflict: package manifest changed while inspecting; refresh installation status"
    );
    let digest = format!("{:x}", Sha256::digest(&bytes));
    let mut hash = Sha256::new();
    hash.update(path.as_os_str().as_encoded_bytes());
    hash.update([0]);
    hash.update(digest.as_bytes());
    Ok(Source {
        path,
        manifest,
        digest,
        id: format!("{:x}", hash.finalize()),
    })
}

fn discover(config: &ManagerConfig) -> (Vec<Source>, Vec<String>) {
    let mut seen = std::collections::HashSet::new();
    let mut sources = Vec::new();
    let mut warnings = Vec::new();
    for path in candidates(config) {
        let canonical = std::fs::canonicalize(&path).unwrap_or(path);
        if !seen.insert(canonical.clone()) {
            continue;
        }
        match inspect(&canonical) {
            Ok(source) => sources.push(source),
            Err(error) => warnings.push(format!(
                "{}: not an installable complete package ({error:#})",
                canonical.display()
            )),
        }
    }
    sources.sort_by_key(|source| {
        (
            ROLES
                .iter()
                .position(|role| *role == role_name(&source.manifest))
                .unwrap_or(ROLES.len()),
            source.path.clone(),
        )
    });
    (sources, warnings)
}

fn scope(value: InstallationScope) -> InstallScope {
    match value {
        InstallationScope::User => InstallScope::User,
        InstallationScope::System => InstallScope::System,
    }
}

fn destination(source: &Path, value: InstallScope) -> InstallationDestination {
    match engine::status(source, value) {
        Ok(status) => InstallationDestination {
            directory: Some(status.directory.unwrap_or(status.root)),
            entry: Some(status.entry),
            installed: status.installed,
            requires_elevation: status.requires_local_action,
            unavailable_reason: None,
        },
        Err(error) => InstallationDestination {
            directory: None,
            entry: None,
            installed: false,
            requires_elevation: false,
            unavailable_reason: Some(format!("{error:#}")),
        },
    }
}

pub(crate) fn status(config: &ManagerConfig, target: String) -> Result<InstallationStatus> {
    let (sources, mut warnings) = discover(config);
    let mut unavailable_roles: BTreeMap<String, String> = ROLES
        .into_iter()
        .map(|role| {
            (
                role.into(),
                "No complete package was found on this target".into(),
            )
        })
        .collect();
    let packages = sources
        .into_iter()
        .map(|source| {
            let role = role_name(&source.manifest);
            unavailable_roles.remove(&role);
            InstallationPackage {
                id: source.id,
                role,
                release_id: source.manifest.release_id,
                user: destination(&source.path, InstallScope::User),
                system: destination(&source.path, InstallScope::System),
                source: source.path,
            }
        })
        .collect();
    let bin = astra_platform::platform::home_dir()
        .ok()
        .map(|home| home.join(".local/bin"));
    #[cfg(unix)]
    if let Some(bin) = bin {
        if !std::env::var_os("PATH")
            .is_some_and(|path| std::env::split_paths(&path).any(|entry| entry == bin))
        {
            warnings.push(format!(
                "To use installed user commands in this shell: export PATH={}:\"$PATH\"",
                quote(&bin.to_string_lossy())
            ));
        }
    }
    #[cfg(windows)]
    let _ = bin;
    if config.state_dir.join("updates/current.json").exists() {
        warnings.push("The existing client update selection is retained and may take precedence on the next TUI launch.".into());
    }
    Ok(InstallationStatus {
        target,
        account: astra_platform::installation::account_name(),
        packages,
        unavailable_roles,
        warnings,
    })
}

fn selected(config: &ManagerConfig, id: &str, release: &str) -> Result<Source> {
    let (sources, _) = discover(config);
    sources
        .into_iter()
        .find(|source| source.id == id && source.manifest.release_id == release)
        .context(
            "Conflict: the selected package is missing or changed; refresh installation status",
        )
}

pub(crate) fn execute(
    config: &ManagerConfig,
    id: &str,
    release: &str,
    value: InstallationScope,
    progress: impl FnMut(&str),
) -> Result<InstallationResult> {
    let source = selected(config, id, release)?;
    let before = engine::status(&source.path, scope(value))?;
    let outcome = engine::install(
        &source.path,
        release,
        &source.digest,
        scope(value),
        progress,
    )?;
    let installed = match outcome {
        engine::InstallOutcome::Installed(status) => status,
        engine::InstallOutcome::RequiresLocalAction(_) => anyhow::bail!("RequiresLocalAction: system installation requires administrator approval on the target machine; open its local TUI and choose Settings > Installation"),
    };
    let directory = installed
        .directory
        .context("Installation did not publish a directory")?;
    let role = role_name(&source.manifest);
    let executable = match role.as_str() {
        "client" => "astra-tui",
        "server" => "astra-server-tui",
        "controller" => "astra-manager",
        "web" => "astra-web",
        _ => unreachable!("validated package role"),
    };
    Ok(InstallationResult {
        launch_command: launch_command(config, &directory.join(astra_platform::platform::executable(executable)), &role),
        role, directory, entry: installed.entry, already_installed: before.installed,
        message: "Installation is ready for the next launch. Running processes, configuration, startup registration and the TUN service are unchanged.".into(),
    })
}

pub(crate) fn prepare(
    config: &ManagerConfig,
    id: &str,
    release: &str,
    value: InstallationScope,
) -> Result<InstallationLaunch> {
    anyhow::ensure!(
        value == InstallationScope::System,
        "Only system installation needs a local elevation helper"
    );
    let source = selected(config, id, release)?;
    Ok(InstallationLaunch {
        program: std::env::current_exe()?,
        arguments: vec![
            "--install-system".into(),
            source
                .path
                .to_str()
                .context("Installation source path is not UTF-8")?
                .into(),
            source.manifest.release_id,
            source.digest,
        ],
    })
}

/// An explicit, short-lived privileged process. It accepts no destination,
/// service name, shell command, or arbitrary operation from a request.
pub(crate) fn run_helper(arguments: Vec<OsString>) -> Result<()> {
    anyhow::ensure!(
        arguments.len() == 3,
        "System installation requires SOURCE RELEASE_ID MANIFEST_SHA256"
    );
    let source = PathBuf::from(&arguments[0]);
    anyhow::ensure!(
        source.is_absolute(),
        "System installation source must be absolute"
    );
    let release = arguments[1].to_str().context("Invalid release ID")?;
    let digest = arguments[2].to_str().context("Invalid manifest digest")?;
    match engine::install(&source, release, digest, InstallScope::System, |message| {
        use std::io::Write;
        let _ = writeln!(std::io::stderr().lock(), "{message}");
    })? {
        engine::InstallOutcome::Installed(_) => Ok(()),
        engine::InstallOutcome::RequiresLocalAction(_) => {
            anyhow::bail!("System installation requires administrator permissions")
        }
    }
}

fn quote(value: &str) -> String {
    #[cfg(windows)]
    return format!("'{}'", value.replace('\'', "''"));
    #[cfg(not(windows))]
    return format!("'{}'", value.replace('\'', "'\\''"));
}

fn launch_command(config: &ManagerConfig, entry: &Path, role: &str) -> String {
    // A direct command preserves custom state/configuration even when the
    // platform's public entry uses the program's normal defaults.
    let mut command = quote(&entry.to_string_lossy());
    #[cfg(windows)]
    {
        command.insert_str(0, "& ");
    }
    match role {
        "client" | "server" => command.push_str(&format!(
            " --state-dir {} --config-dir {}",
            quote(&config.state_dir.to_string_lossy()),
            quote(&config.config_dir.to_string_lossy())
        )),
        "controller" => {
            if let Some(file) = &config.config_file {
                command.push_str(&format!(" --config {}", quote(&file.to_string_lossy())));
            }
        }
        "web" => {
            let file = config.state_dir.join("web-gateway.toml");
            if file.is_file() {
                command.push_str(&format!(" --config {}", quote(&file.to_string_lossy())));
            }
        }
        _ => {}
    }
    if let Some(runtime) = &config.runtime_dir {
        #[cfg(windows)]
        {
            command = format!(
                "$env:ASTRA_RUNTIME_DIR = {}; {command}",
                quote(&runtime.to_string_lossy())
            );
        }
        #[cfg(unix)]
        {
            command = format!(
                "ASTRA_RUNTIME_DIR={} {command}",
                quote(&runtime.to_string_lossy())
            );
        }
    }
    command
}

#[cfg(test)]
#[path = "test/install_test.rs"]
mod tests;
