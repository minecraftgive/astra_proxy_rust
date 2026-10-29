//! Bounded Clash subscription and proxy-provider loading, shared with Proxy Core.
use crate::{convert, get, required, s, seq, yes, Converted, Options};
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_yaml::Value as Y;
use std::{collections::HashSet, io::Read, path::Path, time::Duration};

const LIMIT: u64 = 8 * 1024 * 1024;
pub mod diagnostics;
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Subscription {
    pub url: String,
    pub interval: u64,
    pub user_agent: String,
    pub import_groups: bool,
    pub managed_nodes: Vec<String>,
    pub managed_groups: Vec<String>,
}
impl Default for Subscription {
    fn default() -> Self {
        Self {
            url: String::new(),
            interval: 3600,
            user_agent: "clash.meta/astra".into(),
            import_groups: false,
            managed_nodes: vec![],
            managed_groups: vec![],
        }
    }
}
impl Subscription {
    pub fn validate(&self) -> Result<()> {
        validate_url(&self.url)?;
        ensure!(
            (60..=604800).contains(&self.interval),
            "subscription interval must be 60..604800 seconds"
        );
        ensure!(
            !self.user_agent.is_empty()
                && self.user_agent.len() <= 256
                && !self.user_agent.contains(['\r', '\n']),
            "invalid subscription user agent"
        );
        Ok(())
    }
}
pub fn validate_url(value: &str) -> Result<()> {
    astra_rule_data::validate_remote_url(value)
        .map(|_| ())
        .context("subscription requires HTTP(S) without URL credentials")
}
pub use astra_rule_data::validate_redirect;
pub fn download(url: &str, user_agent: &str) -> Result<String> {
    validate_url(url)?;
    let response = ureq::AgentBuilder::new()
        .timeout(Duration::from_secs(20))
        .redirects(3)
        .build()
        .get(url)
        .set("User-Agent", user_agent)
        .call()
        .map_err(|error| match error {
            ureq::Error::Status(code, _) => anyhow::anyhow!("subscription HTTP {code}"),
            ureq::Error::Transport(error) => anyhow::anyhow!(
                "subscription transport {:?}: {}",
                error.kind(),
                diagnostics::error(&anyhow::anyhow!(error.to_string()))
            ),
        })?;
    validate_redirect(url, response.get_url())?;
    bounded(response.into_reader())
}
fn bounded(reader: impl Read) -> Result<String> {
    let mut data = Vec::new();
    reader.take(LIMIT + 1).read_to_end(&mut data)?;
    ensure!(data.len() as u64 <= LIMIT, "subscription exceeds 8 MiB");
    String::from_utf8(data).context("subscription must be UTF-8 YAML")
}
fn matches(spec: &Y, node: &Y) -> Result<bool> {
    let name = required(node, "name")?;
    for (field, keep) in [("filter", true), ("exclude-filter", false)] {
        if let Some(pattern) = s(spec, field).filter(|p| !p.is_empty()) {
            let pattern = pattern.replace('`', "|");
            let regex = regex::RegexBuilder::new(&pattern)
                .size_limit(2 * 1024 * 1024)
                .build()
                .context("unsupported subscription filter expression")?;
            if regex.is_match(&name) != keep {
                return Ok(false);
            }
        }
    }
    if let Some(types) = s(spec, "exclude-type") {
        if types
            .split('|')
            .any(|kind| Some(kind) == get(node, "type").as_str())
        {
            return Ok(false);
        }
    }
    Ok(true)
}
/// Expand provider membership before the ordinary converter validates protocols/groups.
pub fn convert_clash(input: &str, options: &Options, user_agent: &str) -> Result<Converted> {
    convert_clash_with(input, options, user_agent, &download)
}
fn convert_clash_with(
    input: &str,
    options: &Options,
    user_agent: &str,
    download: &dyn Fn(&str, &str) -> Result<String>,
) -> Result<Converted> {
    let mut root: Y = serde_yaml::from_str(input).context("invalid Clash YAML")?;
    root.apply_merge()?;
    ensure!(root.is_mapping(), "Clash configuration must be a mapping");
    if get(&root, "proxy-providers").is_null() {
        return convert(input, options);
    }
    let providers = get(&root, "proxy-providers")
        .as_mapping()
        .context("proxy-providers must be a mapping")?
        .clone();
    ensure!(providers.len() <= 32, "too many proxy providers");
    let mut nodes = seq(&root, "proxies").to_vec();
    let mut names = std::collections::BTreeMap::new();
    for (name, spec) in providers {
        let name = name
            .as_str()
            .context("provider name must be a string")?
            .to_owned();
        let loaded;
        let provider = match required(&spec, "type")?.as_str() {
            "inline" => {
                loaded = get(&spec, "payload").clone();
                &loaded
            }
            "http" | "file" => {
                let text = if required(&spec, "type")? == "http" {
                    download(&required(&spec, "url")?, user_agent)
                        .with_context(|| format!("download HTTP proxy-provider {name}"))?
                } else {
                    ensure!(
                        !options.remote,
                        "remote subscriptions cannot reference local file providers"
                    );
                    bounded(std::fs::File::open(
                        options.base_dir.join(required(&spec, "path")?),
                    )?)?
                };
                let mut doc: Y =
                    serde_yaml::from_str(&text).context("invalid proxy-provider YAML")?;
                doc.apply_merge()?;
                loaded = get(&doc, "proxies").clone();
                &loaded
            }
            _ => anyhow::bail!("unsupported proxy-provider type"),
        };
        ensure!(
            get(&spec, "override").is_null(),
            "proxy-provider override is not supported; refusing to change node semantics"
        );
        let entries = provider
            .as_sequence()
            .context("proxy-provider requires a proxies/payload array")?;
        let entries = entries
            .iter()
            .filter_map(|v| match matches(&spec, v) {
                Ok(true) => Some(Ok(v.clone())),
                Ok(false) => None,
                Err(e) => Some(Err(e)),
            })
            .collect::<Result<Vec<_>>>()?;
        ensure!(
            nodes.len() + entries.len() <= 4096,
            "too many subscription nodes"
        );
        names.insert(
            name,
            entries
                .iter()
                .map(|v| required(v, "name"))
                .collect::<Result<Vec<_>>>()?,
        );
        nodes.extend(entries);
    }
    let mut groups = seq(&root, "proxy-groups").to_vec();
    for group in &mut groups {
        let mut members = seq(group, "proxies").to_vec();
        let all_providers = yes(group, "include-all-providers") || yes(group, "include-all");
        let providers = if all_providers {
            names.keys().cloned().collect::<Vec<_>>()
        } else {
            seq(group, "use")
                .iter()
                .map(|v| {
                    v.as_str()
                        .map(str::to_owned)
                        .context("provider reference must be a string")
                })
                .collect::<Result<Vec<_>>>()?
        };
        let mut added = vec![];
        if yes(group, "include-all-proxies") || yes(group, "include-all") {
            added.extend(
                seq(&root, "proxies")
                    .iter()
                    .map(|v| required(v, "name"))
                    .collect::<Result<Vec<_>>>()?,
            );
        }
        for provider in providers {
            for member in names
                .get(&provider)
                .context("proxy group references unknown provider")?
            {
                added.push(member.clone());
            }
        }
        added.sort();
        added.dedup();
        for member in added {
            let node = nodes
                .iter()
                .find(|v| get(v, "name").as_str() == Some(&member))
                .context("missing provider node")?;
            if matches(group, node)? && !members.iter().any(|v| v.as_str() == Some(&member)) {
                members.push(Y::String(member));
            }
        }
        let mapping = group
            .as_mapping_mut()
            .context("proxy group must be a mapping")?;
        // Provider-only candidates were filtered above; do not turn them into all nodes.
        mapping.insert(Y::String("include-all".into()), Y::Bool(false));
        mapping.insert(Y::String("include-all-proxies".into()), Y::Bool(false));
        group
            .as_mapping_mut()
            .context("proxy group must be a mapping")?
            .insert(Y::String("proxies".into()), Y::Sequence(members));
    }
    let mapping = root.as_mapping_mut().unwrap();
    mapping.remove(Y::String("proxy-providers".into()));
    mapping.insert(Y::String("proxies".into()), Y::Sequence(nodes));
    mapping.insert(Y::String("proxy-groups".into()), Y::Sequence(groups));
    let mut result = convert(&serde_yaml::to_string(&root)?, options)?;
    result.warnings.insert(
        0,
        "已展开 proxy-providers；下列行号对应展开后的 YAML".into(),
    );
    Ok(result)
}
/// Subscription imports deliberately ignore inbound/routing configuration.
pub fn convert_nodes(
    input: &str,
    options: &Options,
    user_agent: &str,
    import_groups: bool,
) -> Result<Converted> {
    convert_nodes_with(input, options, user_agent, import_groups, &download)
}
fn convert_nodes_with(
    input: &str,
    options: &Options,
    user_agent: &str,
    import_groups: bool,
    download: &dyn Fn(&str, &str) -> Result<String>,
) -> Result<Converted> {
    if crate::share::is_share(input) {
        return crate::share::convert_links(input, options);
    }
    let mut root: Y = serde_yaml::from_str(input).context("invalid Clash YAML")?;
    root.apply_merge()?;
    let root = root
        .as_mapping()
        .context("Clash configuration must be a mapping")?;
    let mut nodes = serde_yaml::Mapping::new();
    for key in ["proxies", "proxy-providers", "proxy-groups", "dns"] {
        if key == "proxy-groups" && !import_groups {
            continue;
        }
        if let Some(value) = root.get(Y::String(key.into())) {
            nodes.insert(Y::String(key.into()), value.clone());
        }
    }
    convert_clash_with(
        &serde_yaml::to_string(&nodes)?,
        options,
        user_agent,
        download,
    )
}
pub fn fetch(config: &Subscription, base_dir: &Path) -> Result<Converted> {
    fetch_with(config, base_dir, &download)
}
/// Let the embedding core supply its protected network path, including providers.
pub fn fetch_with(
    config: &Subscription,
    base_dir: &Path,
    download: &dyn Fn(&str, &str) -> Result<String>,
) -> Result<Converted> {
    config.validate()?;
    let text = download(&config.url, &config.user_agent).context("download subscription")?;
    ensure!(!text.trim_start_matches('\u{feff}').trim_start().starts_with('<'),
        "subscription returned HTML/XML instead of Clash YAML or share links; check URL and authorization");
    let converted = convert_nodes_with(
        &text,
        &Options {
            base_dir: base_dir.into(),
            remote: true,
            ..Default::default()
        },
        &config.user_agent,
        config.import_groups,
        download,
    )
    .context("parse/convert subscription nodes")?;
    ensure!(
        converted.nodes > 0,
        "subscription contains no supported nodes: {}",
        diagnostics::error(&anyhow::anyhow!(converted
            .warnings
            .iter()
            .take(8)
            .cloned()
            .collect::<Vec<_>>()
            .join("; ")))
    );
    Ok(converted)
}
/// Replace only subscription-owned nodes/groups, preserving local runtime settings.
pub fn merge(current: &str, converted: &str, ownership: &mut Subscription) -> Result<String> {
    let mut current: toml::Table = toml::from_str(current)?;
    let remote: toml::Table = toml::from_str(converted)?;
    // Import DNS on first use; explicit local settings and previous imports
    // remain editable and win over subsequent subscription refreshes.
    if !current.contains_key("dns") {
        if let Some(mut dns) = remote.get("dns").cloned() {
            // Resolvers come from the subscription, but where this machine
            // listens is a local decision: keep the loopback default.
            if let Some(table) = dns.as_table_mut() {
                table.remove("listen");
            }
            current.insert("dns".into(), dns);
        }
    }
    let incoming = remote
        .get("outbounds")
        .and_then(|v| v.as_array())
        .context("subscription has no nodes")?;
    ensure!(
        !incoming.is_empty(),
        "subscription contains no supported nodes"
    );
    let local = current
        .entry("outbounds")
        .or_insert_with(|| toml::Value::Array(vec![]))
        .as_array_mut()
        .context("invalid outbounds")?;
    local.retain(|v| {
        !ownership
            .managed_nodes
            .iter()
            .any(|n| v.get("tag").and_then(|v| v.as_str()) == Some(n))
    });
    let mut tags: HashSet<String> = local
        .iter()
        .filter_map(|v| v.get("tag")?.as_str().map(str::to_owned))
        .collect();
    let mut managed_nodes = vec![];
    for node in incoming {
        let tag = node
            .get("tag")
            .and_then(|v| v.as_str())
            .context("node has no tag")?;
        ensure!(
            tags.insert(tag.into()),
            "subscription conflicts with a local node"
        );
        managed_nodes.push(tag.into());
        local.push(node.clone());
    }
    let groups = current
        .entry("proxy_groups")
        .or_insert_with(|| toml::Value::Table(Default::default()))
        .as_table_mut()
        .context("invalid groups")?;
    let old = groups.clone();
    if ownership.import_groups {
        for name in &ownership.managed_groups {
            groups.remove(name);
        }
    }
    let mut managed_groups = vec![];
    if let Some(remote) = remote
        .get("proxy_groups")
        .and_then(|v| v.as_table())
        .filter(|_| ownership.import_groups)
    {
        for (name, group) in remote {
            // Explicit local groups take precedence over subscription groups.
            if groups.contains_key(name) {
                continue;
            }
            let mut group = group.clone();
            if let Some(previous) = old.get(name).and_then(|v| v.as_table()) {
                for (key, value) in previous {
                    if key.starts_with("disconnect_on_") {
                        group
                            .as_table_mut()
                            .unwrap()
                            .insert(key.clone(), value.clone());
                    }
                }
            }
            // Preserve a manual selection while it remains a valid candidate.
            if let Some(selected) = old
                .get(name)
                .and_then(|v| v.get("selected"))
                .and_then(|v| v.as_str())
            {
                if group
                    .get("members")
                    .and_then(|v| v.as_array())
                    .is_some_and(|m| m.iter().any(|m| m.as_str() == Some(selected)))
                    || (group.get("include_all").and_then(|v| v.as_bool()) == Some(true)
                        && tags.contains(selected))
                {
                    group
                        .as_table_mut()
                        .unwrap()
                        .insert("selected".into(), selected.into());
                }
            }
            groups.insert(name.clone(), group);
            managed_groups.push(name.clone());
        }
    }
    ownership.managed_nodes = managed_nodes;
    ownership.managed_groups = managed_groups;
    Ok(toml::to_string_pretty(&current)?)
}

