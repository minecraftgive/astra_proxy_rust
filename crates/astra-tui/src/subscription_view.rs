use crate::protocol::SubscriptionNodes;
use anyhow::{ensure, Result};
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Read,
    path::Path,
};

pub fn read(path: &Path) -> Result<BTreeMap<String, SubscriptionNodes>> {
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
        std::fs::File::open(path.with_extension("subscription-cache.json"))?
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
mod tests {
    use super::*;
    #[test]
    fn sources_are_separate_and_stale_or_invalid_caches_are_ignored() {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory = std::env::temp_dir().join(format!(
            "astra-subscription-view-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("proxy.toml");
        let base="[subscriptions.a]\nurl='https://example.com/a'\n[subscriptions.b]\nurl='https://example.com/b'\n[subscription]\nurl='https://example.com/legacy'\nmanaged_nodes=['old']\n";
        std::fs::write(&path, base).unwrap();
        let cache = serde_json::json!({"base":base,"effective":"[[outbounds]]\ntag='one'\npassword='credential'\n[[outbounds]]\ntag='two'\n","subscriptions":{"a":{"url":"https://example.com/a","managed_nodes":["one","missing","one"]},"b":{"url":"https://example.com/b","managed_nodes":["two"]}},"ownership":{"url":"https://example.com/legacy","managed_nodes":["one"]}});
        let cache_path = path.with_extension("subscription-cache.json");
        std::fs::write(&cache_path, cache.to_string()).unwrap();
        let views = read(&path).unwrap();
        assert_eq!(views["a"].tags, ["one"]);
        assert_eq!(views["b"].tags, ["two"]);
        assert_eq!(views["__legacy__"].tags, ["one"]);
        assert!(!serde_json::to_string(&views)
            .unwrap()
            .contains("credential"));
        std::fs::write(&path, format!("{base}\n#changed")).unwrap();
        assert!(read(&path).unwrap()["a"].tags.is_empty());
        std::fs::write(&cache_path, "invalid").unwrap();
        assert_eq!(read(&path).unwrap()["__legacy__"].tags, ["old"]);
        std::fs::remove_file(cache_path).unwrap();
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir(directory).unwrap();
    }
}
