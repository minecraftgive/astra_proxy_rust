//! Install the current complete local package without replacing running files.
use crate::{
    package, verify_installation_directory, ImportOptions, ReleaseManifest, ReleaseRole,
    MANIFEST_FILE, MANIFEST_LIMIT,
};
use anyhow::{ensure, Context, Result};
pub use astra_platform::installation::InstallScope;
use astra_platform::installation::{self as platform, InstallPaths};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallationStatus {
    pub scope: InstallScope,
    pub role: ReleaseRole,
    pub release_id: String,
    pub manifest_sha256: String,
    pub source: PathBuf,
    pub root: PathBuf,
    pub entry: PathBuf,
    pub directory: Option<PathBuf>,
    pub installed: bool,
    pub requires_local_action: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", content = "installation", rename_all = "snake_case")]
pub enum InstallOutcome {
    Installed(InstallationStatus),
    RequiresLocalAction(InstallationStatus),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u32,
    role: ReleaseRole,
    release_id: String,
    manifest_sha256: String,
    deployment: String,
}
struct Source {
    directory: PathBuf,
    manifest: ReleaseManifest,
    bytes: Vec<u8>,
    hash: String,
}
struct Staging(PathBuf);
impl Drop for Staging {
    fn drop(&mut self) {
        if package::plain_metadata(&self.0).is_ok_and(|metadata| metadata.is_dir()) {
            let expected_parent = self.0.parent().and_then(|path| fs::canonicalize(path).ok());
            if fs::canonicalize(&self.0)
                .ok()
                .is_some_and(|path| path.parent() == expected_parent.as_deref())
            {
                let _ = fs::remove_dir_all(&self.0);
            }
        }
    }
}
pub fn role_name(role: ReleaseRole) -> &'static str {
    match role {
        ReleaseRole::Client => "client",
        ReleaseRole::Server => "server",
        ReleaseRole::Controller => "controller",
        ReleaseRole::Web => "web",
    }
}
fn entry_binary(manifest: &ReleaseManifest) -> &'static str {
    match manifest.role {
        ReleaseRole::Client if manifest.components.contains_key("astra-gui") => "astra-gui",
        ReleaseRole::Server if manifest.components.contains_key("astra-server-gui") => {
            "astra-server-gui"
        }
        ReleaseRole::Client => "astra-tui",
        ReleaseRole::Server => "astra-server-tui",
        ReleaseRole::Controller => "astra-manager",
        ReleaseRole::Web => "astra-web",
    }
}
fn executable(directory: &Path, manifest: &ReleaseManifest) -> PathBuf {
    directory.join(astra_platform::platform::executable(entry_binary(manifest)))
}
fn source(source: &Path) -> Result<Source> {
    ensure!(
        package::plain_metadata(source)?.is_dir(),
        "Installation source is not a directory"
    );
    let directory = fs::canonicalize(source)?;
    let bytes = read_manifest(&directory)?;
    let manifest = verify_installation_directory(&directory, &ImportOptions::default())?;
    ensure!(
        bytes == read_manifest(&directory)?,
        "Installation manifest changed during verification"
    );
    let hash = format!("{:x}", Sha256::digest(&bytes));
    Ok(Source {
        directory,
        manifest,
        bytes,
        hash,
    })
}
fn read_manifest(directory: &Path) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    platform::open_package_file(&directory.join(MANIFEST_FILE))?
        .take(MANIFEST_LIMIT + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MANIFEST_LIMIT,
        "Installation manifest is too large"
    );
    Ok(bytes)
}

fn read_record(paths: &InstallPaths) -> Result<Option<Record>> {
    let bytes = match platform::read_plain_file(
        &paths.root.join("current.json"),
        paths.scope,
        MANIFEST_LIMIT,
    ) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    // A damaged record can be repaired from a complete package; no paths from
    // an invalid record are used, deleted, or followed.
    let Ok(record) = serde_json::from_slice::<Record>(&bytes) else {
        return Ok(None);
    };
    if record.version != 1
        || role_name(record.role) != paths.role
        || record.deployment.is_empty()
        || record.deployment.len() > 180
        || !record
            .deployment
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
        || matches!(record.deployment.as_str(), "." | "..")
    {
        return Ok(None);
    }
    Ok(Some(record))
}

