//! Pure, non-executing export helpers for native TOML configuration.
//!
//! The functions in this module only parse and format text. In particular,
//! shell snippets are returned as data and are never passed to a shell.

use anyhow::{bail, ensure, Context, Result};
use astra_ipc::platform::OpenOptionsExt;
use std::{
    collections::BTreeSet,
    fs::OpenOptions,
    io::Write,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

/// Maximum size accepted by export helpers, matching the import limit.
pub const MAX_EXPORT_BYTES: usize = 480 * 1024;

/// The four shell spellings emitted by [`shell_proxy_commands`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellProxyCommands {
    /// Bash, zsh, and other POSIX-compatible shells.
    pub posix: String,
    /// Fish shell.
    pub fish: String,
    /// PowerShell.
    pub powershell: String,
    /// Windows Command Prompt.
    pub cmd: String,
}

/// A convenient alias for callers that prefer the shorter name.
pub type ProxyCommands = ShellProxyCommands;

/// Validate and return a complete native TOML configuration unchanged.
///
/// Keeping the source text means comments and formatting survive a full
/// export. The returned text is independent of the input and is bounded to
/// the same 480 KiB limit used by imports.
pub fn full_config(text: &str) -> Result<String> {
    let source = without_bom(text);
    ensure_size(source)?;
    let _: toml_edit::DocumentMut = source.parse().context("Invalid TOML configuration")?;
    Ok(source.to_owned())
}

/// Alias for [`full_config`] suitable for UI action handlers.
pub fn export_config(text: &str) -> Result<String> {
    full_config(text)
}

/// Extract one outbound by its native `tag`.
///
/// The result is a valid TOML fragment containing exactly one `[[outbounds]]`
/// entry. Credentials are retained because this is a configuration export;
/// shell exports below intentionally do not use outbound fields.
pub fn selected_node(text: &str, tag: &str) -> Result<String> {
    ensure_name(tag, "node tag")?;
    let value = parse(text)?;
    let nodes = value
        .get("outbounds")
        .and_then(toml::Value::as_array)
        .context("Configuration has no outbounds")?;
    let node = nodes
        .iter()
        .find(|node| node.get("tag").and_then(toml::Value::as_str) == Some(tag))
        .with_context(|| format!("Outbound node {tag:?} was not found"))?;

    let mut table = toml::Table::new();
    table.insert("outbounds".into(), toml::Value::Array(vec![node.clone()]));
    serialize_fragment(table)
}

/// UI-friendly alias for [`selected_node`].
pub fn extract_node(text: &str, tag: &str) -> Result<String> {
    selected_node(text, tag)
}

/// Extract one rule by its zero-based position in `[[rules]]`.
///
/// Direct `rule_set` references bring along their matching definitions under
/// `[rule_sets.<name>]`. Native rule sets cannot nest, so direct dependencies
/// are the complete dependency closure for the supported configuration model.
pub fn selected_rule(text: &str, index: usize) -> Result<String> {
    let value = parse(text)?;
    let rules = value
        .get("rules")
        .and_then(toml::Value::as_array)
        .context("Configuration has no rules")?;
    let rule = rules
        .get(index)
        .with_context(|| format!("Rule index {index} was not found"))?;

    let mut table = toml::Table::new();
    table.insert("rules".into(), toml::Value::Array(vec![rule.clone()]));
    let dependencies = rule_set_dependencies_from_rule(rule);
    if !dependencies.is_empty() {
        if let Some(definitions) = value.get("rule_sets").and_then(toml::Value::as_table) {
            let mut selected = toml::Table::new();
            for name in dependencies {
                if let Some(definition) = definitions.get(&name) {
                    selected.insert(name, definition.clone());
                }
            }
            if !selected.is_empty() {
                table.insert("rule_sets".into(), toml::Value::Table(selected));
            }
        }
    }
    serialize_fragment(table)
}

