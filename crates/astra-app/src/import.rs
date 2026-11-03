use astra_model::CoreKind as Kind;
fn t(source: &str) -> &str {
    source
}
use anyhow::{ensure, Context, Result};
use std::{
    io::Read,
    path::{Path, PathBuf},
};

const LIMIT: usize = 480 * 1024;

/// Terminals paste dropped paths, commonly wrapped in quotes. No shell is run.
pub fn path(input: &str, directory: &Path) -> Result<PathBuf> {
    let input = input.trim();
    let input = input.strip_prefix("& ").unwrap_or(input).trim();
    let input = if input.len() >= 2
        && ((input.starts_with('"') && input.ends_with('"'))
            || (input.starts_with('\'') && input.ends_with('\'')))
    {
        &input[1..input.len().saturating_sub(1)]
    } else {
        input
    };
    ensure!(
        !input.is_empty() && !input.contains(['\n', '\r', '\0']),
        t("Choose one TOML or YAML file")
    );
    let path = PathBuf::from(input);
    Ok(if path.is_absolute() {
        path
    } else {
        directory.join(path)
    })
}

pub struct Imported {
    pub text: String,
    pub warnings: Vec<String>,
}

pub fn links(input: &str, kind: Kind, current: &str) -> Result<Imported> {
    ensure!(
        kind == Kind::Proxy,
        t("Configuration does not match this core")
    );
    ensure!(input.len() <= LIMIT, t("Configuration exceeds 480 KiB"));
    let converted = astra_convert::share::convert_links(input, &Default::default())?;
    let imported: toml_edit::DocumentMut = converted.proxy.parse()?;
    append_nodes(imported, current)
}

#[derive(Clone, Copy, serde::Serialize, serde::Deserialize)]
pub enum MergeMode {
    Append,
    Replace,
    Overwrite(usize),
}

/// An input accepted by [`load_node`] and [`load_rules`].
///
/// Strings are treated as pasted native TOML (or a share link), unless they
/// name an existing file. Paths are read without invoking a shell. The trait
/// is public so UI callers can pass either a text field or a `Path` without
/// converting it first.
pub trait ImportInput {
    fn read_import(&self) -> Result<(String, PathBuf)>;
}

impl ImportInput for &str {
    fn read_import(&self) -> Result<(String, PathBuf)> {
        read_input(self, &std::env::current_dir()?)
    }
}

impl ImportInput for String {
    fn read_import(&self) -> Result<(String, PathBuf)> {
        read_input(self, &std::env::current_dir()?)
    }
}

impl ImportInput for &Path {
    fn read_import(&self) -> Result<(String, PathBuf)> {
        read_file(self)
    }
}

impl ImportInput for PathBuf {
    fn read_import(&self) -> Result<(String, PathBuf)> {
        read_file(self)
    }
}

/// Import one or more native outbounds into the current proxy TOML.
///
/// The input may be a pasted `vless://`/other supported share link, a native
/// `[[outbounds]]` fragment, a complete TOML configuration, or a path to one.
/// Existing draft settings are retained; only `outbounds` is appended.
pub fn load_node<I: ImportInput>(input: I, current: &str) -> Result<Imported> {
    let (source, _) = input.read_import()?;
    load_node_source(&source, current)
}

/// Import rules and rule sets from native TOML into the current rule TOML.
///
/// Both `[[rules]]` fragments and complete rule-core TOML are accepted. Rule
/// set definitions are merged by name and conflicting definitions are rejected.
pub fn load_rules<I: ImportInput>(input: I, current: &str) -> Result<Imported> {
    let (source, _) = input.read_import()?;
    load_rules_source(&source, current)
}

/// String-input variant that resolves relative file names against `directory`.
/// This is useful for a text field backed by the TUI working directory.
pub fn load_node_at(input: &str, directory: &Path, current: &str) -> Result<Imported> {
    let (source, _) = read_input(input, directory)?;
    load_node_source(&source, current)
}

