use super::*;
use astra_ipc::runtime::{Command, Overrides, Snapshot};
pub(super) fn load(path: &std::path::Path) -> Result<Overrides> {
    Ok(astra_ipc::runtime::load_overrides(path)?)
}

pub(super) fn snapshot(current: &Current) -> Snapshot {
    let app = current.load_full();
    let mut snapshot = app.metrics.snapshot();
    snapshot.mode = app.overrides.read().unwrap().mode;
    snapshot.system_proxy = app.config.system_proxy.is_some();
    snapshot.tun = app.config.tun.is_some();
    snapshot.allow_lan = app.lan_enabled();
    snapshot.groups = app.groups.snapshot(&app);
    snapshot.truncated |= snapshot.groups.len() < app.config.proxy_groups.len()
        || snapshot
            .groups
            .iter()
            .any(|g| g.members.len() < app.config.proxy_groups[&g.name].members.len());
    snapshot
}

impl State {
    pub(super) fn runtime_snapshot(&self) -> Snapshot {
        snapshot(&self.current)
    }

    pub(super) async fn runtime_control(&mut self, command: Command) -> Result<Snapshot> {
        let app = self.current.load_full();
        let previous = app.overrides.read().unwrap().clone();
        let mut next = previous.clone();
        let mut reapply = false;
        let mut disconnect = None;
        match command {
            Command::Snapshot => return Ok(self.runtime_snapshot()),
            Command::Disconnect(id) => {
                anyhow::ensure!(app.metrics.disconnect(id), "connection no longer exists");
                return Ok(self.runtime_snapshot());
            }
            Command::TestGroup(name) => {
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
                    config.mode == astra_ipc::groups::Mode::Select,
                    "only select groups accept manual choices"
                );
                anyhow::ensure!(
                    astra_ipc::groups::member_path(&app.config.proxy_groups, &group, &member)
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
        let path = astra_ipc::storage::runtime_path(&self.path, "overrides")?;
        astra_ipc::storage::write_json(&path, &next)?;
        app.groups
            .commit_overrides(&app, next, disconnect.as_deref());
        if let Some(candidate) = candidate {
            if let Err(error) = self.apply(self.path.clone(), candidate).await {
                app.groups.commit_overrides(&app, previous.clone(), None);
                astra_ipc::storage::write_json(&path, &previous)
                    .context("restore runtime overrides after rejected change")?;
                return Err(error);
            }
        }
        Ok(self.runtime_snapshot())
    }
}