/// Extract one rule by its `value` field, requiring that it be unambiguous.
pub fn selected_rule_by_value(text: &str, value: &str) -> Result<String> {
    ensure_name(value, "rule value")?;
    let document = parse(text)?;
    let rules = document
        .get("rules")
        .and_then(toml::Value::as_array)
        .context("Configuration has no rules")?;
    let matches: Vec<_> = rules
        .iter()
        .filter(|rule| rule.get("value").and_then(toml::Value::as_str) == Some(value))
        .collect();
    match matches.as_slice() {
        [rule] => {
            let mut table = toml::Table::new();
            table.insert("rules".into(), toml::Value::Array(vec![(*rule).clone()]));
            let dependencies = rule_set_dependencies_from_rule(rule);
            if !dependencies.is_empty() {
                if let Some(definitions) = document.get("rule_sets").and_then(toml::Value::as_table)
                {
                    let mut selected = toml::Table::new();
                    for name in dependencies {
                        if let Some(definition) = definitions.get(&name) {
                            selected.insert(name, definition.clone());
                        }
                    }
                    if !selected.is_empty() {
                        table.insert("rule_sets".into(), toml::Value::Table(selected));
                    }
                }
            }
            serialize_fragment(table)
        }
        [] => bail!("Rule value {value:?} was not found"),
        _ => bail!("Rule value {value:?} is ambiguous"),
    }
}

/// UI-friendly alias for [`selected_rule`].
pub fn extract_rule(text: &str, index: usize) -> Result<String> {
    selected_rule(text, index)
}

/// Return the names of rule sets directly referenced by one rule.
pub fn rule_set_dependencies(text: &str, index: usize) -> Result<Vec<String>> {
    let value = parse(text)?;
    let rules = value
        .get("rules")
        .and_then(toml::Value::as_array)
        .context("Configuration has no rules")?;
    let rule = rules
        .get(index)
        .with_context(|| format!("Rule index {index} was not found"))?;
    Ok(rule_set_dependencies_from_rule(rule).into_iter().collect())
}

/// Return inert proxy environment assignments derived only from listeners.
///
/// Outbound credentials, subscription URLs, and all other config values are
/// deliberately ignored. This function never spawns a process or evaluates
/// the generated text.
pub fn shell_proxy_commands(text: &str) -> Result<ShellProxyCommands> {
    let value = parse(text)?;
    let mixed = listener(value.get("mixed_listen"))?;
    let http = if mixed.is_some() {
        mixed.clone()
    } else {
        listener(value.get("http_listen"))?
    };
    let socks = if mixed.is_some() {
        mixed
    } else {
        listener(value.get("socks5_listen"))?
    };
    ensure!(
        http.is_some() || socks.is_some(),
        "Configuration has no proxy listener"
    );

    let http_url = http.as_deref().map(|address| format!("http://{address}"));
    let socks_url = socks
        .as_deref()
        .map(|address| format!("socks5h://{address}"));
    let mut assignments = Vec::new();
    if let Some(url) = http_url.as_deref() {
        assignments.push(("HTTP_PROXY", url));
        assignments.push(("HTTPS_PROXY", url));
    }
    if let Some(url) = socks_url.as_deref() {
        assignments.push(("ALL_PROXY", url));
    }

    let posix = assignments
        .iter()
        .map(|(name, value)| format!("export {name}={}", shell_quote(value)))
        .chain(assignments.iter().map(|(name, value)| {
            let lower = name.to_ascii_lowercase();
            format!("export {lower}={}", shell_quote(value))
        }))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let fish = assignments
        .iter()
        .map(|(name, value)| format!("set -gx {name} {}", fish_quote(value)))
        .chain(assignments.iter().map(|(name, value)| {
            let lower = name.to_ascii_lowercase();
            format!("set -gx {lower} {}", fish_quote(value))
        }))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let powershell = assignments
        .iter()
        .map(|(name, value)| format!("$env:{name} = {}", powershell_quote(value)))
        .chain(assignments.iter().map(|(name, value)| {
            let lower = name.to_ascii_lowercase();
            format!("$env:{lower} = {}", powershell_quote(value))
        }))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let cmd = assignments
        .iter()
        .map(|(name, value)| format!("set \"{name}={value}\""))
        .chain(assignments.iter().map(|(name, value)| {
            let lower = name.to_ascii_lowercase();
            format!("set \"{lower}={value}\"")
        }))
        .collect::<Vec<_>>()
        .join("\r\n")
        + "\r\n";

    Ok(ShellProxyCommands {
        posix,
        fish,
        powershell,
        cmd,
    })
}

/// Alias for [`shell_proxy_commands`].
pub fn proxy_commands(text: &str) -> Result<ShellProxyCommands> {
    shell_proxy_commands(text)
}

