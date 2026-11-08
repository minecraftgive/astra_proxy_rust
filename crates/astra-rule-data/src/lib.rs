//! Shared rule-provider format and bounded resource loading.
pub mod geoip;
pub mod geosite;
use anyhow::Result;
pub use astra_model::config::ruleset::{compile_domain_regex, parse_classical, MatchSpec, RuleSet};
use std::path::Path;
#[cfg(test)]
use std::{
    fs,
    io::{Read, Write},
    time::Duration,
};
pub trait RuleSetExt {
    fn load(&self, base: &Path) -> Result<Vec<MatchSpec>>;
    fn load_with(&self, base: &Path, loader: &ResourceLoader) -> Result<Vec<MatchSpec>>;
}
impl RuleSetExt for RuleSet {
    fn load(&self, base: &Path) -> Result<Vec<MatchSpec>> {
        self.load_with(base, &ResourceLoader::default())
    }
    fn load_with(&self, base: &Path, loader: &ResourceLoader) -> Result<Vec<MatchSpec>> {
        self.validate()?;
        if self.path.is_none() {
            return self.parse_entries(&self.payload);
        }
        let (_, entries) = loader.load_resource_with(
            base,
            self.path.as_deref().unwrap(),
            self.url.as_deref(),
            self.interval,
            16 * 1024 * 1024,
            |data| self.parse(data),
        )?;
        Ok(entries)
    }
}
pub use astra_resource::{
    load_resource, load_resource_with, resolve, validate_redirect, validate_remote_url, LoadPolicy,
    ResourceLoader,
};

#[cfg(test)]
#[path = "test/rule_data_test.rs"]
mod tests;