/// Materialize a stable group per subscription. Local group settings are retained.
pub fn prepare_groups(document: &mut toml::Table) -> Result<()> {
    let mut sources = document
        .get("subscriptions")
        .and_then(|v| v.as_table())
        .cloned()
        .unwrap_or_default();
    if let Some(legacy) = document.get("subscription") {
        sources.insert("__legacy__".into(), legacy.clone());
    }
    let nodes: HashSet<String> = document
        .get("outbounds")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|v| v.get("tag")?.as_str().map(str::to_owned))
        .collect();
    let groups = document
        .entry("proxy_groups")
        .or_insert_with(|| toml::Value::Table(Default::default()))
        .as_table_mut()
        .context("invalid groups")?;
    for (source, settings) in sources {
        let existing = groups
            .iter()
            .find(|(_, g)| g.get("subscription").and_then(|v| v.as_str()) == Some(source.as_str()))
            .map(|(n, _)| n.clone());
        let name = if let Some(name) = existing {
            name
        } else {
            let stem: String = source
                .chars()
                .scan(0usize, |len, c| {
                    *len += c.len_utf8();
                    (*len <= 42).then_some(c)
                })
                .collect();
            let stem = if source == "__legacy__" {
                "subscription".to_owned()
            } else {
                stem
            };
            let mut name = stem.clone();
            let mut index = 1;
            while name == "PROXY"
                || astra_ipc::groups::builtin(&name)
                || nodes.contains(&name)
                || groups.contains_key(&name)
            {
                name = format!("{stem}-subscription-{index}");
                index += 1;
            }
            name
        };
        let group = groups
            .entry(name)
            .or_insert_with(|| toml::Value::Table(Default::default()))
            .as_table_mut()
            .context("invalid subscription group")?;
        group.insert("subscription".into(), source.into());
        group.entry("mode").or_insert_with(|| "select".into());
        group
            .entry("include_in_proxy")
            .or_insert_with(|| true.into());
        let members: Vec<_> = settings
            .get("managed_nodes")
            .and_then(|v| v.as_array())
            .into_iter()
            .flatten()
            .filter(|v| v.as_str().is_some_and(|s| nodes.contains(s)))
            .cloned()
            .collect();
        if group.get("selected").is_some_and(|s| !members.contains(s)) {
            group.remove("selected");
        }
        group.insert("members".into(), members.into());
    }
    Ok(())
}