/// Select the snippet matching the current platform and user's shell.
pub fn current_shell_commands(text: &str) -> Result<String> {
    let commands = shell_proxy_commands(text)?;
    #[cfg(windows)]
    {
        let shell = std::env::var_os("ComSpec")
            .and_then(|value| value.to_str().map(str::to_ascii_lowercase))
            .unwrap_or_default();
        return Ok(if shell.contains("powershell") || shell.contains("pwsh") {
            commands.powershell
        } else {
            commands.cmd
        });
    }
    #[cfg(not(windows))]
    {
        let shell = std::env::var_os("SHELL")
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_default();
        Ok(if shell.ends_with("/fish") {
            commands.fish
        } else {
            commands.posix
        })
    }
}

/// Write export data without overwriting an existing destination.
///
/// The complete export is first written and synced to a private, same-directory
/// temporary file. A hard link then commits it without replacing a destination
/// created by another writer, so partial output is never exposed.
pub fn write_new(path: &Path, text: &str) -> Result<()> {
    ensure_size(text)?;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .context("Export path has no file name")?
        .to_string_lossy();

    let (temporary, mut file) = loop {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(".{name}.astra-export-{}-{id}.tmp", std::process::id()));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
        {
            Ok(file) => break (temporary, file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("Create export temporary file in {}", parent.display()))
            }
        }
    };

    let result = (|| {
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::hard_link(&temporary, path)
            .with_context(|| format!("Commit export {}", path.display()))?;
        Ok(())
    })();
    let _ = std::fs::remove_file(&temporary);
    result
}

fn parse(text: &str) -> Result<toml::Value> {
    let source = without_bom(text);
    ensure_size(source)?;
    toml::from_str(source).context("Invalid TOML configuration")
}

fn serialize_fragment(table: toml::Table) -> Result<String> {
    let output = toml::to_string_pretty(&toml::Value::Table(table))?;
    ensure_size(&output)?;
    Ok(output)
}

fn without_bom(text: &str) -> &str {
    text.strip_prefix('\u{feff}').unwrap_or(text)
}

fn ensure_size(text: &str) -> Result<()> {
    ensure!(
        text.len() <= MAX_EXPORT_BYTES,
        "Configuration exceeds 480 KiB"
    );
    Ok(())
}

fn ensure_name(name: &str, label: &str) -> Result<()> {
    ensure!(
        !name.is_empty() && !name.chars().any(char::is_control),
        "Invalid {label}"
    );
    Ok(())
}

fn rule_set_dependencies_from_rule(rule: &toml::Value) -> BTreeSet<String> {
    let is_rule_set = rule
        .get("kind")
        .and_then(toml::Value::as_str)
        .is_some_and(|kind| {
            kind.eq_ignore_ascii_case("rule_set") || kind.eq_ignore_ascii_case("rule-set")
        });
    if !is_rule_set {
        return BTreeSet::new();
    }
    rule.get("value")
        .and_then(toml::Value::as_str)
        .filter(|name| !name.is_empty())
        .map(|name| [name.to_owned()].into_iter().collect())
        .unwrap_or_default()
}

fn listener(item: Option<&toml::Value>) -> Result<Option<String>> {
    let Some(item) = item else { return Ok(None) };
    let Some(value) = item.as_str() else {
        bail!("Proxy listener must be a string")
    };
    if value.is_empty() {
        return Ok(None);
    }
    Ok(Some(normalize_listener(value)?))
}

fn normalize_listener(value: &str) -> Result<String> {
    ensure!(
        !value.chars().any(|c| c.is_control() || c.is_whitespace()),
        "Invalid proxy listener"
    );
    let (host, port) = if let Some(rest) = value.strip_prefix('[') {
        let end = rest.find(']').context("Invalid IPv6 proxy listener")?;
        let host = &rest[..end];
        let port = rest[end + 1..]
            .strip_prefix(':')
            .context("Invalid proxy listener")?;
        (host, port)
    } else {
        value
            .rsplit_once(':')
            .context("Proxy listener must include a port")?
    };
    ensure!(
        !host.is_empty() && port.parse::<u16>().is_ok(),
        "Invalid proxy listener"
    );
    ensure!(
        host.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | ':' | '-' | '_')),
        "Invalid proxy listener"
    );
    let host = match host {
        "0.0.0.0" | "::" => "127.0.0.1",
        other => other,
    };
    if host.contains(':') {
        Ok(format!("[{host}]:{port}"))
    } else {
        Ok(format!("{host}:{port}"))
    }
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\\''"))
}

