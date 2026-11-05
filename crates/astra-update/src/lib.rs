//! Local, immutable release storage. Importing a package never runs its code.
//!
//! SHA-256 detects corruption; it does not authenticate a publisher. Selecting
//! a release changes only the next-launch record, never running processes.
mod package;
mod store;

use anyhow::{bail, ensure, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
pub use store::{
    CurrentRelease, ImportResult, ReleaseReferences, ReleaseStore, TransactionJournal,
    TransactionPhase,
};

pub const HOST_PROTOCOL_VERSION: u32 = astra_model::HOST_PROTOCOL_VERSION as u32;
pub const TARGET: &str = env!("ASTRA_UPDATE_TARGET");
pub const MANIFEST_FILE: &str = "manifest.json";
pub const MANIFEST_LIMIT: u64 = 1024 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ProtocolRange {
    pub min: u32,
    pub max: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReleaseFile {
    pub path: String,
    pub size: u64,
    pub sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ReleaseManifest {
    pub schema_version: u32,
    /// Legacy manifests without a role are client worker bundles.
    #[serde(default)]
    pub role: ReleaseRole,
    pub release_id: String,
    pub target: String,
    pub host_protocol: ProtocolRange,
    pub components: BTreeMap<String, String>,
    pub files: Vec<ReleaseFile>,
    /// Build metadata is absent from legacy release manifests.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Only present in legacy packages built with the removed fast/runtime modes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cargo_profile: Option<String>,
}
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseRole {
    #[default]
    Client,
    Server,
    Controller,
    Web,
}

#[derive(Debug, Clone)]
pub struct ImportOptions {
    pub target: String,
    pub host_protocol: u32,
    /// False permits storing a package for a future host maintenance update.
    /// Activation always enforces compatibility regardless of this setting.
    pub enforce_host_protocol: bool,
    pub max_archive_bytes: u64,
    pub max_file_bytes: u64,
    pub max_total_bytes: u64,
    pub max_entries: usize,
}
impl Default for ImportOptions {
    fn default() -> Self {
        Self {
            target: TARGET.into(),
            host_protocol: HOST_PROTOCOL_VERSION,
            enforce_host_protocol: false,
            max_archive_bytes: 2 * 1024 * 1024 * 1024,
            max_file_bytes: 512 * 1024 * 1024,
            max_total_bytes: 2 * 1024 * 1024 * 1024,
            max_entries: 128,
        }
    }
}

pub(crate) fn validate_id(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 128
            && value != "."
            && value != ".."
            && value
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)),
        "Invalid release ID"
    );
    validate_path(value)?;
    Ok(())
}

/// One portable path grammar, including on Unix, prevents Windows aliases,
/// alternate streams and separators from acquiring meaning after extraction.
pub(crate) fn validate_path(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty() && value.len() <= 240,
        "Invalid package path length"
    );
    for component in value.split('/') {
        ensure!(
            !component.is_empty()
                && component != "."
                && component != ".."
                && !component.ends_with('.')
                && component
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b)),
            "Unsafe package path: {value}"
        );
        let stem = component.split('.').next().unwrap().to_ascii_uppercase();
        ensure!(
            !matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
                && !(stem.len() == 4
                    && (stem.starts_with("COM") || stem.starts_with("LPT"))
                    && matches!(stem.as_bytes()[3], b'1'..=b'9')),
            "Reserved package path: {value}"
        );
    }
    Ok(())
}

impl ReleaseManifest {
    pub fn validate(&self, options: &ImportOptions) -> Result<()> {
        ensure!(
            self.schema_version == 1,
            "Unsupported release manifest schema"
        );
        match (
            self.profile.as_deref(),
            self.mode.as_deref(),
            self.cargo_profile.as_deref(),
        ) {
            (None, None, None) => {}
            // Current packages: the workspace has exactly the dev and release Cargo profiles.
            (Some(profile), None, Some(cargo_profile)) => ensure!(
                matches!((profile, cargo_profile), ("debug", "dev") | ("release", "release")),
                "Invalid build metadata: profile={profile}, cargo_profile={cargo_profile}"
            ),
            // Packages built before the build modes were removed remain importable.
            (Some(profile), Some(mode), Some(cargo_profile)) => ensure!(
                matches!(
                    (profile, mode, cargo_profile),
                    ("debug", "fast", "dev")
                        | ("release", "fast", "release-fast")
                        | ("debug", "runtime", "dev-runtime")
                        | ("release", "runtime", "release-runtime")
                ),
                "Invalid build metadata: profile={profile}, mode={mode}, cargo_profile={cargo_profile}"
            ),
            _ => bail!("Build metadata requires profile and cargo_profile together"),
        }
        ensure!(self.role == ReleaseRole::Client,
            "This is a {:?} package, not a client worker update; install it through the corresponding service maintenance restart", self.role);
        validate_id(&self.release_id)?;
        ensure!(
            self.target == options.target,
            "Release target {} does not match {}",
            self.target,
            options.target
        );
        ensure!(
            self.host_protocol.min <= self.host_protocol.max,
            "Invalid host protocol range"
        );
        if options.enforce_host_protocol {
            ensure!(
                self.compatible_host(options.host_protocol),
                "Incompatible host protocol range"
            );
        }
        ensure!(
            !self.files.is_empty() && self.files.len() <= options.max_entries,
            "Too many or no release files"
        );
        let mut names = BTreeSet::new();
        let mut total = 0u64;
        for file in &self.files {
            validate_path(&file.path)?;
            ensure!(
                file.path != MANIFEST_FILE && names.insert(file.path.to_ascii_lowercase()),
                "Duplicate/reserved release file"
            );
            ensure!(
                file.size <= options.max_file_bytes,
                "Release file is too large"
            );
            total = total
                .checked_add(file.size)
                .ok_or_else(|| anyhow::anyhow!("Release size overflow"))?;
            ensure!(total <= options.max_total_bytes, "Release is too large");
            ensure!(
                file.sha256.len() == 64 && file.sha256.bytes().all(|b| b.is_ascii_hexdigit()),
                "Invalid SHA-256"
            );
        }
        let suffix = if options.target.contains("windows") {
            ".exe"
        } else {
            ""
        };
        for component in ["astra-proxy", "astra-rule", "astra-tui", "astra-host"] {
            ensure!(
                self.components
                    .get(component)
                    .is_some_and(|version| !version.trim().is_empty()),
                "Missing component version: {component}"
            );
            ensure!(
                self.files
                    .iter()
                    .any(|file| file.path == format!("{component}{suffix}")),
                "Missing executable: {component}"
            );
        }
        for (component, version) in &self.components {
            validate_path(component)?;
            ensure!(
                !component.contains('/') && !version.trim().is_empty() && version.len() <= 128,
                "Invalid component metadata"
            );
            ensure!(
                self.files
                    .iter()
                    .any(|file| file.path == format!("{component}{suffix}")),
                "Missing executable for component: {component}"
            );
        }
        Ok(())
    }

    pub fn compatible_host(&self, protocol: u32) -> bool {
        self.host_protocol.min <= protocol && protocol <= self.host_protocol.max
    }
}

#[cfg(test)]
#[path = "test/update_test.rs"]
mod tests;