/// String-input variant that resolves relative file names against `directory`.
pub fn load_rules_at(input: &str, directory: &Path, current: &str) -> Result<Imported> {
    let (source, _) = read_input(input, directory)?;
    load_rules_source(&source, current)
}

fn load_node_source(source: &str, current: &str) -> Result<Imported> {
    let source = source.trim_start_matches('\u{feff}');
    ensure!(!source.trim().is_empty(), t("No imported nodes"));
    if astra_convert::share::is_share(source) {
        return links(source, Kind::Proxy, current);
    }
    ensure!(source.len() <= LIMIT, t("Configuration exceeds 480 KiB"));
    let imported: toml_edit::DocumentMut = source.parse().context(t("Invalid configuration"))?;
    append_nodes(imported, current)
}

fn append_nodes(imported: toml_edit::DocumentMut, current: &str) -> Result<Imported> {
    let mut doc: toml_edit::DocumentMut = current.parse().context(t("Invalid configuration"))?;
    let incoming = imported
        .get("outbounds")
        .and_then(toml_edit::Item::as_array_of_tables)
        .context("No imported nodes")?;
    ensure!(!incoming.is_empty(), "No imported nodes");
    let groups = doc_group_names(current)?;
    let existing = doc
        .entry("outbounds")
        .or_insert(toml_edit::Item::ArrayOfTables(Default::default()))
        .as_array_of_tables_mut()
        .context("Invalid outbounds")?;
    let mut tags = existing
        .iter()
        .filter_map(|node| {
            node.get("tag")
                .and_then(toml_edit::Item::as_str)
                .map(str::to_owned)
        })
        .collect::<std::collections::HashSet<_>>();
    for node in incoming {
        let tag = node
            .get("tag")
            .and_then(toml_edit::Item::as_str)
            .context("Missing tag")?;
        ensure!(
            tags.insert(tag.to_owned()) && groups.iter().all(|name| name != tag),
            t("Proxy target name already exists")
        );
        existing.push(node.clone());
    }
    // Imported tables carry positions from another document. Reassign them so
    // each node's config/TLS/transport tables follow their own array entry.
    crate::edit::position_tables(doc.as_table_mut(), &mut 0);
    finish_document(doc)
}

fn load_rules_source(source: &str, current: &str) -> Result<Imported> {
    let source = source.trim_start_matches('\u{feff}');
    ensure!(!source.trim().is_empty(), t("No imported rules"));
    ensure!(source.len() <= LIMIT, t("Configuration exceeds 480 KiB"));
    let imported: toml_edit::DocumentMut = source.parse().context(t("Invalid configuration"))?;
    append_rules(imported, current)
}

fn append_rules(imported: toml_edit::DocumentMut, current: &str) -> Result<Imported> {
    let mut doc: toml_edit::DocumentMut = current.parse().context(t("Invalid configuration"))?;
    let incoming = imported
        .get("rules")
        .and_then(toml_edit::Item::as_array_of_tables);
    let incoming_sets = imported
        .get("rule_sets")
        .and_then(toml_edit::Item::as_table_like);
    ensure!(
        incoming.is_some_and(|rules| !rules.is_empty())
            || incoming_sets.is_some_and(|sets| sets.iter().next().is_some()),
        "No imported rules"
    );
    if let Some(incoming) = incoming {
        let existing = doc
            .entry("rules")
            .or_insert(toml_edit::Item::ArrayOfTables(Default::default()))
            .as_array_of_tables_mut()
            .context("Invalid rules")?;
        let identity = |rule: &toml_edit::Table| {
            ["kind", "value", "action"]
                .iter()
                .map(|key| rule.get(key).map(ToString::to_string).unwrap_or_default())
                .collect::<Vec<_>>()
                .join("\u{1f}")
        };
        let mut identities = existing
            .iter()
            .map(identity)
            .collect::<std::collections::HashSet<_>>();
        for rule in incoming {
            let key = identity(rule);
            ensure!(identities.insert(key), "Duplicate rule");
            existing.push(rule.clone());
        }
    }

    if let Some(incoming_sets) = incoming_sets {
        let sets = doc
            .entry("rule_sets")
            .or_insert(toml_edit::Item::Table(toml_edit::Table::new()))
            .as_table_like_mut()
            .context("Invalid rule_sets")?;
        for (name, incoming_set) in incoming_sets.iter() {
            if let Some(existing_set) = sets.get(name) {
                ensure!(
                    existing_set.to_string() == incoming_set.to_string(),
                    "Rule set name conflicts with existing definition"
                );
            } else {
                sets.insert(name, incoming_set.clone());
            }
        }
    }
    crate::edit::position_tables(doc.as_table_mut(), &mut 0);
    finish_document(doc)
}

