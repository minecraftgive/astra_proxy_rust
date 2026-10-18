//! OS settings are snapshotted before any write and restored on normal shutdown.
use crate::config::ProxyConfig;
use anyhow::{bail, ensure, Context, Result};
use serde::{Deserialize, Serialize};
use std::{
    io::Write,
    net::SocketAddr,
    os::unix::fs::OpenOptionsExt,
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Clone, Serialize, Deserialize)]
struct Change {
    apply: Vec<String>,
    restore: Vec<String>,
}
#[derive(Default)]
pub struct Guard {
    changes: Vec<Change>,
    journal: Option<PathBuf>,
}
#[derive(Serialize, Deserialize)]
struct Snapshot {
    changes: Vec<Change>,
}
fn command(args: &[String]) -> Result<String> {
    let (program, args) = args.split_first().context("empty OS command")?;
    let output = Command::new(program)
        .args(args)
        .output()
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
    let address: SocketAddr = config
        .socks5_listen
        .as_ref()
        .context("SOCKS listener required")?
        .parse()?;
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
                ("-getwebproxy", "-setwebproxystate"),
                ("-getsecurewebproxy", "-setsecurewebproxystate"),
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
            let old = query(&args(&[
                "/usr/sbin/networksetup",
                "-getsocksfirewallproxy",
                &service,
            ]))?;
            let get = |key: &str| {
                old.lines()
                    .find_map(|line| line.strip_prefix(key))
                    .map(str::trim)
            };
            ensure!(
                get("Authenticated Proxy Enabled:") != Some("1"),
                "cannot restore authenticated SOCKS proxy on {service}; disable it first"
            );
            let host = get("Server:").context("missing proxy Server")?;
            let port = get("Port:").context("missing proxy Port")?;
            let enabled = get("Enabled:") == Some("Yes");
            // networksetup enables the proxy while setting host/port. Undo order
            // restores the endpoint first, then its original on/off state.
            changes.push(Change {
                apply: args(&[
                    "/usr/sbin/networksetup",
                    "-setsocksfirewallproxy",
                    &service,
                    &address.ip().to_string(),
                    &address.port().to_string(),
                ]),
                restore: args(&[
                    "/usr/sbin/networksetup",
                    "-setsocksfirewallproxystate",
                    &service,
                    if enabled { "on" } else { "off" },
                ]),
            });
            changes.push(Change {
                apply: args(&[
                    "/usr/sbin/networksetup",
                    "-setsocksfirewallproxystate",
                    &service,
                    "on",
                ]),
                restore: args(&[
                    "/usr/sbin/networksetup",
                    "-setsocksfirewallproxy",
                    &service,
                    if host.is_empty() { "" } else { host },
                    port,
                ]),
            });
        }
    } else if cfg!(target_os = "linux") {
        ensure!(
            settings.services.is_empty(),
            "system_proxy.services is macOS-only"
        );
        for (schema, key, value) in [
            ("org.gnome.system.proxy.http", "host", "''".to_owned()),
            ("org.gnome.system.proxy.https", "host", "''".to_owned()),
            ("org.gnome.system.proxy.ftp", "host", "''".to_owned()),
            (
                "org.gnome.system.proxy.socks",
                "host",
                format!("'{}'", address.ip()),
            ),
            (
                "org.gnome.system.proxy.socks",
                "port",
                address.port().to_string(),
            ),
            ("org.gnome.system.proxy", "mode", "'manual'".to_owned()),
        ] {
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
        let changes = plan(config, command)?;
        if changes.is_empty() {
            return Ok(Self::default());
        }
        let text = toml::to_string(&Snapshot {
            changes: changes.clone(),
        })?;
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(journal)
            .with_context(|| format!("system proxy recovery file exists or cannot be created: {}; restore it using --restore-system-proxy before starting", journal.display()))?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        let guard = Self {
            changes,
            journal: Some(journal.to_owned()),
        };
        for change in &guard.changes {
            command(&change.apply)?;
        }
        Ok(guard)
    }
    pub fn restore(&mut self) -> Result<()> {
        let mut error = None;
        for change in self.changes.iter().rev() {
            if let Err(e) = command(&change.restore) {
                error = Some(e);
            }
        }
        if let Some(error) = error {
            return Err(error);
        }
        self.changes.clear();
        if let Some(path) = self.journal.take() {
            std::fs::remove_file(path)?;
        }
        Ok(())
    }
}
impl Drop for Guard {
    fn drop(&mut self) {
        if let Err(error) = self.restore() {
            tracing::error!(%error, "failed to restore system proxy settings");
        }
    }
}

/// Explicit recovery after SIGKILL/power loss; only expected OS setting commands
/// are accepted, even if the recovery document was manually edited.
pub fn recover(path: &Path) -> Result<()> {
    let snapshot: Snapshot = toml::from_str(&std::fs::read_to_string(path)?)?;
    for change in &snapshot.changes {
        let restore = &change.restore;
        let mac = restore
            .first()
            .is_some_and(|v| v == "/usr/sbin/networksetup")
            && restore.get(1).is_some_and(|v| {
                matches!(
                    v.as_str(),
                    "-setsocksfirewallproxy"
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
        ensure!(mac || gnome, "unsupported command in recovery file");
    }
    Guard {
        changes: snapshot.changes,
        journal: Some(path.to_owned()),
    }
    .restore()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn system_settings_plan_restores_original_endpoint_and_enable_states() {
        let config: ProxyConfig = toml::from_str("standalone=true\nsocks5_listen='127.0.0.1:1080'\n[system_proxy]\nservices=['Wi-Fi']\n[[outbounds]]\nkind='socks5'\ntag='node'\nserver='127.0.0.1:7890'\n").unwrap();
        if !cfg!(target_os = "macos") {
            return;
        }
        let plan = plan(&config, |args| {
            if args[1] == "-getsocksfirewallproxy" {Ok("Enabled: No\nServer: original.example\nPort: 1234\nAuthenticated Proxy Enabled: 0".into())}
            else {Ok("Enabled: Yes".into())}
        }).unwrap();
        assert_eq!(plan.len(), 6);
        assert_eq!(plan[4].apply.last().unwrap(), "1080");
        assert_eq!(&plan[5].restore[3..], &["original.example", "1234"]);
        assert_eq!(plan[4].restore.last().unwrap(), "off");
        assert!(plan[..4]
            .iter()
            .all(|change| change.restore.last().unwrap() == "on"));
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
