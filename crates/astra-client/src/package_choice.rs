//! A launch choice never changes the resident backend or the saved selection.
use crate::bootstrap::Options;
use anyhow::{ensure, Context, Result};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Package {
    pub directory: PathBuf,
    pub release_id: String,
    pub manifest_sha256: String,
}
impl Package {
    pub fn inspect(directory: &Path) -> Result<Self> {
        let directory = std::fs::canonicalize(directory)?;
        let manifest =
            astra_update::verify_installation_directory(&directory, &Default::default())?;
        let bytes = astra_platform::storage::read_bounded(
            &directory.join(astra_update::MANIFEST_FILE),
            astra_update::MANIFEST_LIMIT,
        )?;
        ensure!(
            serde_json::from_slice::<astra_update::ReleaseManifest>(&bytes)? == manifest,
            "Package changed during verification"
        );
        Ok(Self {
            directory,
            release_id: manifest.release_id,
            manifest_sha256: format!("{:x}", Sha256::digest(bytes)),
        })
    }
    pub fn verify(&self) -> Result<()> {
        ensure!(
            Self::inspect(&self.directory)? == *self,
            "The selected package changed; reopen it before installing or launching"
        );
        Ok(())
    }
    pub fn id(&self) -> String {
        let mut hash = Sha256::new();
        hash.update(self.directory.as_os_str().as_encoded_bytes());
        hash.update([0]);
        hash.update(self.manifest_sha256.as_bytes());
        format!("frontend:{:x}", hash.finalize())
    }
    pub fn label(&self) -> String {
        format!(
            "{} · {}\n{}",
            self.release_id,
            &self.manifest_sha256[..12],
            self.directory.display()
        )
    }
}

#[derive(Clone, Debug)]
pub struct Choice {
    pub opened: Package,
    pub selected: Package,
}
impl Choice {
    pub fn discover(options: &Options) -> Result<Option<Self>> {
        if options.explicit_bin
            || options.endpoint.is_some()
            || options.silent
            || options.daemon
            || options.shutdown
            || options.help
            || options.operation.is_some()
            || options.resume_ui
            || options.launch.window_id.is_some()
            || astra_management::maintenance::pending(&options.home)?
        {
            return Ok(None);
        }
        let directory = options
            .executable
            .parent()
            .context("Missing frontend directory")?;
        // Unpackaged development frontends retain their existing launch behavior.
        if !directory.join(astra_update::MANIFEST_FILE).is_file() {
            return Ok(None);
        }
        let opened = Package::inspect(directory)?;
        let Some((selected, _)) = crate::bootstrap::selected_startup_release(&options.home, false)?
        else {
            return Ok(None);
        };
        let selected = Package::inspect(&selected)?;
        if opened.directory == selected.directory
            || opened.manifest_sha256 == selected.manifest_sha256
        {
            return Ok(None);
        }
        Ok(Some(Self { opened, selected }))
    }
    pub fn choose(&self, opened: bool, options: &mut Options) -> Result<()> {
        let package = if opened { &self.opened } else { &self.selected };
        package.verify()?;
        let name = options.executable.file_name().context("Unknown frontend")?;
        ensure!(
            package.directory.join(name).is_file(),
            "Selected package lacks this frontend"
        );
        options.bin = package.directory.clone();
        options.explicit_bin = true;
        Ok(())
    }
}

#[cfg(test)]
#[path = "test/package_choice_test.rs"]
pub(crate) mod tests;