fn finish_document(doc: toml_edit::DocumentMut) -> Result<Imported> {
    let text = doc.to_string();
    text.parse::<toml_edit::DocumentMut>()
        .context(t("Invalid configuration"))?;
    ensure!(text.len() <= LIMIT, t("Configuration exceeds 480 KiB"));
    Ok(Imported {
        text,
        warnings: vec![],
    })
}

pub fn merge_at(
    input: &str,
    directory: &Path,
    current: &str,
    kind: Kind,
    mode: MergeMode,
) -> Result<Imported> {
    ensure!(
        kind != Kind::Server,
        "Use full native TOML import for server configurations"
    );
    let (source, base) = read_input(input, directory)?;
    let source = source.trim_start_matches('\u{feff}');
    let mut incoming: toml_edit::DocumentMut = if astra_convert::share::is_share(source) {
        ensure!(
            kind == Kind::Proxy,
            t("Configuration does not match this core")
        );
        astra_convert::share::convert_links(source, &Default::default())?
            .proxy
            .parse()?
    } else if let Ok(doc) = source.parse() {
        doc
    } else {
        let converted = astra_convert::convert(
            source,
            &astra_convert::Options {
                base_dir: base.clone(),
                strict_rules: true,
                ..Default::default()
            },
        )
        .map_err(|_| anyhow::anyhow!(t("Invalid configuration")))?;
        if kind == Kind::Proxy {
            converted.proxy
        } else {
            converted.rules
        }
        .parse()?
    };
    let key = if kind == Kind::Proxy {
        "outbounds"
    } else {
        "rules"
    };
    normalize_collection(&mut incoming, key)?;
    if let Some(sets) = incoming
        .get_mut("rule_sets")
        .and_then(toml_edit::Item::as_table_like_mut)
    {
        for (_, set) in sets.iter_mut() {
            if let Some(path) = set.as_table_like_mut().and_then(|s| s.get_mut("path")) {
                rebase(path, &base);
            }
        }
    }
    let mut doc: toml_edit::DocumentMut = current.parse()?;
    normalize_collection(&mut doc, key)?;
    let mut position = None;
    match mode {
        MergeMode::Append => {}
        MergeMode::Replace => {
            doc.remove(key);
        }
        MergeMode::Overwrite(index) => {
            ensure!(
                incoming
                    .get(key)
                    .and_then(toml_edit::Item::as_array_of_tables)
                    .is_some_and(|a| a.len() == 1),
                t("Choose exactly one imported item")
            );
            let array = doc
                .get_mut(key)
                .and_then(toml_edit::Item::as_array_of_tables_mut)
                .context(t("Select an item first"))?;
            ensure!(index < array.len(), t("Select an item first"));
            if kind == Kind::Proxy {
                let tag = array
                    .get(index)
                    .and_then(|n| n.get("tag"))
                    .and_then(toml_edit::Item::as_str);
                let new_tag = incoming[key]
                    .as_array_of_tables()
                    .and_then(|a| a.get(0))
                    .and_then(|n| n.get("tag"))
                    .and_then(toml_edit::Item::as_str);
                ensure!(
                    tag == new_tag,
                    t("Keep the selected node tag when overwriting")
                );
            }
            array.remove(index);
            position = Some(index);
        }
    }
    let result = if kind == Kind::Proxy {
        append_nodes(incoming, &doc.to_string())?
    } else {
        append_rules(incoming, &doc.to_string())?
    };
    let mut doc: toml_edit::DocumentMut = result.text.parse()?;
    if let Some(index) = position {
        let array = doc[key].as_array_of_tables_mut().unwrap();
        let mut tables: Vec<_> = array.iter().cloned().collect();
        let table = tables.pop().unwrap();
        tables.insert(index, table);
        *array = tables.into_iter().collect();
    }
    if kind == Kind::Proxy {
        let value: toml::Table = toml::from_str(&doc.to_string())?;
        let tags: std::collections::HashSet<String> = value
            .get("outbounds")
            .and_then(toml::Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|n| n.get("tag").and_then(toml::Value::as_str))
            .map(str::to_owned)
            .collect();
        let groups: std::collections::BTreeMap<String, astra_model::groups::Group> = value
            .get("proxy_groups")
            .cloned()
            .map(toml::Value::try_into)
            .transpose()?
            .unwrap_or_default();
        astra_model::groups::validate(&groups, &tags).map_err(anyhow::Error::msg)?;
    } else if let Some(rules) = doc
        .get("rules")
        .and_then(toml_edit::Item::as_array_of_tables)
    {
        for rule in rules {
            if rule.get("kind").and_then(toml_edit::Item::as_str) == Some("rule_set") {
                let name = rule
                    .get("value")
                    .and_then(toml_edit::Item::as_str)
                    .context(t("Missing rule set"))?;
                ensure!(
                    doc.get("rule_sets")
                        .and_then(toml_edit::Item::as_table_like)
                        .is_some_and(|sets| sets.get(name).is_some()),
                    t("Missing rule set")
                );
            }
        }
    }
    crate::edit::position_tables(doc.as_table_mut(), &mut 0);
    finish_document(doc)
}

