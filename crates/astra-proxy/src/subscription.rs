//! Proxy Core owns the subscription's last validated runtime snapshot.
use crate::App;
use anyhow::{ensure, Result};
use astra_convert::subscription::Subscription;
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Write},
    path::{Path, PathBuf},
};

#[derive(Serialize, Deserialize)]
pub(crate) struct Cache {
    pub base: String,
    pub effective: String,
    pub ownership: Subscription,
}
fn path(config: &Path) -> PathBuf {
    config.with_extension("subscription-cache.json")
}
pub(crate) fn read(config: &Path, base: &str) -> Option<Cache> {
    let load = || -> Result<Cache> {
        let mut bytes = Vec::new();
        std::fs::File::open(path(config))?
            .take(16 * 1024 * 1024 + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= 16 * 1024 * 1024,
            "subscription cache too large"
        );
        let cache: Cache = serde_json::from_slice(&bytes)?;
        ensure!(
            cache.base == base,
            "subscription cache belongs to a different configuration"
        );
        Ok(cache)
    };
    load().ok()
}
pub(crate) fn cached_app(config: &Path, original: App) -> App {
    if original.config.subscription.is_none() {
        return original;
    }
    if let Some(cache) = read(config, &original.source_text) {
        if let Ok(mut app) = App::from_text(config, cache.effective) {
            app.source_text = original.source_text;
            tracing::info!("validated subscription cache loaded");
            return app;
        }
    }
    original
}
pub(crate) fn save(config: &Path, cache: &Cache) -> Result<()> {
    let data = serde_json::to_vec(cache)?;
    ensure!(
        data.len() <= 16 * 1024 * 1024,
        "subscription cache too large"
    );
    let target = path(config);
    let temporary = target.with_extension(format!("{}.tmp", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    let result = (|| -> Result<()> {
        file.write_all(&data)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, &target)?;
        Ok(())
    })();
    let _ = std::fs::remove_file(temporary);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cache_survives_replacement_and_is_ignored_after_config_changes_or_corruption() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory =
            std::env::temp_dir().join(format!("astra-node-cache-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        let config = directory.join("proxy.toml");
        let base = "standalone=true\nstandalone_outbound='PROXY'\nsocks5_listen='127.0.0.1:1080'\n[subscription]\nurl='http://127.0.0.1:1/config'\n";
        let settings = Subscription {
            url: "http://127.0.0.1:1/config".into(),
            ..Default::default()
        };
        let effective = format!(
            "{base}\n[[outbounds]]\nkind='socks5'\ntag='cached-node'\nserver='127.0.0.1:1234'\n"
        );
        let cache = Cache {
            base: base.into(),
            effective: effective.clone(),
            ownership: settings,
        };
        save(&config, &cache).unwrap();
        save(&config, &cache).unwrap();
        assert_eq!(read(&config, base).unwrap().effective, effective);
        let app = cached_app(&config, App::from_text(&config, base.into()).unwrap());
        assert!(app.outbounds.contains_key("cached-node"));
        assert_eq!(app.source_text, base);
        assert!(read(&config, &format!("{base}\n# changed")).is_none());
        std::fs::write(path(&config), b"invalid").unwrap();
        let app = cached_app(&config, App::from_text(&config, base.into()).unwrap());
        assert!(app.outbounds.is_empty());
        std::fs::remove_file(path(&config)).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }
}
