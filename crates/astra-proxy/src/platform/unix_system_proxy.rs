//! OS settings are snapshotted before any write and restored on normal shutdown.
use crate::config::ProxyConfig;
use anyhow::{bail, ensure, Context, Result};
use astra_platform::platform::OpenOptionsExt;
use serde::{Deserialize, Serialize};
use std::{
    io::{Read, Seek, Write},
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Change {
    apply: Vec<String>,
    restore: Vec<String>,
}
#[derive(Default)]
pub struct Guard {
    changes: Vec<Change>,
    journal: Option<PathBuf>,
    checks: Vec<Check>,
    lease: Option<super::network_recovery::Lease>,
    lock: Option<std::fs::File>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct Snapshot {
    changes: Vec<Change>,
    #[serde(default)]
    checks: Vec<Check>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Check {
    query: Vec<String>,
    before: String,
    expected: String,
}
fn checks(changes: &[Change]) -> Result<Vec<Check>> {
    checks_with(changes, command)
}
fn checks_with(
    changes: &[Change],
    mut read: impl FnMut(&[String]) -> Result<String>,
) -> Result<Vec<Check>> {
    let mut checks: Vec<Check> = vec![];
    for change in changes {
        let a = &change.apply;
        let query = if a.first().is_some_and(|v| v == "gsettings") {
            vec![a[0].clone(), "get".into(), a[2].clone(), a[3].clone()]
        } else {
            let read = match a.get(1).map(String::as_str) {
                Some("-setwebproxy" | "-setwebproxystate") => "-getwebproxy",
                Some("-setsecurewebproxy" | "-setsecurewebproxystate") => "-getsecurewebproxy",
                Some("-setsocksfirewallproxy" | "-setsocksfirewallproxystate") => {
                    "-getsocksfirewallproxy"
                }
                Some("-setautoproxystate") => "-getautoproxyurl",
                Some("-setproxyautodiscovery") => "-getproxyautodiscovery",
                _ => bail!("unknown system proxy operation"),
            };
            vec!["/usr/sbin/networksetup".into(), read.into(), a[2].clone()]
        };
        let index = match checks.iter().position(|c| c.query == query) {
            Some(index) => index,
            None => {
                let before = read(&query)?;
                checks.push(Check {
                    query,
                    expected: before.clone(),
                    before,
                });
                checks.len() - 1
            }
        };
        let check = &mut checks[index];
        if a[0] == "gsettings" {
            check.expected = a[4].clone();
            continue;
        }
        let mut values = vec![];
        match a[1].as_str() {
            "-setwebproxy" | "-setsecurewebproxy" | "-setsocksfirewallproxy" => {
                values.push(("Enabled:", "Yes".to_owned()));
                values.push(("Server:", a[3].clone()));
                values.push(("Port:", a[4].clone()));
            }
            "-setproxyautodiscovery" => values.push((
                "Auto Proxy Discovery:",
                if a[3] == "on" { "On" } else { "Off" }.to_owned(),
            )),
            _ => values.push((
                "Enabled:",
                if a[3] == "on" { "Yes" } else { "No" }.to_owned(),
            )),
        }
        for (key, value) in values {
            ensure!(
                check.expected.lines().any(|line| line.starts_with(key)),
                "unexpected macOS proxy query format"
            );
            check.expected = check
                .expected
                .lines()
                .map(|line| {
                    if line.starts_with(key) {
                        format!("{key} {value}")
                    } else {
                        line.to_owned()
                    }
                })
                .collect::<Vec<_>>()
                .join("\n");
        }
    }
    Ok(checks)
}
fn validate_check(check: &Check) -> Result<()> {
    let q = &check.query;
    let mac = q.len() == 3
        && q[0] == "/usr/sbin/networksetup"
        && matches!(
            q[1].as_str(),
            "-getwebproxy"
                | "-getsecurewebproxy"
                | "-getsocksfirewallproxy"
                | "-getautoproxyurl"
                | "-getproxyautodiscovery"
        );
    let gnome = q.len() == 4
        && q[0] == "gsettings"
        && q[1] == "get"
        && matches!(
            q[2].as_str(),
            "org.gnome.system.proxy"
                | "org.gnome.system.proxy.http"
                | "org.gnome.system.proxy.https"
                | "org.gnome.system.proxy.socks"
                | "org.gnome.system.proxy.ftp"
        )
        && matches!(q[3].as_str(), "mode" | "use-same-proxy" | "host" | "port");
    ensure!(mac || gnome, "unsupported proxy ownership query");
    Ok(())
}
fn command(args: &[String]) -> Result<String> {
    let (program, args) = args.split_first().context("empty OS command")?;
    let output = astra_platform::command::output(Command::new(program).args(args))
        .with_context(|| format!("Run {program}"))?;
    let stdout = String::from_utf8(output.stdout)?;
    ensure!(
        output.status.success()
            && !stdout
                .lines()
                .any(|line| line.trim_start().starts_with("** Error:")
                    || line.trim_start().starts_with("Error:")),
        "{program} failed: {}{}",
        String::from_utf8_lossy(&output.stderr),
        stdout
    );
    Ok(stdout.trim().to_owned())
}
fn args(items: &[&str]) -> Vec<String> {
    items.iter().map(|v| (*v).to_owned()).collect()
}
fn plan(
    config: &ProxyConfig,
    mut query: impl FnMut(&[String]) -> Result<String>,
) -> Result<Vec<Change>> {
    let Some(settings) = &config.system_proxy else {
        return Ok(vec![]);
    };
    super::mode_config::system_proxy_address(config)?;
    let (http, socks) = super::mode_config::system_proxy_addresses(config)?;
    let mut changes = vec![];
    if cfg!(target_os = "macos") {
        let services = if settings.services.is_empty() {
            query(&args(&[
                "/usr/sbin/networksetup",
                "-listallnetworkservices",
            ]))?
            .lines()
            .skip(1)
            .filter(|s| !s.starts_with('*'))
            .map(str::to_owned)
            .collect()
        } else {
            settings.services.clone()
        };
        ensure!(!services.is_empty(), "no macOS network services found");
        for service in services {
            for (read, write) in [
                ("-getautoproxyurl", "-setautoproxystate"),
                ("-getproxyautodiscovery", "-setproxyautodiscovery"),
            ] {
                let before = query(&args(&["/usr/sbin/networksetup", read, &service]))?;
                let enabled = before.lines().any(|line| {
                    line.trim() == "Enabled: Yes" || line.trim() == "Auto Proxy Discovery: On"
                });
                changes.push(Change {
                    apply: args(&["/usr/sbin/networksetup", write, &service, "off"]),
                    restore: args(&[
                        "/usr/sbin/networksetup",
                        write,
                        &service,
                        if enabled { "on" } else { "off" },
                    ]),
                });
            }
            for (read, set, state, address) in [
                ("-getwebproxy", "-setwebproxy", "-setwebproxystate", http),
                (
                    "-getsecurewebproxy",
                    "-setsecurewebproxy",
                    "-setsecurewebproxystate",
                    http,
                ),
                (
                    "-getsocksfirewallproxy",
                    "-setsocksfirewallproxy",
                    "-setsocksfirewallproxystate",
                    socks,
                ),
            ] {
                let old = query(&args(&["/usr/sbin/networksetup", read, &service]))?;
                let get = |key: &str| {
                    old.lines()
                        .find_map(|line| line.strip_prefix(key))
                        .map(str::trim)
                };
                ensure!(
                    get("Authenticated Proxy Enabled:") != Some("1"),
                    "cannot restore authenticated proxy on {service}; disable it first"
                );
                let host = get("Server:").context("missing proxy Server")?;
                let port = get("Port:").context("missing proxy Port")?;
                let enabled = get("Enabled:") == Some("Yes");
                let Some(address) = address else {
                    changes.push(Change {
                        apply: args(&["/usr/sbin/networksetup", state, &service, "off"]),
                        restore: args(&[
                            "/usr/sbin/networksetup",
                            state,
                            &service,
                            if enabled { "on" } else { "off" },
                        ]),
                    });
                    continue;
                };
                // networksetup enables the proxy while setting host/port. Undo order
                // restores the endpoint first, then its original on/off state.
                changes.push(Change {
                    apply: args(&[
                        "/usr/sbin/networksetup",
                        set,
                        &service,
                        &address.ip().to_string(),
                        &address.port().to_string(),
                    ]),
                    restore: args(&[
                        "/usr/sbin/networksetup",
                        state,
                        &service,
                        if enabled { "on" } else { "off" },
                    ]),
                });
                changes.push(Change {
                    apply: args(&["/usr/sbin/networksetup", state, &service, "on"]),
                    restore: args(&[
                        "/usr/sbin/networksetup",
                        set,
                        &service,
                        if host.is_empty() { "" } else { host },
                        port,
                    ]),
                });
            }
        }
    } else if cfg!(target_os = "linux") {
        ensure!(
            settings.services.is_empty(),
            "system_proxy.services is macOS-only"
        );
        let mut values = vec![];
        for (schema, address) in [
            ("org.gnome.system.proxy.http", http),
            ("org.gnome.system.proxy.https", http),
            ("org.gnome.system.proxy.socks", socks),
        ] {
            values.push((
                schema,
                "host",
                address
                    .map(|a| format!("'{}'", a.ip()))
                    .unwrap_or_else(|| "''".into()),
            ));
            values.push((
                schema,
                "port",
                address.map(|a| a.port()).unwrap_or(0).to_string(),
            ));
        }
        values.push(("org.gnome.system.proxy.ftp", "host", "''".into()));
        values.push(("org.gnome.system.proxy", "use-same-proxy", "false".into()));
        values.push(("org.gnome.system.proxy", "mode", "'manual'".into()));
        for (schema, key, value) in values {
            let old = query(&args(&["gsettings", "get", schema, key]))
                .context("Linux system_proxy requires a GNOME user D-Bus session")?;
            changes.push(Change {
                apply: args(&["gsettings", "set", schema, key, &value]),
                restore: args(&["gsettings", "set", schema, key, &old]),
            });
        }
    } else {
        bail!("system proxy unsupported");
    }
    Ok(changes)
}

impl Guard {
    pub fn apply(config: &ProxyConfig, journal: &Path) -> Result<Self> {
        if config.system_proxy.is_none() {
            return Ok(Self::default());
        }
        super::network_recovery::ensure()?;
        let lock = ownership_lock(None)?;
        let changes = plan(config, command)?;
        if changes.is_empty() {
            return Ok(Self::default());
        }
        let checks = checks(&changes)?;
        let snapshot = Snapshot {
            changes: changes.clone(),
            checks: checks.clone(),
        };
        let text = toml::to_string(&snapshot)?;
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(journal)
            .with_context(|| format!("system proxy recovery file exists or cannot be created: {}; restore it using --restore-system-proxy before starting", journal.display()))?;
        if let Err(error) = astra_platform::platform::adopt_owner(journal) {
            let _ = std::fs::remove_file(journal);
            return Err(anyhow::Error::new(error)
                .context("hand the system proxy journal to the invoking user"));
        }
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        astra_platform::storage::sync_directory(
            journal.parent().context("proxy snapshot directory")?,
        )?;
        let lease = super::network_recovery::Lease::prepare(
            super::network_recovery::Resource::SystemProxy {
                path: std::fs::canonicalize(journal)?,
                snapshot: Some(snapshot.clone()),
            },
        )?;
        let mut lock = lock;
        lock.set_len(0)?;
        lock.write_all(
            std::fs::canonicalize(journal)?
                .as_os_str()
                .as_encoded_bytes(),
        )?;
        lock.sync_all()?;
        let guard = Self {
            changes,
            journal: Some(journal.to_owned()),
            checks,
            lease: Some(lease),
            lock: Some(lock),
        };
        for change in &guard.changes {
            command(&change.apply)?;
        }
        guard.lease.as_ref().unwrap().commit()?;
        Ok(guard)
    }
    pub fn restore(&mut self) -> Result<()> {
        for check in &self.checks {
            validate_check(check)?;
            let current = command(&check.query)?;
            ensure!(
                compatible(&current, &check.before, &check.expected),
                "system proxy changed externally; recovery record retained"
            );
        }
        let mut error = None;
        for change in self.changes.iter().rev() {
            if let Err(e) = command(&change.restore) {
                error = Some(e);
            }
        }
        if let Some(error) = error {
            return Err(error);
        }
        if let Some(path) = &self.journal {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        if let Some(lease) = &self.lease {
            lease.complete()?;
        }
        if let Some(lock) = &self.lock {
            lock.set_len(0)?;
            lock.sync_all()?;
        }
        self.lease = None;
        self.journal = None;
        self.changes.clear();
        self.checks.clear();
        self.lock = None;
        Ok(())
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        if let Err(error) = self.restore() {
            if let Some(lease) = &self.lease {
                lease.failed(&error);
            }
            tracing::error!(%error, "failed to restore system proxy settings");
        }
    }
}

/// Explicit recovery after SIGKILL/power loss; only expected OS setting commands
/// are accepted, even if the recovery document was manually edited.
pub fn recover(path: &Path) -> Result<()> {
    let snapshot: Snapshot = toml::from_str(std::str::from_utf8(
        &super::network_recovery::read_private(path, 1024 * 1024)?,
    )?)?;
    restore_snapshot(path, &snapshot)
}
fn restore_snapshot(path: &Path, snapshot: &Snapshot) -> Result<()> {
    for change in &snapshot.changes {
        let restore = &change.restore;
        let mac = restore
            .first()
            .is_some_and(|v| v == "/usr/sbin/networksetup")
            && restore.get(1).is_some_and(|v| {
                matches!(
                    v.as_str(),
                    "-setwebproxy"
                        | "-setsecurewebproxy"
                        | "-setsocksfirewallproxy"
                        | "-setsocksfirewallproxystate"
                        | "-setwebproxystate"
                        | "-setsecurewebproxystate"
                        | "-setautoproxystate"
                        | "-setproxyautodiscovery"
                )
            });
        let gnome = restore.first().is_some_and(|v| v == "gsettings")
            && restore.get(1).is_some_and(|v| v == "set")
            && restore
                .get(2)
                .is_some_and(|v| v.starts_with("org.gnome.system.proxy"));
        let valid_arguments = if mac {
            match restore.get(1).map(String::as_str) {
                Some("-setwebproxy" | "-setsecurewebproxy" | "-setsocksfirewallproxy") => {
                    restore.len() == 5
                }
                _ => restore.len() == 4 && matches!(restore[3].as_str(), "on" | "off"),
            }
        } else if gnome {
            restore.len() == 5
                && matches!(
                    restore[2].as_str(),
                    "org.gnome.system.proxy"
                        | "org.gnome.system.proxy.http"
                        | "org.gnome.system.proxy.https"
                        | "org.gnome.system.proxy.socks"
                        | "org.gnome.system.proxy.ftp"
                )
                && matches!(
                    restore[3].as_str(),
                    "mode" | "use-same-proxy" | "host" | "port"
                )
        } else {
            false
        };
        ensure!(
            (mac || gnome) && valid_arguments,
            "unsupported command in recovery file"
        );
    }
    let lock = ownership_lock(Some(path))?;
    Guard {
        changes: snapshot.changes.clone(),
        journal: Some(path.to_owned()),
        checks: snapshot.checks.clone(),
        lease: None,
        lock: Some(lock),
    }
    .restore()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn system_settings_plan_restores_original_endpoint_and_enable_states() {
        let config: ProxyConfig = toml::from_str("standalone=true\nmixed_listen='127.0.0.1:7891'\nsocks5_listen='127.0.0.1:1080'\n[system_proxy]\nservices=['Wi-Fi']\n[[outbounds]]\nkind='socks5'\ntag='node'\nserver='127.0.0.1:7890'\n").unwrap();
        if !cfg!(target_os = "macos") {
            return;
        }
        let plan = plan(&config, |args| {
            if args[1].ends_with("proxy") {Ok("Enabled: No\nServer: original.example\nPort: 1234\nAuthenticated Proxy Enabled: 0".into())}
            else {Ok("Enabled: Yes".into())}
        }).unwrap();
        assert_eq!(plan.len(), 8);
        for pair in plan[2..].as_chunks::<2>().0 {
            assert_eq!(pair[0].apply.last().unwrap(), "7891");
            assert_eq!(&pair[1].restore[3..], &["original.example", "1234"]);
            assert_eq!(pair[0].restore.last().unwrap(), "off");
        }
        assert!(plan[..2]
            .iter()
            .all(|change| change.restore.last().unwrap() == "on"));
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn gnome_snapshot_captures_restored_values_and_expected_overrides() {
        let config: ProxyConfig=toml::from_str("standalone=true\nmixed_listen='127.0.0.1:7891'\n[system_proxy]\n[[outbounds]]\nkind='socks5'\ntag='node'\nserver='127.0.0.1:7890'\n").unwrap();
        let restored = |args: &[String]| -> Result<String> {
            Ok(match args[3].as_str() {
                "host" => "'original.proxy'",
                "port" => "3128",
                "mode" => "'auto'",
                "use-same-proxy" => "true",
                _ => panic!("unexpected key"),
            }
            .into())
        };
        let changes = plan(&config, restored).unwrap();
        let snapshot = checks_with(&changes, restored).unwrap();
        assert_eq!(snapshot.len(), 9);
        let mode = snapshot.iter().find(|c| c.query[3] == "mode").unwrap();
        assert_eq!(mode.before, "'auto'");
        assert_eq!(mode.expected, "'manual'");
        let http = snapshot
            .iter()
            .find(|c| c.query[2] == "org.gnome.system.proxy.http" && c.query[3] == "host")
            .unwrap();
        assert_eq!(http.before, "'original.proxy'");
        assert_eq!(http.expected, "'127.0.0.1'");
        for change in changes {
            assert!(!change.restore.iter().any(|s| s.contains("7891")));
        }
        assert!(compatible(
            "Enabled: Yes\nServer: original\nPort: 3128",
            "Enabled: No\nServer: original\nPort: 3128",
            "Enabled: Yes\nServer: localhost\nPort: 7891"
        ));
        assert!(!compatible(
            "Enabled: Yes\nServer: external\nPort: 3128",
            "Enabled: No\nServer: original\nPort: 3128",
            "Enabled: Yes\nServer: localhost\nPort: 7891"
        ));
    }
    #[test]
    fn recovery_rejects_arbitrary_commands_before_execution() {
        let path =
            std::env::temp_dir().join(format!("astra-recovery-test-{}.toml", std::process::id()));
        std::fs::write(&path, "[[changes]]\napply=[]\nrestore=['/bin/false']\n").unwrap();
        assert!(recover(&path)
            .unwrap_err()
            .to_string()
            .contains("unsupported command"));
        assert!(path.exists());
        std::fs::remove_file(path).unwrap();
    }
}

pub(super) fn recover_owned(path: &Path) -> Result<()> {
    let bytes = super::network_recovery::read_private(path, 1024 * 1024)?;
    let snapshot: Snapshot = toml::from_str(std::str::from_utf8(&bytes)?)?;
    ensure!(
        !snapshot.checks.is_empty(),
        "legacy proxy snapshot requires explicit --restore-system-proxy"
    );
    for check in &snapshot.checks {
        validate_check(check)?;
    }
    recover(path)
}

fn compatible(current: &str, before: &str, expected: &str) -> bool {
    if current == before || current == expected {
        return true;
    }
    let current: Vec<_> = current.lines().collect();
    let before: Vec<_> = before.lines().collect();
    let expected: Vec<_> = expected.lines().collect();
    current.len() == before.len()
        && current.len() == expected.len()
        && current
            .iter()
            .zip(before)
            .zip(expected)
            .all(|((actual, before), expected)| *actual == before || *actual == expected)
}
fn ownership_lock(recovery: Option<&Path>) -> Result<std::fs::File> {
    let path = astra_platform::platform::home_dir()?.join(".astra/system-proxy.lock");
    astra_platform::storage::prepare_parent(&path)?;
    if let Ok(metadata) = std::fs::symlink_metadata(&path) {
        ensure!(
            metadata.is_file() && !metadata.file_type().is_symlink(),
            "system proxy lock must be a regular file"
        );
    }
    let mut lock = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(&path)?;
    super::network_recovery::validate_file(&lock, &path)?;
    lock.try_lock()
        .context("another Astra instance owns system proxy settings")?;
    let mut previous = String::new();
    (&mut lock).take(32769).read_to_string(&mut previous)?;
    ensure!(previous.len() <= 32768, "invalid system proxy owner record");
    if !previous.is_empty() && Path::new(&previous).is_file() {
        ensure!(
            recovery.is_some_and(
                |p| std::fs::canonicalize(p).ok() == std::fs::canonicalize(&previous).ok()
            ),
            "previous system proxy recovery remains pending: {previous}"
        );
    }
    lock.rewind()?;
    Ok(lock)
}

pub(super) fn recover_snapshot(path: &Path, snapshot: &Snapshot) -> Result<()> {
    ensure!(
        !snapshot.checks.is_empty(),
        "legacy proxy snapshot requires explicit --restore-system-proxy"
    );
    for check in &snapshot.checks {
        validate_check(check)?;
    }
    restore_snapshot(path, snapshot)
}