fn normalize_collection(doc: &mut toml_edit::DocumentMut, key: &str) -> Result<()> {
    if let Some(array) = doc.get(key).and_then(toml_edit::Item::as_array) {
        let mut tables = toml_edit::ArrayOfTables::new();
        for item in array {
            tables.push(
                item.as_inline_table()
                    .context(t("Invalid configuration"))?
                    .clone()
                    .into_table(),
            );
        }
        doc[key] = toml_edit::Item::ArrayOfTables(tables);
    }
    Ok(())
}

fn read_input(input: &str, directory: &Path) -> Result<(String, PathBuf)> {
    let trimmed = input.trim();
    let path = Path::new(trimmed);
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        directory.join(path)
    };
    if path.is_file() {
        return read_file(&path);
    }
    Ok((input.to_owned(), directory.to_path_buf()))
}

fn read_file(path: &Path) -> Result<(String, PathBuf)> {
    ensure!(path.is_file(), t("Choose one TOML or YAML file"));
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(LIMIT as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= LIMIT, t("Configuration exceeds 480 KiB"));
    let source = String::from_utf8(bytes).context(t("Configuration must be UTF-8"))?;
    Ok((
        source,
        path.parent().unwrap_or(Path::new(".")).to_path_buf(),
    ))
}
fn doc_group_names(current: &str) -> Result<Vec<String>> {
    let doc: toml::Value = toml::from_str(current)?;
    Ok(doc
        .get("proxy_groups")
        .and_then(toml::Value::as_table)
        .map(|v| v.keys().cloned().collect())
        .unwrap_or_default())
}

