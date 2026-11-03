use super::*;
use astra_model::runtime::Command;
use astra_model::runtime::Overrides;
use astra_model::runtime::Snapshot;

pub(super) struct OverrideCheckpoint {
    path: PathBuf,
    previous: Option<Vec<u8>>,
    consistency: Arc<std::sync::OnceLock<String>>,
    committed: bool,
}
impl OverrideCheckpoint {
    fn begin(
        path: PathBuf,
        next: &Overrides,
        consistency: Arc<std::sync::OnceLock<String>>,
    ) -> Result<Self> {
        let previous = match astra_platform::storage::read_bounded(&path, 1024 * 1024) {
            Ok(bytes) => Some(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            Err(error) => return Err(error.into()),
        };
        // Atomic replacement can succeed before its directory flush fails.
        // Arm rollback before attempting the write, including that error path.
        let checkpoint = Self {
            path,
            previous,
            consistency,
            committed: false,
        };
        astra_platform::storage::write_json(&checkpoint.path, next)?;
        Ok(checkpoint)
    }
    pub(super) fn commit(&mut self) {
        self.committed = true;
    }
}
impl Drop for OverrideCheckpoint {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        let restored = match &self.previous {
            Some(bytes) => astra_platform::storage::atomic_write(
                &self.path,
                bytes,
                astra_platform::storage::WritePolicy::runtime(),
            ),
            None => match std::fs::remove_file(&self.path) {
                Ok(()) => self
                    .path
                    .parent()
                    .map_or(Ok(()), astra_platform::storage::sync_directory),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
                Err(error) => Err(error),
            },
        };
        if let Err(error) = restored {
            let message = format!(
                "Runtime override outcome unknown: restoring its checkpoint failed: {error}"
            );
            tracing::error!(%message);
            let _ = self.consistency.set(message);
        }
    }
}
pub(super) fn load(path: &std::path::Path) -> Result<Overrides> {
    Ok(astra_platform::runtime::load_overrides(path)?)
}

pub(super) fn snapshot(current: &Current) -> Snapshot {
    let app = current.load_full();
    let mut snapshot = app.metrics.snapshot();
    snapshot.mode = app.overrides.load().mode;
    snapshot.system_proxy = app.config.system_proxy.is_some();
    snapshot.tun = app.config.tun.is_some();
    snapshot.allow_lan = app.lan_enabled();
    snapshot.groups = app.groups.snapshot(&app);
    if let Some(host) = &app.hosted {
        host.merge_snapshot(&mut snapshot);
    }
    snapshot.truncated |= snapshot.groups.len() < app.config.proxy_groups.len()
        || snapshot
            .groups
            .iter()
            .any(|g| g.members.len() < app.config.proxy_groups[&g.name].members.len());
    snapshot
}

impl State {
    pub(super) fn begin_override_checkpoint(
        &self,
        source: &std::path::Path,
        next: &Overrides,
    ) -> Result<OverrideCheckpoint> {
        let path = astra_platform::storage::runtime_path(source, "overrides")?;
        let mut value = next.clone();
        // Keep the existing elevated-start policy: a later unprivileged launch
        // must not silently inherit this process's privileged TUN session.
        if self.elevated_tun {
            value.tun = None;
        }
        OverrideCheckpoint::begin(path, &value, self.consistency.clone())
    }

    pub(super) fn runtime_snapshot(&self) -> Snapshot {
        snapshot(&self.current)
    }