fn verified_deployment(
    paths: &InstallPaths,
    record: &Record,
    source: &Source,
) -> Result<Option<PathBuf>> {
    if record.release_id != source.manifest.release_id || record.manifest_sha256 != source.hash {
        return Ok(None);
    }
    let directory = paths.root.join("deployments").join(&record.deployment);
    platform::validate_destination(&directory, paths.scope)?;
    let verified = (|| -> Result<()> {
        let manifest = verify_installation_directory(&directory, &ImportOptions::default())?;
        ensure!(manifest == source.manifest, "Installed manifest differs");
        for component in manifest.components.keys() {
            ensure!(
                platform::executable_ready(
                    &directory.join(astra_platform::platform::executable(component)),
                    paths.scope
                )?,
                "Installed executable permissions differ"
            );
        }
        let bytes =
            platform::read_plain_file(&directory.join(MANIFEST_FILE), paths.scope, MANIFEST_LIMIT)?;
        ensure!(
            format!("{:x}", Sha256::digest(bytes)) == source.hash,
            "Installed manifest checksum differs"
        );
        Ok(())
    })();
    Ok(verified.is_ok().then_some(directory))
}

fn inspect(source: &Source, paths: &InstallPaths) -> Result<InstallationStatus> {
    platform::validate_destination(&paths.root, paths.scope)?;
    let directory = match read_record(paths)? {
        Some(record) => verified_deployment(paths, &record, source)?,
        None => None,
    };
    let installed = match &directory {
        Some(directory) => {
            platform::entry_matches(paths, &executable(directory, &source.manifest))?
        }
        None => {
            platform::check_entry(paths)?;
            false
        }
    };
    Ok(InstallationStatus {
        scope: paths.scope,
        role: source.manifest.role,
        release_id: source.manifest.release_id.clone(),
        manifest_sha256: source.hash.clone(),
        source: source.directory.clone(),
        root: paths.root.clone(),
        entry: paths.entry.clone(),
        directory,
        installed,
        requires_local_action: !installed && !platform::has_permission(paths.scope)?,
    })
}

/// Read-only; querying installation never creates directories or launch entries.
pub fn status(source_directory: &Path, scope: InstallScope) -> Result<InstallationStatus> {
    let source = source(source_directory)?;
    let paths = platform::layout_for_frontend(
        scope,
        role_name(source.manifest.role),
        matches!(
            entry_binary(&source.manifest),
            "astra-gui" | "astra-server-gui"
        ),
    )?;
    inspect(&source, &paths)
}

pub fn install(
    source_directory: &Path,
    expected_release_id: &str,
    expected_manifest_sha256: &str,
    scope: InstallScope,
    on_progress: impl FnMut(&str),
) -> Result<InstallOutcome> {
    let source = source(source_directory)?;
    let paths = platform::layout_for_frontend(
        scope,
        role_name(source.manifest.role),
        matches!(
            entry_binary(&source.manifest),
            "astra-gui" | "astra-server-gui"
        ),
    )?;
    install_at(
        source,
        expected_release_id,
        expected_manifest_sha256,
        paths,
        on_progress,
    )
}