/// Named subscription refresh updates only that source and its generated group.
pub fn merge_named(
    current: &str,
    converted: &str,
    ownership: &mut Subscription,
    source: &str,
) -> Result<String> {
    let text = merge(current, converted, ownership)?;
    let mut document: toml::Table = toml::from_str(&text)?;
    let settings = if source == "__legacy__" {
        document.get_mut("subscription")
    } else {
        document
            .get_mut("subscriptions")
            .and_then(|v| v.as_table_mut())
            .and_then(|s| s.get_mut(source))
    }
    .context("subscription source missing")?
    .as_table_mut()
    .context("invalid subscription")?;
    settings.insert(
        "managed_nodes".into(),
        ownership
            .managed_nodes
            .iter()
            .cloned()
            .map(toml::Value::String)
            .collect::<Vec<_>>()
            .into(),
    );
    settings.insert(
        "managed_groups".into(),
        ownership
            .managed_groups
            .iter()
            .cloned()
            .map(toml::Value::String)
            .collect::<Vec<_>>()
            .into(),
    );
    prepare_groups(&mut document)?;
    let groups: std::collections::BTreeMap<String, astra_ipc::groups::Group> =
        document["proxy_groups"].clone().try_into()?;
    let nodes = document
        .get("outbounds")
        .and_then(|v| v.as_array())
        .into_iter()
        .flatten()
        .filter_map(|v| v.get("tag")?.as_str().map(str::to_owned))
        .collect();
    let expanded = astra_ipc::groups::expand(&groups, &nodes).map_err(anyhow::Error::msg)?;
    for (name, group) in &expanded {
        if group.selected.as_ref().is_some_and(|tag| {
            astra_ipc::groups::member_path(&expanded, name, tag).is_none()
                && !astra_ipc::groups::pending_subscription_selection(&expanded, name)
        }) {
            document["proxy_groups"][name]
                .as_table_mut()
                .unwrap()
                .remove("selected");
        }
    }
    Ok(toml::to_string_pretty(&document)?)
}
