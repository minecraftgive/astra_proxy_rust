use crate::SubscriptionNodes;
use anyhow::{ensure, Result};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    path::Path,
};

pub fn read(path: &Path) -> Result<BTreeMap<String, SubscriptionNodes>> {
    read_in(&astra_platform::storage::runtime_root()?, path)
}
pub fn read_in(root: &Path, path: &Path) -> Result<BTreeMap<String, SubscriptionNodes>> {
    let base = std::fs::read_to_string(path)?;
    ensure!(base.len() <= 480 * 1024, "Configuration exceeds 480 KiB");
    let doc: toml::Value = toml::from_str(&base)?;
    let mut result = BTreeMap::new();
    let sources = doc
        .get("subscriptions")
        .and_then(toml::Value::as_table)
        .cloned()
        .unwrap_or_default();
    for (name, source) in sources
        .iter()
        .map(|(n, s)| (n.as_str(), s))
        .chain(doc.get("subscription").map(|s| ("__legacy__", s)))
    {
        let url = source
            .get("url")
            .and_then(toml::Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let tags = source
            .get("managed_nodes")
            .and_then(toml::Value::as_array)
            .map(|v| {
                v.iter()
                    .filter_map(|v| v.as_str().map(str::to_owned))
                    .collect()
            })
            .unwrap_or_default();
        result.insert(name.into(), SubscriptionNodes { url, tags });
    }
    // Core-owned snapshots are read locally; only names are sent to the editor.
    let cache = (|| -> Result<serde_json::Value> {
        let mut data = Vec::new();
        std::fs::File::open({
            let runtime = astra_platform::storage::runtime_path_in(root, path, "subscriptions")?;
            if runtime.exists() {
                runtime
            } else {
                path.with_extension("subscription-cache.json")
            }
        })?
        .take(16 * 1024 * 1024 + 1)
        .read_to_end(&mut data)?;
        ensure!(data.len() <= 16 * 1024 * 1024, "Cache exceeds size limit");
        let cache: serde_json::Value = serde_json::from_slice(&data)?;
        ensure!(
            cache.get("base").and_then(|v| v.as_str()) == Some(base.as_str()),
            "Cache is stale"
        );
        Ok(cache)
    })();
    if let Ok(cache) = cache {
        let effective: Option<toml::Value> = cache
            .get("effective")
            .and_then(|v| v.as_str())
            .and_then(|s| toml::from_str(s).ok());
        let available: BTreeSet<_> = effective
            .as_ref()
            .and_then(|v| v.get("outbounds"))
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
            .filter_map(|v| v.get("tag")?.as_str())
            .collect();
        for (name, view) in &mut result {
            let owned = cache
                .get("subscriptions")
                .and_then(|v| v.get(name))
                .or_else(|| {
                    if name == "__legacy__" {
                        cache.get("ownership")
                    } else {
                        None
                    }
                });
            if let Some(owned) =
                owned.filter(|v| v.get("url").and_then(|v| v.as_str()) == Some(view.url.as_str()))
            {
                view.tags = owned
                    .get("managed_nodes")
                    .and_then(|v| v.as_array())
                    .into_iter()
                    .flatten()
                    .filter_map(|v| v.as_str())
                    .filter(|tag| available.contains(tag))
                    .map(str::to_owned)
                    .collect();
            }
        }
    }
    for view in result.values_mut() {
        view.tags.sort();
        view.tags.dedup();
    }
    Ok(result)
}

#[cfg(test)]
#[path = "test/subscription_view_test.rs"]
mod tests;
