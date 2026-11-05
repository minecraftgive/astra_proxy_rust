//! Pure, non-executing export helpers for native TOML configuration.
//!
//! The functions in this module only parse and format text. In particular,
//! shell snippets are returned as data and are never passed to a shell.

use crate::shell::{fish_quote, shell_quote};
use anyhow::{bail, ensure, Context, Result};
use astra_platform::platform::OpenOptionsExt;
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
#[cfg(test)]
pub fn selected_node(text: &str, tag: &str) -> Result<String> {
    selected_nodes(text, &[tag.to_owned()])
}

/// Extract selected outbounds in source order, preserving all native fields.
/// Repeated selections are deduplicated; missing or ambiguous tags fail the
/// entire export rather than silently omitting or choosing a node.
pub fn selected_nodes(text: &str, tags: &[String]) -> Result<String> {
    let nodes = matching_nodes(text, tags)?;
    let mut table = toml::Table::new();
    table.insert("outbounds".into(), toml::Value::Array(nodes));
    serialize_fragment(table)
}

/// Export each selected node as one share URI, in source order. No output is
/// returned unless every selected node is representable and fits the limit.
pub fn selected_nodes_share(text: &str, tags: &[String]) -> Result<String> {
    let nodes = matching_nodes(text, tags)?;
    let mut output = String::new();
    for node in nodes {
        let tag = node["tag"].as_str().unwrap();
        let link = astra_convert::share::export_node(&node)
            .with_context(|| format!("Export share link for outbound {tag:?}"))?;
        ensure!(
            output.len() + link.len() < MAX_EXPORT_BYTES,
            "Share link export exceeds 480 KiB"
        );
        output.push_str(&link);
        output.push('\n');
    }
    Ok(output)
}

fn matching_nodes(text: &str, tags: &[String]) -> Result<Vec<toml::Value>> {
    ensure!(!tags.is_empty(), "Select at least one outbound node");
    for tag in tags {
        ensure_name(tag, "node tag")?;
    }
    let requested: BTreeSet<_> = tags.iter().map(String::as_str).collect();
    let value = parse(text)?;
    let nodes = value
        .get("outbounds")
        .and_then(toml::Value::as_array)
        .context("Configuration has no outbounds")?;
    let mut matched = BTreeSet::new();
    let mut selected = Vec::new();
    for node in nodes {
        if let Some(tag) = node.get("tag").and_then(toml::Value::as_str) {
            if requested.contains(tag) {
                ensure!(matched.insert(tag), "Outbound node {tag:?} is ambiguous");
                selected.push(node.clone());
            }
        }
    }
    if let Some(tag) = requested.difference(&matched).next() {
        bail!("Outbound node {tag:?} was not found");
    }
    Ok(selected)
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
#[cfg(test)]
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

/// Return the names of rule sets directly referenced by one rule.
#[cfg(test)]
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

/// Select the snippet matching the current platform and user's shell.
pub fn current_shell_commands(text: &str) -> Result<String> {
    let commands = shell_proxy_commands(text)?;
    #[cfg(windows)]
    {
        let shell = std::env::var_os("ComSpec")
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_default();
        // PowerShell normally inherits ComSpec=cmd.exe. Prefer the nearest
        // actual shell ancestor, including launches through cargo/rustup.
        Ok(
            match windows_shell(std::process::id(), &windows_processes(), &shell) {
                WindowsShell::PowerShell => commands.powershell,
                WindowsShell::Cmd => commands.cmd,
            },
        )
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

#[cfg(any(windows, test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WindowsShell {
    PowerShell,
    Cmd,
}

#[cfg(any(windows, test))]
fn windows_shell(mut pid: u32, processes: &[(u32, u32, String)], fallback: &str) -> WindowsShell {
    let identify = |name: &str| match name
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(name)
        .to_ascii_lowercase()
        .as_str()
    {
        "powershell.exe" | "pwsh.exe" => Some(WindowsShell::PowerShell),
        "cmd.exe" => Some(WindowsShell::Cmd),
        _ => None,
    };
    // Bound traversal even when a parent has exited or a PID was recycled.
    for _ in 0..32 {
        let Some((_, parent, _)) = processes.iter().find(|(id, _, _)| *id == pid) else {
            break;
        };
        let Some((_, _, name)) = processes.iter().find(|(id, _, _)| id == parent) else {
            break;
        };
        if let Some(shell) = identify(name) {
            return shell;
        }
        pid = *parent;
    }
    identify(fallback).unwrap_or(WindowsShell::Cmd)
}

#[cfg(windows)]
fn windows_processes() -> Vec<(u32, u32, String)> {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, INVALID_HANDLE_VALUE},
        System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
            TH32CS_SNAPPROCESS,
        },
    };
    let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snapshot == INVALID_HANDLE_VALUE {
        return Vec::new();
    }
    let mut processes = Vec::new();
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut available = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
    while available {
        let length = entry
            .szExeFile
            .iter()
            .position(|ch| *ch == 0)
            .unwrap_or(entry.szExeFile.len());
        processes.push((
            entry.th32ProcessID,
            entry.th32ParentProcessID,
            String::from_utf16_lossy(&entry.szExeFile[..length]),
        ));
        available = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
    }
    unsafe {
        CloseHandle(snapshot);
    }
    processes
}

/// Write export data without overwriting an existing destination.
///
/// Missing parent directories are created after validating the output size.
/// The complete export is first written and synced to a private, same-directory
/// temporary file. A hard link then commits it without replacing a destination
/// created by another writer, so partial output is never exposed.
pub fn write_new(path: &Path, text: &str) -> Result<()> {
    ensure_size(text)?;
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = path
        .file_name()
        .context("Export path has no file name")?
        .to_string_lossy();
    std::fs::create_dir_all(parent)
        .with_context(|| format!("Create export directory {}", parent.display()))?;

    let (temporary, mut file) = loop {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        let temporary = parent.join(format!(
            ".{name}.astra-export-{}-{id}.tmp",
            std::process::id()
        ));
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)
        {
            Ok(file) => break (temporary, file),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("Create export temporary file in {}", parent.display())
                })
            }
        }
    };

    let result = (|| {
        file.write_all(text.as_bytes())
            .with_context(|| format!("Write export {}", path.display()))?;
        file.sync_all()
            .with_context(|| format!("Sync export {}", path.display()))?;
        drop(file);
        std::fs::hard_link(&temporary, path).with_context(|| {
            format!(
                "Commit export {} without overwriting an existing file",
                path.display()
            )
        })?;
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

fn powershell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

#[cfg(test)]
#[path = "test/export_test.rs"]
mod tests;
