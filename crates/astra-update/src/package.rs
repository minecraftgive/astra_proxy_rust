use crate::{validate_path, ImportOptions, ReleaseManifest, MANIFEST_FILE, MANIFEST_LIMIT};
use anyhow::{bail, ensure, Context, Result};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet,
    fs::{self, File, Metadata, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

pub(crate) fn plain_metadata(path: &Path) -> Result<Metadata> {
    let metadata = fs::symlink_metadata(path)?;
    ensure!(
        !metadata.file_type().is_symlink(),
        "Symbolic link is not permitted: {}",
        path.display()
    );
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        ensure!(
            metadata.file_attributes() & 0x400 == 0,
            "Reparse point is not permitted: {}",
            path.display()
        );
    }
    ensure!(
        metadata.is_file() || metadata.is_dir(),
        "Special file is not permitted: {}",
        path.display()
    );
    Ok(metadata)
}

struct Extractor<'a> {
    destination: &'a Path,
    options: &'a ImportOptions,
    names: BTreeSet<String>,
    total: u64,
}
impl Extractor<'_> {
    fn entry(
        &mut self,
        name: &str,
        directory: bool,
        size: u64,
        mut reader: impl Read,
    ) -> Result<()> {
        let name = if directory {
            name.strip_suffix('/').unwrap_or(name)
        } else {
            name
        };
        validate_path(name)?;
        ensure!(
            self.names.insert(name.to_ascii_lowercase()),
            "Duplicate package entry: {name}"
        );
        ensure!(
            self.names.len() <= self.options.max_entries,
            "Too many package entries"
        );
        let target = self.destination.join(name);
        if directory {
            ensure!(size == 0, "Directory has payload");
            fs::create_dir_all(&target)?;
            return Ok(());
        }
        ensure!(
            size <= self.options.max_file_bytes,
            "Package file is too large: {name}"
        );
        self.total = self
            .total
            .checked_add(size)
            .context("Package size overflow")?;
        ensure!(
            self.total <= self.options.max_total_bytes,
            "Expanded package is too large"
        );
        fs::create_dir_all(target.parent().context("Missing parent")?)?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&target)?;
        let copied = std::io::copy(&mut reader.by_ref().take(size + 1), &mut file)?;
        ensure!(copied == size, "Package entry size mismatch: {name}");
        file.flush()?;
        file.sync_all()?;
        Ok(())
    }

    fn directory(&mut self, source: &Path, prefix: &str) -> Result<()> {
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            let component = entry
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("Package path is not UTF-8"))?;
            let name = if prefix.is_empty() {
                component
            } else {
                format!("{prefix}/{component}")
            };
            let metadata = plain_metadata(&entry.path())?;
            if metadata.is_dir() {
                self.entry(&name, true, 0, std::io::empty())?;
                self.directory(&entry.path(), &name)?;
            } else {
                self.entry(&name, false, metadata.len(), File::open(entry.path())?)?;
            }
        }
        Ok(())
    }
}