fn fish_quote(value: &str) -> String {
    shell_quote(value)
}

fn powershell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"mixed_listen = "127.0.0.1:7890"

[[outbounds]]
kind = "vless_tls"
tag = "secret-node"
password = "must-not-be-in-shell"

[[outbounds]]
kind = "socks5"
tag = "other"
server = "127.0.0.1:1"

[[rules]]
kind = "rule_set"
value = "ads"
action = "block"

[[rules]]
kind = "domain_suffix"
value = "example.com"
action = "direct"

[rule_sets.ads]
behavior = "domain"
format = "yaml"
payload = ["+.ads.example"]
"#;

    #[test]
    fn full_export_preserves_native_text() {
        let source = "# comment\nstandalone = true\n";
        assert_eq!(full_config(source).unwrap(), source);
        assert!(full_config(&"x".repeat(MAX_EXPORT_BYTES + 1)).is_err());
    }

    #[test]
    fn selected_node_contains_only_requested_node() {
        let output = selected_node(CONFIG, "secret-node").unwrap();
        let value: toml::Value = toml::from_str(&output).unwrap();
        assert_eq!(value["outbounds"].as_array().unwrap().len(), 1);
        assert_eq!(value["outbounds"][0]["tag"].as_str(), Some("secret-node"));
        assert!(!output.contains("other"));
    }

    #[test]
    fn selected_rule_carries_rule_set_definition() {
        let output = selected_rule(CONFIG, 0).unwrap();
        let value: toml::Value = toml::from_str(&output).unwrap();
        assert_eq!(value["rules"].as_array().unwrap().len(), 1);
        assert_eq!(
            value["rule_sets"]["ads"]["behavior"].as_str(),
            Some("domain")
        );
        assert_eq!(rule_set_dependencies(CONFIG, 0).unwrap(), vec!["ads"]);
        assert!(!selected_rule(CONFIG, 1).unwrap().contains("rule_sets"));
    }

    #[test]
    fn rule_selection_by_value_rejects_ambiguous_values() {
        assert!(selected_rule_by_value(CONFIG, "missing").is_err());
        assert!(selected_rule_by_value(CONFIG, "example.com")
            .unwrap()
            .contains("domain_suffix"));
    }

    #[test]
    fn shell_commands_are_inert_and_do_not_include_outbound_secrets() {
        let commands = shell_proxy_commands(CONFIG).unwrap();
        assert!(commands
            .posix
            .contains("export HTTP_PROXY='http://127.0.0.1:7890'"));
        assert!(commands
            .fish
            .contains("set -gx ALL_PROXY 'socks5h://127.0.0.1:7890'"));
        assert!(commands
            .powershell
            .contains("$env:HTTPS_PROXY = 'http://127.0.0.1:7890'"));
        assert!(commands
            .cmd
            .contains("set \"HTTP_PROXY=http://127.0.0.1:7890\""));
        assert!(!commands.posix.contains("must-not-be-in-shell"));
    }

    #[test]
    fn separate_listeners_generate_expected_proxy_kinds() {
        let commands =
            shell_proxy_commands("http_listen='127.0.0.1:8080'\nsocks5_listen='[::1]:1080'")
                .unwrap();
        assert!(commands.posix.contains("http://127.0.0.1:8080"));
        assert!(commands.posix.contains("socks5h://[::1]:1080"));
    }

    #[test]
    fn write_new_commits_complete_content_and_never_overwrites() {
        let path = std::env::temp_dir().join(format!(
            "astra-tui-export-{}-{}",
            std::process::id(),
            NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_file(&path);
        write_new(&path, "standalone = true\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "standalone = true\n");
        assert!(write_new(&path, "overwritten = true\n").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "standalone = true\n");
        let temporary_prefix = format!(".{}-{}.astra-export-", path.file_name().unwrap().to_string_lossy(), std::process::id());
        assert!(!std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .filter_map(Result::ok)
            .any(|entry| entry.file_name().to_string_lossy().starts_with(&temporary_prefix)));
        std::fs::remove_file(path).unwrap();
    }
}