    pub(super) async fn runtime_control(&mut self, command: Command) -> Result<Snapshot> {
        if let Some(error) = self.consistency.get() {
            anyhow::bail!("{error}");
        }
        if let Some(host) = &self.hosted {
            host.ensure_consistent()?;
        }
        if !matches!(
            command,
            Command::Snapshot | Command::Disconnect(_) | Command::TestGroup(_)
        ) {
            if let Some(host) = &self.hosted {
                anyhow::ensure!(
                    !host.busy().await,
                    "an update is prepared; retry changes after activation"
                );
            }
        }
        let app = self.current.load_full();
        let previous = (*app.overrides.load_full()).clone();
        let mut next = previous.clone();
        let mut reapply = false;
        let mut disconnect = None;
        match command {
            Command::Snapshot => return Ok(self.runtime_snapshot()),
            Command::Disconnect(id) => {
                if let Some(host) = &app.hosted {
                    anyhow::ensure!(host.disconnect(id).await?, "connection no longer exists");
                    return Ok(self.runtime_snapshot());
                }
                anyhow::ensure!(app.metrics.disconnect(id), "connection no longer exists");
                return Ok(self.runtime_snapshot());
            }
            Command::TestGroup(name) => {
                if let Some(host) = &app.hosted {
                    host.test_group(name).await?;
                    return Ok(self.runtime_snapshot());
                }
                crate::proxy::groups::Groups::request_test(app, name)?;
                return Ok(self.runtime_snapshot());
            }
            Command::SetMode(mode) => next.mode = mode,
            Command::AllowLan(enabled) => {
                if enabled {
                    anyhow::ensure!(app.config.socks5_listen.iter().chain(&app.config.http_listen).chain(&app.config.mixed_listen).any(|s| s.parse::<std::net::SocketAddr>().is_ok_and(|a| !a.ip().is_loopback())), "configure a non-loopback listener such as 0.0.0.0:7890 before enabling LAN access");
                }
                next.allow_lan = Some(enabled);
            }
            Command::Select { group, member } => {
                let config = app
                    .config
                    .proxy_groups
                    .get(&group)
                    .context("unknown proxy group")?;
                anyhow::ensure!(
                    config.mode == astra_model::groups::Mode::Select,
                    "only select groups accept manual choices"
                );
                anyhow::ensure!(
                    astra_model::groups::member_path(&app.config.proxy_groups, &group, &member)
                        .is_some(),
                    "node is not a reachable group member"
                );
                let current = previous
                    .selected
                    .get(&group)
                    .or(config.selected.as_ref())
                    .or_else(|| config.members.first());
                if config.disconnect_on_manual && current != Some(&member) {
                    disconnect = Some(group.clone());
                }
                next.selected.insert(group, member);
            }
            Command::SystemProxy(enabled) => {
                next.system_proxy = Some(enabled);
                reapply = true;
            }
            Command::Tun(enabled) => {
                next.tun = Some(enabled);
                reapply = true;
            }
            Command::Reset => {
                next = Overrides::default();
                reapply = true;
            }
        }
        let candidate = if reapply {
            let path = self.path.clone();
            let text = self.text.clone();
            let mut candidate = tokio::task::spawn_blocking(move || {
                App::from_text(&path, text).map(|app| crate::subscription::cached_app(&path, app))
            })
            .await??;
            crate::mode_config::runtime_modes(&mut candidate.config, &next);
            if candidate.config.system_proxy.is_some() {
                crate::mode_config::system_proxy_address(&candidate.config)?;
            }
            Some(candidate)
        } else {
            None
        };
        if let Some(candidate) = candidate {
            // Candidate preparation owns its overrides. Cancellation before the
            // final synchronous commit cannot change current routing or disk.
            self.apply_overrides(self.path.clone(), candidate, Some(next.clone()))
                .await?;
        } else if let Some(host) = &app.hosted {
            // A lost worker reply may follow a successful worker-side commit.
            // Never claim a rollback by restoring only the host's copy.
            host.overrides(next.clone(), disconnect.clone(), || {
                let mut checkpoint = self.begin_override_checkpoint(&self.path, &next)?;
                app.groups
                    .commit_overrides(&app, next, disconnect.as_deref());
                checkpoint.commit();
                Ok(())
            })
            .await?;
        } else {
            let mut checkpoint = self.begin_override_checkpoint(&self.path, &next)?;
            app.groups
                .commit_overrides(&app, next, disconnect.as_deref());
            checkpoint.commit();
        }
        Ok(self.runtime_snapshot())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Directory(PathBuf);
    impl Directory {
        fn new() -> Self {
            let path = std::env::temp_dir().join(format!(
                "astra-override-checkpoint-{}",
                uuid::Uuid::new_v4()
            ));
            std::fs::create_dir(&path).unwrap();
            Self(path)
        }
    }
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
    #[test]
    fn rejected_override_checkpoint_restores_exact_previous_file_or_absence() {
        let directory = Directory::new();
        let path = directory.0.join("overrides.json");
        let fault = Arc::new(std::sync::OnceLock::new());
        let next = Overrides {
            allow_lan: Some(true),
            ..Default::default()
        };
        std::fs::write(&path, b"{\n  \"allow_lan\": false\n}\n").unwrap();
        let before = std::fs::read(&path).unwrap();
        drop(OverrideCheckpoint::begin(path.clone(), &next, fault.clone()).unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), before);
        std::fs::remove_file(&path).unwrap();
        drop(OverrideCheckpoint::begin(path.clone(), &next, fault.clone()).unwrap());
        assert!(!path.exists());
        assert!(fault.get().is_none());
        let mut checkpoint = OverrideCheckpoint::begin(path.clone(), &next, fault.clone()).unwrap();
        checkpoint.commit();
        drop(checkpoint);
        let saved: Overrides = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(saved.allow_lan, Some(true));
    }
    #[test]
    fn failed_override_rollback_latches_uncertain_runtime() {
        let directory = Directory::new();
        let path = directory.0.join("overrides.json");
        std::fs::write(&path, b"{}").unwrap();
        let fault = Arc::new(std::sync::OnceLock::new());
        let checkpoint =
            OverrideCheckpoint::begin(path.clone(), &Overrides::default(), fault.clone()).unwrap();
        std::fs::remove_file(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        drop(checkpoint);
        assert!(fault.get().unwrap().contains("outcome unknown"));
        assert!(path.is_dir());
    }
}