pub(crate) fn extract(
    source: &Path,
    destination: &Path,
    options: &ImportOptions,
) -> Result<PathBuf> {
    let metadata = plain_metadata(source)?;
    let mut extractor = Extractor {
        destination,
        options,
        names: BTreeSet::new(),
        total: 0,
    };
    if metadata.is_dir() {
        extractor.directory(source, "")?;
    } else {
        ensure!(
            metadata.len() <= options.max_archive_bytes,
            "Archive is too large"
        );
        let filename = source
            .file_name()
            .and_then(|v| v.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if filename.ends_with(".zip") {
            let mut archive = zip::ZipArchive::new(File::open(source)?)?;
            ensure!(archive.len() <= options.max_entries, "Too many ZIP entries");
            for index in 0..archive.len() {
                let mut file = archive.by_index(index)?;
                if let Some(mode) = file.unix_mode() {
                    let kind = mode & 0o170000;
                    ensure!(
                        kind == 0 || kind == 0o100000 || kind == 0o040000,
                        "ZIP links/special files are not permitted"
                    );
                }
                let name = file.name().to_owned();
                extractor.entry(&name, file.is_dir(), file.size(), &mut file)?;
            }
        } else if filename.ends_with(".tar.gz") || filename.ends_with(".tgz") {
            let decoder = flate2::read::GzDecoder::new(File::open(source)?);
            let budget = options
                .max_total_bytes
                .checked_add((options.max_entries as u64).saturating_mul(4096))
                .and_then(|v| v.checked_add(1024))
                .context("Archive limit overflow")?;
            // Bound even TAR extension headers, which tar may consume before
            // returning a normal entry to the caller.
            let mut archive = tar::Archive::new(decoder.take(budget + 1));
            for entry in archive.entries()? {
                let mut entry = entry?;
                let kind = entry.header().entry_type();
                ensure!(
                    kind.is_file() || kind.is_dir(),
                    "TAR links/special files are not permitted"
                );
                let name = std::str::from_utf8(&entry.path_bytes())?.to_owned();
                extractor.entry(&name, kind.is_dir(), entry.size(), &mut entry)?;
            }
            // Finish gzip decoding to verify its trailer/CRC and reject trailing
            // expanded data beyond the archive budget (including tar padding).
            let mut decoder = archive.into_inner();
            std::io::copy(&mut decoder, &mut std::io::sink())?;
            ensure!(decoder.limit() > 0, "Expanded archive exceeds limit");
        } else {
            bail!("Expected a release directory, ZIP or .tar.gz archive");
        }
    }
    let root_manifest = destination.join(MANIFEST_FILE);
    if root_manifest.is_file() {
        return Ok(destination.to_owned());
    }
    // The existing build script wraps client archives in exactly one client/.
    let entries: Vec<_> = fs::read_dir(destination)?.collect::<std::io::Result<_>>()?;
    ensure!(
        entries.len() == 1
            && entries[0].file_name() == "client"
            && entries[0].file_type()?.is_dir(),
        "Missing root manifest.json or client/manifest.json"
    );
    Ok(destination.join("client"))
}

fn inventory(
    directory: &Path,
    prefix: &str,
    files: &mut BTreeSet<String>,
    options: &ImportOptions,
    count: &mut usize,
) -> Result<()> {
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        *count += 1;
        ensure!(*count <= options.max_entries, "Too many installed entries");
        let component = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("Invalid installed path"))?;
        let name = if prefix.is_empty() {
            component
        } else {
            format!("{prefix}/{component}")
        };
        validate_path(&name)?;
        let metadata = plain_metadata(&entry.path())?;
        if metadata.is_dir() {
            inventory(&entry.path(), &name, files, options, count)?;
        } else {
            ensure!(files.insert(name.to_owned()), "Duplicate installed path");
        }
    }
    Ok(())
}

pub(crate) fn verify(directory: &Path, options: &ImportOptions) -> Result<ReleaseManifest> {
    ensure!(
        plain_metadata(directory)?.is_dir(),
        "Release is not a directory"
    );
    let manifest_path = directory.join(MANIFEST_FILE);
    let metadata = plain_metadata(&manifest_path)?;
    ensure!(
        metadata.is_file() && metadata.len() <= MANIFEST_LIMIT,
        "Manifest is too large or not a file"
    );
    let mut bytes = Vec::new();
    File::open(&manifest_path)?
        .take(MANIFEST_LIMIT + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MANIFEST_LIMIT,
        "Manifest is too large"
    );
    let manifest: ReleaseManifest =
        serde_json::from_slice(&bytes).context("Invalid release manifest")?;
    manifest.validate(options)?;
    let mut files = BTreeSet::new();
    inventory(directory, "", &mut files, options, &mut 0)?;
    let mut expected: BTreeSet<String> = manifest.files.iter().map(|f| f.path.clone()).collect();
    expected.insert(MANIFEST_FILE.into());
    ensure!(
        files == expected,
        "Package contains missing or unlisted files"
    );
    for entry in &manifest.files {
        let path = directory.join(&entry.path);
        let metadata = plain_metadata(&path)?;
        ensure!(
            metadata.is_file() && metadata.len() == entry.size,
            "File size mismatch: {}",
            entry.path
        );
        let mut file = File::open(&path)?.take(entry.size + 1);
        let mut hash = Sha256::new();
        let read = std::io::copy(&mut file, &mut hash)?;
        ensure!(
            read == entry.size
                && format!("{:x}", hash.finalize()).eq_ignore_ascii_case(&entry.sha256),
            "SHA-256 mismatch: {}",
            entry.path
        );
    }
    Ok(manifest)
}

pub(crate) fn finish_files(directory: &Path, manifest: &ReleaseManifest) -> Result<()> {
    #[cfg(unix)]
    for entry in &manifest.files {
        let path = directory.join(&entry.path);
        // Keep a writable descriptor while persisting the permission change;
        // fsync on a read-only descriptor is not portable across all targets.
        let file = OpenOptions::new().write(true).open(&path)?;
        {
            use std::os::unix::fs::PermissionsExt;
            let executable = manifest.components.contains_key(&entry.path);
            fs::set_permissions(
                &path,
                fs::Permissions::from_mode(if executable { 0o500 } else { 0o400 }),
            )?;
        }
        file.sync_all()?;
    }
    #[cfg(not(unix))]
    let _ = (directory, manifest);
    Ok(())
}
