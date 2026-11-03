//! Bounded private audit metadata. Payloads and credentials are never accepted here.
use astra_platform::platform::OpenOptionsExt;
use std::{
    fs::{File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
};
pub struct Audit {
    path: PathBuf,
    file: Option<File>,
    written: u64,
}
impl Audit {
    pub fn new(path: &Path) -> anyhow::Result<Self> {
        astra_platform::storage::prepare_parent(path)?;
        anyhow::ensure!(
            !std::fs::symlink_metadata(path).is_ok_and(|m| m.file_type().is_symlink()),
            "Audit path cannot be a symlink"
        );
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(path)?;
        astra_platform::platform::set_private(path, 0o600)?;
        Ok(Self {
            path: path.to_owned(),
            written: file.metadata()?.len(),
            file: Some(file),
        })
    }
    pub fn record(&mut self, event: &str, id: Option<&str>) -> anyhow::Result<()> {
        if self.written >= 4 * 1024 * 1024 {
            self.file.take();
            let previous = self.path.with_extension("previous.jsonl");
            if previous.exists() {
                std::fs::remove_file(&previous)?;
            }
            std::fs::rename(&self.path, &previous)?;
            let replacement = Self::new(&self.path)?;
            self.file = replacement.file;
            self.written = 0;
        }
        let line = serde_json::to_vec(
            &serde_json::json!({"at_ms":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis().to_string(),"event":event,"request_id":id}),
        )?;
        let file = self
            .file
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("Audit file unavailable"))?;
        file.write_all(&line)?;
        file.write_all(b"\n")?;
        self.written += line.len() as u64 + 1;
        Ok(())
    }
}