fn install_at(
    source: Source,
    expected_release_id: &str,
    expected_manifest_sha256: &str,
    paths: InstallPaths,
    mut on_progress: impl FnMut(&str),
) -> Result<InstallOutcome> {
    ensure!(
        source.manifest.release_id == expected_release_id
            && source.hash == expected_manifest_sha256,
        "The installation source changed; refresh its details before installing"
    );
    let current = inspect(&source, &paths)?;
    if current.installed {
        return Ok(InstallOutcome::Installed(current));
    }
    if current.requires_local_action {
        return Ok(InstallOutcome::RequiresLocalAction(current));
    }
    platform::check_entry(&paths)?;
    on_progress("Preparing installation");
    platform::prepare_directory(&paths.root, paths.scope)?;
    let lock_path = paths.root.join("install.lock");
    platform::validate_destination(&lock_path, paths.scope)?;
    if lock_path.try_exists()? {
        let _ = platform::read_plain_file(&lock_path, paths.scope, 1024)?;
    }
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(if paths.scope == InstallScope::System {
                0o644
            } else {
                0o600
            })
            .custom_flags(libc::O_NOFOLLOW);
    }
    let lock = options.open(&lock_path)?;
    platform::protect_file(&lock_path, paths.scope, false)?;
    lock.try_lock()
        .context("Another installation is in progress")?;
    let current = inspect(&source, &paths)?;
    if current.installed {
        return Ok(InstallOutcome::Installed(current));
    }
    platform::check_entry(&paths)?;
    let old_entry = if paths.entry.try_exists()? {
        Some(platform::read_plain_file(
            &paths.entry,
            paths.scope,
            1024 * 1024,
        )?)
    } else {
        None
    };

    let directory = if let Some(directory) = current.directory {
        directory
    } else {
        let deployments = paths.root.join("deployments");
        let staging_root = paths.root.join("staging");
        platform::prepare_directory(&deployments, paths.scope)?;
        platform::prepare_directory(&staging_root, paths.scope)?;
        let id = format!(
            "{}-{}",
            source.manifest.release_id,
            uuid::Uuid::new_v4().simple()
        );
        let staging = staging_root.join(&id);
        platform::prepare_staging_directory(&staging, paths.scope)?;
        let temporary = Staging(staging.clone());
        on_progress("Copying the current program package");
        for file in &source.manifest.files {
            let destination = staging.join(&file.path);
            let parent = destination.parent().context("Package file has no parent")?;
            platform::prepare_staging_directory(parent, paths.scope)?;
            let source_path = source.directory.join(&file.path);
            ensure!(
                package::plain_metadata(&source_path)?.is_file(),
                "Package file was replaced"
            );
            let mut ancestor = source_path.parent();
            while let Some(path) = ancestor.filter(|path| *path != source.directory) {
                ensure!(
                    path.starts_with(&source.directory) && package::plain_metadata(path)?.is_dir(),
                    "Package parent was replaced"
                );
                ancestor = path.parent();
            }
            let mut input = platform::open_package_file(&source_path)?.take(file.size + 1);
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&destination)?;
            let bytes = std::io::copy(&mut input, &mut output)?;
            ensure!(
                bytes == file.size,
                "Package file size changed: {}",
                file.path
            );
            let binary = source
                .manifest
                .components
                .keys()
                .any(|name| file.path == astra_platform::platform::executable(name));
            platform::protect_staging_file(&destination, paths.scope, binary)?;
            output.flush()?;
            output.sync_all()?;
        }
        platform::atomic_staging_file(&staging.join(MANIFEST_FILE), &source.bytes, paths.scope)?;
        on_progress("Verifying the installed program package");
        ensure!(
            verify_installation_directory(&staging, &ImportOptions::default())? == source.manifest,
            "Copied package verification failed"
        );
        let mut directories = std::collections::BTreeSet::new();
        for file in &source.manifest.files {
            let path = staging.join(&file.path);
            let binary = source
                .manifest
                .components
                .keys()
                .any(|name| file.path == astra_platform::platform::executable(name));
            platform::protect_file(&path, paths.scope, binary)?;
            for parent in path
                .ancestors()
                .skip(1)
                .take_while(|parent| *parent != staging)
            {
                directories.insert(parent.to_owned());
            }
        }
        platform::protect_file(&staging.join(MANIFEST_FILE), paths.scope, false)?;
        for directory in directories.iter().rev() {
            platform::finish_staging_directory(directory, paths.scope)?;
        }
        platform::finish_staging_directory(&staging, paths.scope)?;
        let destination = deployments.join(&id);
        platform::validate_destination(&destination, paths.scope)?;
        fs::rename(&staging, &destination)?;
        // The guard now points to the absent staging name, never the published release.
        drop(temporary);
        destination
    };
    let deployment = directory
        .file_name()
        .and_then(|name| name.to_str())
        .context("Invalid deployment directory")?
        .to_owned();
    let record = Record {
        version: 1,
        role: source.manifest.role,
        release_id: source.manifest.release_id.clone(),
        manifest_sha256: source.hash.clone(),
        deployment,
    };
    platform::check_entry(&paths)?;
    on_progress("Publishing the installation entry");
    platform::publish_entry(&paths, &executable(&directory, &source.manifest))?;
    let record_bytes = serde_json::to_vec_pretty(&record)?;
    if let Err(error) = platform::atomic_file(
        &paths.root.join("current.json"),
        &record_bytes,
        paths.scope,
        false,
    ) {
        // Restore the previous entry if committing the small current record fails.
        // Published immutable payload can remain for a subsequent repair.
        let restored = match old_entry {
            Some(bytes) => platform::atomic_file(&paths.entry, &bytes, paths.scope, cfg!(unix)),
            None => fs::remove_file(&paths.entry),
        };
        restored
            .context("Installation record failed and the previous entry could not be restored")?;
        return Err(error.into());
    }
    on_progress("Installation complete; the running processes are unchanged");
    Ok(InstallOutcome::Installed(inspect(&source, &paths)?))
}

#[cfg(test)]
#[path = "test/installation_test.rs"]
mod tests;