pub fn load(path: &Path, kind: Kind, current: &str) -> Result<Imported> {
    ensure!(path.is_file(), t("Choose one TOML or YAML file"));
    let mut bytes = Vec::new();
    std::fs::File::open(path)?
        .take(LIMIT as u64 + 1)
        .read_to_end(&mut bytes)?;
    ensure!(bytes.len() <= LIMIT, t("Configuration exceeds 480 KiB"));
    let source = String::from_utf8(bytes).context(t("Configuration must be UTF-8"))?;
    let source = source.trim_start_matches('\u{feff}');
    if astra_convert::share::is_share(source) {
        return links(source, kind, current);
    }
    let base = path.parent().context("Missing import directory")?;
    let yaml = path
        .extension()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.eq_ignore_ascii_case("yaml") || s.eq_ignore_ascii_case("yml"));
    ensure!(
        kind != Kind::Server || !yaml,
        "Server configurations require native TOML"
    );
    let (text, warnings) = if yaml {
        let current: toml::Table = toml::from_str(current)?;
        let socket_key = if kind == Kind::Proxy {
            "rule_socket"
        } else {
            "socket"
        };
        let converted = astra_convert::convert(
            source,
            &astra_convert::Options {
                base_dir: base.to_path_buf(),
                rule_socket: current
                    .get(socket_key)
                    .and_then(toml::Value::as_str)
                    .map(str::to_owned),
                strict_rules: true,
                ..Default::default()
            },
        )?;
        (
            if kind == Kind::Proxy {
                converted.proxy
            } else {
                converted.rules
            },
            converted.warnings,
        )
    } else {
        ensure!(
            path.extension()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.eq_ignore_ascii_case("toml")),
            t("Choose one TOML or YAML file")
        );
        (source.to_owned(), Vec::new())
    };
    let mut doc: toml_edit::DocumentMut = text.parse().context(t("Invalid configuration"))?;
    // Reject the opposite core's document before replacing the draft.
    let proxy = [
        "rule_socket",
        "standalone",
        "outbounds",
        "socks5_listen",
        "http_listen",
        "tun",
    ]
    .iter()
    .any(|key| doc.contains_key(key));
    let rule = [
        "socket",
        "rules",
        "default_action",
        "rule_sets",
        "geoip_database",
        "geosite_database",
    ]
    .iter()
    .any(|key| doc.contains_key(key));
    ensure!(
        match kind {
            Kind::Proxy => proxy && !rule && !doc.contains_key("listeners"),
            Kind::Rule => rule && !proxy && !doc.contains_key("listeners"),
            Kind::Server => doc.contains_key("listeners") && !proxy && !rule,
        },
        t("Configuration does not match this core")
    );
    if kind == Kind::Rule {
        for key in ["geoip_database", "geosite_database"] {
            if let Some(item) = doc.get_mut(key) {
                rebase(item, base);
            }
        }
        if let Some(sets) = doc
            .get_mut("rule_sets")
            .and_then(toml_edit::Item::as_table_like_mut)
        {
            for (_, set) in sets.iter_mut() {
                if let Some(item) = set.as_table_like_mut().and_then(|s| s.get_mut("path")) {
                    rebase(item, base);
                }
            }
        }
    }
    if kind == Kind::Server {
        normalize_collection(&mut doc, "listeners")?;
        if let Some(listeners) = doc
            .get_mut("listeners")
            .and_then(toml_edit::Item::as_array_of_tables_mut)
        {
            for listener in listeners.iter_mut() {
                if let Some(tls) = listener
                    .get_mut("tls")
                    .and_then(toml_edit::Item::as_table_like_mut)
                {
                    for key in ["certificate", "private_key"] {
                        if let Some(item) = tls.get_mut(key) {
                            rebase(item, base);
                        }
                    }
                }
            }
        }
    }
    let text = doc.to_string();
    ensure!(text.len() <= LIMIT, t("Configuration exceeds 480 KiB"));
    Ok(Imported { text, warnings })
}

