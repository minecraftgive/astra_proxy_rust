use crate::{package, validate_id, ImportOptions, ReleaseManifest};
use anyhow::{ensure, Context, Result};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{
    collections::BTreeSet,
    fs::{self, File, OpenOptions},
    io::Read,
    path::{Path, PathBuf},
};

#[derive(Debug, Clone)]
pub struct ImportResult {
    pub directory: PathBuf,
    pub manifest: ReleaseManifest,
    pub already_installed: bool,
    pub compatible_host: bool,
}

pub use astra_model::update::CurrentRelease;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TransactionPhase {
    Prepared,
    Committed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TransactionJournal {
    pub phase: TransactionPhase,
    pub before: Option<CurrentRelease>,
    /// None atomically clears the next-launch choice, returning to the bundle.
    pub after: Option<CurrentRelease>,
}

/// A complete snapshot supplied by the owner of running generations. GC never
/// guesses liveness from PIDs; callers must include every active/draining host,
/// worker, TUI, DLL and explicitly retained rollback generation.
#[derive(Debug, Clone, Default)]
pub struct ReleaseReferences {
    pub live: BTreeSet<String>,
    pub rollback: BTreeSet<String>,
}

#[derive(Debug, Clone)]
pub struct ReleaseStore {
    root: PathBuf,
}

impl ReleaseStore {
    pub fn new(state_dir: impl AsRef<Path>) -> Self {
        Self {
            root: state_dir.as_ref().join("updates"),
        }
    }
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn lock(&self) -> Result<File> {
        for directory in [
            &self.root,
            &self.root.join("releases"),
            &self.root.join("staging"),
        ] {
            fs::create_dir_all(directory)?;
            ensure!(
                package::plain_metadata(directory)?.is_dir(),
                "Update store path is not a directory"
            );
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(directory, fs::Permissions::from_mode(0o700))?;
            }
        }
        let path = self.root.join("store.lock");
        if fs::symlink_metadata(&path).is_ok() {
            ensure!(
                package::plain_metadata(&path)?.is_file(),
                "Invalid update lock"
            );
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        file.try_lock()
            .context("Another release operation is in progress")?;
        Ok(file)
    }

    pub fn release_dir(&self, release_id: &str) -> Result<PathBuf> {
        validate_id(release_id)?;
        Ok(self.root.join("releases").join(release_id))
    }

    pub fn import(
        &self,
        source: impl AsRef<Path>,
        options: &ImportOptions,
    ) -> Result<ImportResult> {
        let _lock = self.lock()?;
        self.recover_locked()?;
        let temporary = tempfile::Builder::new()
            .prefix("import-")
            .tempdir_in(self.root.join("staging"))?;
        let content = temporary.path().join("content");
        fs::create_dir(&content)?;
        let extracted = package::extract(source.as_ref(), &content, options)?;
        let manifest = package::verify(&extracted, options)?;
        let compatible_host = manifest.compatible_host(options.host_protocol);
        let directory = self.release_dir(&manifest.release_id)?;
        if fs::symlink_metadata(&directory).is_ok() {
            let existing = package::verify(&directory, options)?;
            ensure!(
                existing == manifest,
                "Release ID already contains different content"
            );
            return Ok(ImportResult {
                directory,
                manifest,
                already_installed: true,
                compatible_host,
            });
        }
        package::finish_files(&extracted, &manifest)?;
        // Staging and releases are within the same store/filesystem. A release
        // becomes visible only after complete validation, never file by file.
        fs::rename(&extracted, &directory)?;
        sync_directory(&self.root.join("releases"))?;
        Ok(ImportResult {
            directory,
            manifest,
            already_installed: false,
            compatible_host,
        })
    }

    pub fn current(&self) -> Result<Option<CurrentRelease>> {
        let _lock = self.lock()?;
        self.recover_locked()?;
        self.read_current()
    }

    fn read_current(&self) -> Result<Option<CurrentRelease>> {
        // JSON null is a committed reset; an absent record is a fresh store.
        let current: Option<CurrentRelease> =
            read_json::<Option<CurrentRelease>>(&self.root.join("current.json"))?.flatten();
        if let Some(current) = &current {
            validate_current(current)?;
        }
        Ok(current)
    }

    pub fn journal(&self) -> Result<Option<TransactionJournal>> {
        let _lock = self.lock()?;
        self.read_journal()
    }
    fn read_journal(&self) -> Result<Option<TransactionJournal>> {
        let journal: Option<TransactionJournal> = read_json(&self.root.join("transaction.json"))?;
        if let Some(journal) = &journal {
            if let Some(after) = &journal.after {
                validate_current(after)?;
            }
            if let Some(before) = &journal.before {
                validate_current(before)?;
            }
        }
        Ok(journal)
    }

    /// Atomically choose the next-launch release. This is deliberately
    /// separate from import and does not imply that any running worker changed.
    pub fn commit_current(&self, release_id: &str) -> Result<CurrentRelease> {
        self.commit_current_with_options(release_id, &ImportOptions::default())
    }
    pub fn commit_current_with_options(
        &self,
        release_id: &str,
        options: &ImportOptions,
    ) -> Result<CurrentRelease> {
        let _lock = self.lock()?;
        self.recover_locked()?;
        let mut activation_options = options.clone();
        activation_options.enforce_host_protocol = true;
        let manifest = package::verify(&self.release_dir(release_id)?, &activation_options)?;
        ensure!(
            manifest.release_id == release_id,
            "Installed manifest ID does not match directory"
        );
        let before = self.read_current()?;
        if let Some(current) = &before {
            if current.release_id == release_id {
                return Ok(current.clone());
            }
        }
        let after = CurrentRelease {
            release_id: release_id.into(),
            previous_release_id: before.as_ref().map(|v| v.release_id.clone()),
        };
        let mut journal = TransactionJournal {
            phase: TransactionPhase::Prepared,
            before,
            after: Some(after.clone()),
        };
        atomic_json(&self.root.join("transaction.json"), &journal)?;
        atomic_json(&self.root.join("current.json"), &after)?;
        journal.phase = TransactionPhase::Committed;
        atomic_json(&self.root.join("transaction.json"), &journal)?;
        Ok(after)
    }

    /// Clear only the next-launch release selection. Existing processes and
    /// installed releases remain untouched. The null record is published with
    /// the same recoverable transaction protocol as selecting a release.
    pub fn clear_current(&self) -> Result<()> {
        let _lock = self.lock()?;
        self.recover_locked()?;
        let before = self.read_current()?;
        if before.is_none() {
            return Ok(());
        }
        let mut journal = TransactionJournal {
            phase: TransactionPhase::Prepared,
            before,
            after: None,
        };
        atomic_json(&self.root.join("transaction.json"), &journal)?;
        atomic_json(
            &self.root.join("current.json"),
            &Option::<CurrentRelease>::None,
        )?;
        journal.phase = TransactionPhase::Committed;
        atomic_json(&self.root.join("transaction.json"), &journal)?;
        Ok(())
    }

    /// Resolve the default startup choice only after rechecking every file and
    /// host compatibility. Explicit --bin-dir callers can bypass this choice.
    pub fn resolve_current(&self) -> Result<Option<ImportResult>> {
        let _lock = self.lock()?;
        self.recover_locked()?;
        let Some(current) = self.read_current()? else {
            return Ok(None);
        };
        let directory = self.release_dir(&current.release_id)?;
        let options = ImportOptions {
            enforce_host_protocol: true,
            ..Default::default()
        };
        let manifest = package::verify(&directory, &options)?;
        ensure!(
            manifest.release_id == current.release_id,
            "Installed manifest ID does not match selected directory"
        );
        Ok(Some(ImportResult {
            directory,
            manifest,
            already_installed: true,
            compatible_host: true,
        }))
    }

    /// Resolve a process interruption using the atomic current record. A
    /// prepared journal without a published current record is abandoned;
    /// a published one is completed. Never executes rollback process actions.
    pub fn recover(&self) -> Result<()> {
        let _lock = self.lock()?;
        self.recover_locked()
    }
    fn recover_locked(&self) -> Result<()> {
        let Some(mut journal) = self.read_journal()? else {
            return Ok(());
        };
        let current = self.read_current()?;
        if current == journal.after {
            if let Some(after) = &journal.after {
                ensure!(
                    package::plain_metadata(&self.release_dir(&after.release_id)?)?.is_dir(),
                    "Selected release is missing"
                );
            }
            if journal.phase != TransactionPhase::Committed {
                journal.phase = TransactionPhase::Committed;
                atomic_json(&self.root.join("transaction.json"), &journal)?;
            }
        } else {
            ensure!(
                journal.phase == TransactionPhase::Prepared && current == journal.before,
                "Current release and transaction journal disagree; refusing automatic recovery"
            );
            fs::remove_file(self.root.join("transaction.json"))?;
            sync_directory(&self.root)?;
        }
        Ok(())
    }

    /// Only removes recognized releases unreferenced by the caller, current,
    /// previous, or a pending transaction. The caller must hold its generation
    /// lifecycle lock while supplying references and invoking this method.
    pub fn gc(&self, references: &ReleaseReferences) -> Result<Vec<String>> {
        let _lock = self.lock()?;
        self.recover_locked()?;
        let mut keep = references.live.clone();
        keep.extend(references.rollback.iter().cloned());
        if let Some(current) = self.read_current()? {
            keep.insert(current.release_id);
            keep.extend(current.previous_release_id);
        }
        for id in &keep {
            validate_id(id)?;
        }
        let releases = fs::canonicalize(self.root.join("releases"))?;
        let mut removed = Vec::new();
        for entry in fs::read_dir(&releases)? {
            let entry = entry?;
            let Some(id) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if validate_id(&id).is_err() || keep.contains(&id) {
                continue;
            }
            let path = entry.path();
            ensure!(
                package::plain_metadata(&path)?.is_dir(),
                "Unexpected file in release store"
            );
            let manifest: Option<ReleaseManifest> = read_json(&path.join(crate::MANIFEST_FILE))?;
            let Some(manifest) = manifest else {
                continue;
            };
            ensure!(manifest.release_id == id, "Installed release ID mismatch");
            // Validate every child before recursive removal, and enforce the
            // canonical target boundary even on Windows/reparse-point systems.
            check_tree(&path)?;
            let canonical = fs::canonicalize(&path)?;
            ensure!(
                canonical.parent() == Some(releases.as_path()),
                "GC target escaped release store"
            );
            fs::remove_dir_all(&canonical)?;
            removed.push(id);
        }
        sync_directory(&releases)?;
        removed.sort();
        Ok(removed)
    }
}

fn validate_current(current: &CurrentRelease) -> Result<()> {
    validate_id(&current.release_id)?;
    if let Some(id) = &current.previous_release_id {
        validate_id(id)?;
    }
    Ok(())
}
fn check_tree(path: &Path) -> Result<()> {
    if package::plain_metadata(path)?.is_dir() {
        for entry in fs::read_dir(path)? {
            check_tree(&entry?.path())?;
        }
    }
    Ok(())
}
fn read_json<T: DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        result => {
            result?;
        }
    }
    let metadata = package::plain_metadata(path)?;
    ensure!(
        metadata.is_file() && metadata.len() <= crate::MANIFEST_LIMIT,
        "Invalid release metadata file"
    );
    let mut bytes = Vec::new();
    File::open(path)?
        .take(crate::MANIFEST_LIMIT + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= crate::MANIFEST_LIMIT,
        "Release metadata exceeds limit"
    );
    Ok(Some(
        serde_json::from_slice(&bytes).with_context(|| format!("Read {}", path.display()))?,
    ))
}
fn atomic_json(path: &Path, value: &impl Serialize) -> Result<()> {
    if fs::symlink_metadata(path).is_ok() {
        ensure!(
            package::plain_metadata(path)?.is_file(),
            "Invalid release record"
        );
    }
    astra_platform::storage::atomic_write(
        path,
        &serde_json::to_vec(value)?,
        astra_platform::storage::WritePolicy::default(),
    )?;
    Ok(())
}

fn sync_directory(path: &Path) -> Result<()> {
    astra_platform::storage::sync_directory(path)?;
    Ok(())
}
