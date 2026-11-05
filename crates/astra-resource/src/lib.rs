//! Bounded resource loading with policy owned by each caller, never process globals.
use anyhow::{ensure, Context, Result};
use std::{
    fs,
    io::Read,
    path::Path,
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LoadPolicy {
    #[default]
    Refresh,
    PreferCache,
    ReadOnly,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CacheStatus {
    Local,
    Fresh,
    Stale,
    ReadOnly,
    Refreshed,
    Fallback,
}
#[derive(Debug)]
pub struct LoadResult<T> {
    pub bytes: Vec<u8>,
    pub value: T,
    pub cache_status: CacheStatus,
}
#[derive(Debug, Default)]
pub struct ResourceLoader {
    policy: LoadPolicy,
    served_stale: AtomicBool,
}
impl ResourceLoader {
    pub fn new(policy: LoadPolicy) -> Self {
        Self {
            policy,
            served_stale: AtomicBool::new(false),
        }
    }
    pub fn policy(&self) -> LoadPolicy {
        self.policy
    }
    pub fn served_stale(&self) -> bool {
        self.served_stale.load(Ordering::Relaxed)
    }
    pub fn load_resource(
        &self,
        base: &Path,
        path: &str,
        url: Option<&str>,
        interval: u64,
        limit: u64,
        validate: impl Fn(&[u8]) -> Result<()>,
    ) -> Result<Vec<u8>> {
        self.load(base, path, url, interval, limit, validate)
            .map(|result| result.bytes)
    }
    pub fn load_resource_with<T>(
        &self,
        base: &Path,
        path: &str,
        url: Option<&str>,
        interval: u64,
        limit: u64,
        decode: impl Fn(&[u8]) -> Result<T>,
    ) -> Result<(Vec<u8>, T)> {
        self.load(base, path, url, interval, limit, decode)
            .map(|result| (result.bytes, result.value))
    }
    pub fn load<T>(
        &self,
        base: &Path,
        path: &str,
        url: Option<&str>,
        interval: u64,
        limit: u64,
        decode: impl Fn(&[u8]) -> Result<T>,
    ) -> Result<LoadResult<T>> {
        let source = resolve(base, path);
        let path = if url.is_some() {
            astra_platform::storage::runtime_path(&source, "resources")?
        } else {
            source.clone()
        };
        let read = || -> Result<(Vec<u8>, T)> {
            let mut data = Vec::new();
            let input = if path.exists() { &path } else { &source };
            fs::File::open(input)?
                .take(limit.saturating_add(1))
                .read_to_end(&mut data)?;
            ensure!(data.len() as u64 <= limit, "resource exceeds size limit");
            let parsed = decode(&data)?;
            Ok((data, parsed))
        };
        let Some(url) = url else {
            return read()
                .map(|(bytes, value)| LoadResult {
                    bytes,
                    value,
                    cache_status: CacheStatus::Local,
                })
                .with_context(|| format!("load resource {}", path.display()));
        };
        validate_url(url)?;
        let cached = read().ok();
        if self.policy == LoadPolicy::ReadOnly {
            return cached.map(|(bytes, value)| LoadResult { bytes, value, cache_status: CacheStatus::ReadOnly }).context("rule generation requires a validated cached resource; refresh it through the resident rule owner first");
        }
        let fresh = fs::metadata(&path)
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age.as_secs() < interval);
        let serve = fresh || self.policy == LoadPolicy::PreferCache;
        let cached = match cached {
            Some(cached) if serve => {
                if !fresh {
                    self.served_stale
                        .store(true, std::sync::atomic::Ordering::Relaxed);
                }
                return Ok(LoadResult {
                    bytes: cached.0,
                    value: cached.1,
                    cache_status: if fresh {
                        CacheStatus::Fresh
                    } else {
                        CacheStatus::Stale
                    },
                });
            }
            cached => cached,
        };
        let update = (|| -> Result<(Vec<u8>, T)> {
            let response = ureq::AgentBuilder::new()
                .timeout(Duration::from_secs(15))
                .redirects(3)
                .build()
                .get(url)
                .call()
                .context("resource download failed")?;
            validate_redirect(url, response.get_url())?;
            let mut data = Vec::new();
            response
                .into_reader()
                .take(limit.saturating_add(1))
                .read_to_end(&mut data)?;
            ensure!(data.len() as u64 <= limit, "download exceeds size limit");
            let parsed = decode(&data).context("downloaded resource is invalid")?;
            astra_platform::storage::atomic_write(
                &path,
                &data,
                astra_platform::storage::WritePolicy::runtime(),
            )?;
            Ok((data, parsed))
        })();
        match (update, cached) {
            (Ok((bytes, value)), _) => Ok(LoadResult {
                bytes,
                value,
                cache_status: CacheStatus::Refreshed,
            }),
            (Err(error), Some(data)) => {
                tracing::warn!(%error,resource=%path.display(),"resource update failed; keeping validated cache");
                Ok(LoadResult {
                    bytes: data.0,
                    value: data.1,
                    cache_status: CacheStatus::Fallback,
                })
            }
            (Err(error), None) => Err(error),
        }
    }
}
pub use astra_model::config::resolve;
fn validate_url(url: &str) -> Result<()> {
    validate_remote_url(url).map(|_| ())
}

/// Remote subscriptions, providers and rule data accept HTTP(S) without URL
/// credentials. Plain HTTP stays allowed for providers that offer no TLS.
pub use astra_model::config::validate_remote_url;

/// A redirect may never downgrade HTTPS to plain HTTP: a URL configured as
/// HTTPS keeps its TLS protection. Configured plain HTTP may redirect anywhere.
pub use astra_model::config::validate_redirect;

pub fn load_resource(
    base: &Path,
    path: &str,
    url: Option<&str>,
    interval: u64,
    limit: u64,
    validate: impl Fn(&[u8]) -> Result<()>,
) -> Result<Vec<u8>> {
    ResourceLoader::default().load_resource(base, path, url, interval, limit, validate)
}
pub fn load_resource_with<T>(
    base: &Path,
    path: &str,
    url: Option<&str>,
    interval: u64,
    limit: u64,
    decode: impl Fn(&[u8]) -> Result<T>,
) -> Result<(Vec<u8>, T)> {
    ResourceLoader::default().load_resource_with(base, path, url, interval, limit, decode)
}

#[cfg(test)]
#[path = "test/resource_test.rs"]
mod tests;