fn rebase(item: &mut toml_edit::Item, base: &Path) {
    if let Some(value) = item.as_str().filter(|s| !s.is_empty()) {
        let path = Path::new(value);
        if path.is_relative() {
            *item = toml_edit::value(base.join(path).to_string_lossy().into_owned());
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    pub struct Fixture(pub PathBuf);
    impl Fixture {
        pub fn new() -> Self {
            static NEXT: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            let root = std::env::temp_dir().join(format!(
                "astra-import-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&root).unwrap();
            Self(root)
        }
        pub fn write(&self, name: &str, text: impl AsRef<[u8]>) -> PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, text).unwrap();
            path
        }
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn share_links_append_without_replacing_draft_or_local_settings() {
        let link = "vless://00000000-0000-4000-8000-000000000001@example.com:443?security=tls&sni=example.com&type=xhttp&mode=auto&path=%2Fhello#new-node";
        let current = "# keep comment\nstandalone=true\nstandalone_outbound='PROXY'\nsocks5_listen='127.0.0.1:1080'\n[[outbounds]]\nkind='socks5'\ntag='local'\nserver='localhost:1234'\n[subscriptions.local]\nurl='https://example.com/sub'\n";
        let imported = links(link, Kind::Proxy, current).unwrap();
        assert!(imported.text.contains("# keep comment"));
        let doc: toml::Value = toml::from_str(&imported.text).unwrap();
        assert_eq!(doc["outbounds"].as_array().unwrap().len(), 2);
        assert_eq!(
            doc["subscriptions"]["local"]["url"].as_str(),
            Some("https://example.com/sub")
        );
        assert!(links(link, Kind::Proxy, &imported.text).is_err());
        assert!(links(link, Kind::Rule, current).is_err());
    }
    #[test]
    fn fragment_helpers_append_nodes_rules_and_sets_with_conflict_checks() {
        let node = "[[outbounds]]\nkind='socks5'\ntag='new'\nserver='127.0.0.1:1'\n";
        let imported = load_node(
            node,
            "standalone=true\n[[outbounds]]\ntag='old'\nkind='direct'\n",
        )
        .unwrap();
        let value: toml::Value = toml::from_str(&imported.text).unwrap();
        assert_eq!(value["outbounds"].as_array().unwrap().len(), 2);
        assert!(load_node(node, &imported.text).is_err());

        let rules = "[[rules]]\nkind='domain_suffix'\nvalue='example.com'\naction='direct'\n[rule_sets.ads]\nbehavior='domain'\n";
        let imported = load_rules(rules, "socket='rules.sock'\n").unwrap();
        let value: toml::Value = toml::from_str(&imported.text).unwrap();
        assert_eq!(value["rules"].as_array().unwrap().len(), 1);
        assert_eq!(
            value["rule_sets"]["ads"]["behavior"].as_str(),
            Some("domain")
        );
        assert!(load_rules(rules, &imported.text).is_err());
        assert!(load_rules("[rule_sets.ads]\nbehavior='ip'\n[[rules]]\nkind='ip_cidr'\nvalue='127.0.0.1/32'\naction='direct'\n", &imported.text).is_err());
    }

    #[test]
    fn appended_native_nodes_keep_nested_tables_with_their_node() {
        let link = "vless://00000000-0000-4000-8000-000000000001@example.com:443?security=tls&sni=example.com&type=xhttp&mode=auto&path=%2Fhello#first";
        let current = "standalone=true\n[[outbounds]]\nkind='native'\ntag='existing'\n[outbounds.config]\nprotocol='vless'\nserver='old.example:443'\n[outbounds.config.tls]\nenabled=true\nserver_name='old.example'\n";
        let first = links(link, Kind::Proxy, current).unwrap();
        let second = links(&link.replace("#first", "#second"), Kind::Proxy, &first.text).unwrap();
        let doc: toml::Value = toml::from_str(&second.text).unwrap();
        let nodes = doc["outbounds"].as_array().unwrap();
        assert_eq!(nodes.len(), 3);
        assert_eq!(
            nodes[0]["config"]["server"].as_str(),
            Some("old.example:443")
        );
        for (node, tag) in nodes[1..].iter().zip(["first", "second"]) {
            assert_eq!(node["tag"].as_str(), Some(tag));
            assert_eq!(
                node["config"]["tls"]["server_name"].as_str(),
                Some("example.com")
            );
            assert_eq!(node["config"]["transport"]["path"].as_str(), Some("/hello"));
        }
    }
    #[test]
    fn dropped_paths_handle_spaces_unicode_quotes_and_powershell_prefix_without_shell() {
        let directory = std::env::current_dir().unwrap();
        for text in [
            "配置 file.toml",
            "\"配置 file.toml\"",
            "'配置 file.toml'",
            "& '配置 file.toml'",
        ] {
            assert_eq!(
                path(text, &directory).unwrap(),
                directory.join("配置 file.toml")
            );
        }
        for text in ["", "\"\"", "'a.toml'\n'b.toml'", "a\0.toml"] {
            assert!(path(text, &directory).is_err());
        }
        assert!(path("'", &directory).is_ok()); // A literal filename; never panic on one quote.
    }
    #[test]
    fn native_import_keeps_comments_and_rebases_both_rule_set_forms() {
        let fixture = Fixture::new();
        for sets in [
            "[rule_sets.local]\npath='lists/local.yaml'\n",
            "[rule_sets]\nlocal={path='lists/local.yaml'}\n",
        ] {
            let path = fixture.write("规则 TOML.toml", format!("\u{feff}# keep me\nsocket='rule.sock'\ngeoip_database='Country.mmdb'\ngeosite_database='geosite.dat'\n{sets}"));
            let imported = load(&path, Kind::Rule, "").unwrap();
            assert!(imported.text.contains("# keep me"));
            let doc: toml::Value = toml::from_str(&imported.text).unwrap();
            assert_eq!(
                doc["geoip_database"].as_str().unwrap(),
                fixture.0.join("Country.mmdb").to_str().unwrap()
            );
            assert_eq!(
                doc["rule_sets"]["local"]["path"].as_str().unwrap(),
                fixture.0.join("lists/local.yaml").to_str().unwrap()
            );
            assert_eq!(doc["socket"].as_str(), Some("rule.sock"));
            assert!(imported.warnings.is_empty());
        }
    }
    #[test]
    fn mihomo_import_selects_core_preserves_endpoint_and_stays_offline() {
        let fixture = Fixture::new();
        let path = fixture.write("配置.yml", "socks-port: 1088\nproxies:\n  - {name: edge, type: socks5, server: 127.0.0.1, port: 1080}\nrule-providers:\n  local: {type: file, behavior: domain, path: './local.yaml'}\nrules:\n  - RULE-SET,local,edge\n  - GEOSITE,cn,DIRECT\n  - MATCH,edge\n");
        let proxy = load(&path, Kind::Proxy, "rule_socket='bound.rules'").unwrap();
        let proxy: toml::Value = toml::from_str(&proxy.text).unwrap();
        assert_eq!(proxy["rule_socket"].as_str(), Some("bound.rules"));
        assert_eq!(proxy["outbounds"][0]["tag"].as_str(), Some("edge"));
        assert!(proxy.get("rules").is_none());
        let rules = load(&path, Kind::Rule, "socket='bound.rules'").unwrap();
        assert!(!rules.warnings.is_empty());
        let rules: toml::Value = toml::from_str(&rules.text).unwrap();
        assert_eq!(rules["socket"].as_str(), Some("bound.rules"));
        assert_eq!(rules["rules"][1]["kind"].as_str(), Some("geosite"));
        assert_eq!(
            rules["rule_sets"]["local"]["path"].as_str().unwrap(),
            fixture.0.join("./local.yaml").to_str().unwrap()
        );
        assert!(!fixture.0.join("geosite").exists());
    }
    #[test]
    fn invalid_wrong_core_large_and_unsupported_inputs_are_rejected() {
        let fixture = Fixture::new();
        for (name, text, kind) in [
            ("wrong.toml", "socket='r'", Kind::Proxy),
            ("wrong2.toml", "standalone=true", Kind::Rule),
            ("bad.toml", "socket=[", Kind::Rule),
            ("empty.yaml", "", Kind::Rule),
            ("bad.yml", "rules: ['UNSUPPORTED,test,DIRECT']", Kind::Rule),
            ("data.txt", "socket='r'", Kind::Rule),
        ] {
            assert!(
                load(&fixture.write(name, text), kind, "socket='bound'").is_err(),
                "{name}"
            );
        }
        assert!(load(
            &fixture.write("large.toml", vec![b'a'; LIMIT + 1]),
            Kind::Rule,
            ""
        )
        .is_err());
        assert!(load(&fixture.write("binary.toml", [0xff, 0xfe]), Kind::Rule, "").is_err());
    }
}
