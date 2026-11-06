//! Shared live data, selection, filtering and semantic runtime actions.
use crate::{
    connections::{self, ConnectionRow, ConnectionRowKey, ConnectionTarget},
    i18n::t,
    navigation::CONNECTIONS,
    protocol::Action,
    state::{Client, Input, Prompt},
    tr,
};
use astra_model::runtime::{Command, Coverage, Mode, RuntimeTelemetry, Snapshot};
use std::{
    collections::{HashMap, HashSet},
    time::{Duration, Instant},
};

#[derive(Default)]
pub struct State {
    pub selected: Option<u64>,
    pub selected_target: Option<ConnectionTarget>,
    pub expanded_targets: HashSet<ConnectionTarget>,
    pub fresh_connections: HashMap<u64, Instant>,
    pub frozen: Option<Snapshot>,
    pub frozen_telemetry: Option<RuntimeTelemetry>,
    pub frozen_at: Option<String>,
    pub detail_open: bool,
    pub detail_scroll: u16,
}

pub fn bytes(value: u64) -> String {
    for (unit, scale) in [("GiB", 1u64 << 30), ("MiB", 1 << 20), ("KiB", 1 << 10)] {
        if value >= scale {
            return format!("{:.1} {unit}", value as f64 / scale as f64);
        }
    }
    format!("{value} B")
}

pub fn rate_value(value: Option<u64>, compact: bool) -> String {
    let width = if compact { 7 } else { 10 };
    let Some(value) = value else {
        return format!("{:>width$}", "—");
    };
    for (unit, short, shift) in [
        ("EiB", "E", 60),
        ("PiB", "P", 50),
        ("TiB", "T", 40),
        ("GiB", "G", 30),
        ("MiB", "M", 20),
        ("KiB", "K", 10),
    ] {
        let scale = 1u64 << shift;
        if value >= scale {
            let number = value as f64 / scale as f64;
            let text = if compact {
                format!("{number:.1}{short}")
            } else {
                format!("{number:.1} {unit}")
            };
            return format!("{text:>width$}");
        }
    }
    let text = if compact {
        value.to_string()
    } else {
        format!("{value} B")
    };
    format!("{text:>width$}")
}

pub fn exit_chain(snapshot: &Snapshot) -> (Vec<String>, Option<u64>, Option<bool>) {
    let mut chain = vec![match snapshot.mode {
        Mode::Direct => "DIRECT".to_owned(),
        _ => "PROXY".to_owned(),
    }];
    let (mut delay, mut alive) = (None, None);
    for _ in 0..8 {
        let Some(group) = snapshot
            .groups
            .iter()
            .find(|g| Some(&g.name) == chain.last())
        else {
            break;
        };
        let Some(next) = group.selected.clone() else {
            break;
        };
        if let Some(member) = group.members.iter().find(|m| m.name == next) {
            (delay, alive) = (member.delay_ms, member.alive);
        }
        if chain.contains(&next) {
            break;
        }
        chain.push(next);
    }
    (chain, delay, alive)
}

impl Client {
    pub fn live_rates_available(&self) -> bool {
        self.live().is_some() && self.runtime_rates_available()
    }
    pub fn sync_connection_selection(&mut self) {
        if let Some(index) = self.selected_connection_row_index() {
            self.connection_selection = index;
        }
    }

    pub fn track_new_connections(&mut self, snapshot: &Snapshot) {
        let Some(previous) = &self.runtime_snapshot else {
            return;
        };
        let known: HashSet<_> = previous.connections.iter().map(|c| c.id).collect();
        let now = Instant::now();
        for connection in &snapshot.connections {
            // Older entries reappearing in a partial sample are not new connections.
            if !known.contains(&connection.id) && connection.elapsed_ms < 5000 {
                self.live_state
                    .fresh_connections
                    .entry(connection.id)
                    .or_insert(now + Duration::from_secs(5));
            }
        }
    }

    pub fn next_connection_expiry(&self) -> Option<Instant> {
        self.live_state.fresh_connections.values().copied().min()
    }

    pub fn expire_connection_highlights(&mut self) -> bool {
        let now = Instant::now();
        let count = self.live_state.fresh_connections.len();
        self.live_state
            .fresh_connections
            .retain(|_, until| *until > now);
        let changed = count != self.live_state.fresh_connections.len();
        if changed {
            self.sync_connection_selection();
        }
        changed
    }

    pub fn live(&self) -> Option<&Snapshot> {
        let row = self.active_proxy()?;
        self.runtime_snapshot
            .as_ref()
            .filter(|s| self.runtime_instance == row.instance.name && Some(s.pid) == row.pid)
    }

    pub fn runtime_health(&self) -> String {
        if !self.runtime_error.is_empty() {
            return t("Reconnecting · last received data").into();
        }
        if self
            .runtime_received
            .is_some_and(|at| at.elapsed() > Duration::from_secs(3))
        {
            return t("Runtime data stale").into();
        }
        match &self.runtime_telemetry {
            Some(meta) if meta.ingress_paused => t("Update: new connections paused").into(),
            Some(meta) if meta.coverage == Coverage::Partial => {
                t("Partial worker telemetry").into()
            }
            Some(meta)
                if matches!(meta.coverage, Coverage::Stale | Coverage::Unknown)
                    || meta.sample_age_ms.saturating_add(
                        self.runtime_received
                            .map_or(0, |at| at.elapsed().as_millis() as u64),
                    ) > 3000 =>
            {
                t("Worker telemetry stale").into()
            }
            Some(meta) => tr!("Generation {} · live", meta.active_generation.unwrap_or(0)),
            None if self.live().is_some() => t("Legacy telemetry · source age unknown").into(),
            None => t("Waiting for runtime data…").into(),
        }
    }

    pub fn connections_paused(&self) -> bool {
        self.live_state.frozen.is_some()
    }

    pub fn live_detail_open(&self) -> bool {
        self.live_state.detail_open
    }

    pub fn connection_badges(&self) -> String {
        let mut badges = Vec::new();
        let paused = tr!(
            "PAUSED {}",
            self.live_state.frozen_at.as_deref().unwrap_or("—")
        );
        if self.connections_paused() {
            badges.push(paused.as_str());
        }
        if !self.runtime_error.is_empty() {
            badges.push(t("RECONNECTING"));
        } else if self
            .runtime_received
            .is_some_and(|at| at.elapsed() > Duration::from_secs(3))
        {
            badges.push(t("STALE"));
        } else if let Some(meta) = &self.runtime_telemetry {
            if meta.ingress_paused {
                badges.push(t("INGRESS PAUSED"));
            }
            if matches!(meta.coverage, Coverage::Stale | Coverage::Unknown)
                || meta.sample_age_ms.saturating_add(
                    self.runtime_received
                        .map_or(0, |at| at.elapsed().as_millis() as u64),
                ) > 3000
            {
                badges.push(t("STALE"));
            } else if meta.coverage == Coverage::Partial {
                badges.push(t("PARTIAL"));
            }
        }
        if badges.is_empty() {
            String::new()
        } else {
            format!("{} · ", badges.join(" · "))
        }
    }

    pub fn connection_pause_hint(&self) -> Option<String> {
        self.connections_paused().then(|| {
            tr!(
                "Paused {} · background updates continue · Space resume",
                self.live_state.frozen_at.as_deref().unwrap_or("—")
            )
        })
    }

    pub fn toggle_live_pause(&mut self) -> Input {
        if self.live_state.frozen.take().is_none() {
            self.live_state.frozen = self.live().cloned();
            self.live_state.frozen_telemetry = self.runtime_telemetry.clone();
            self.live_state.frozen_at = self
                .live_state
                .frozen
                .as_ref()
                .map(|_| chrono::Local::now().format("%H:%M:%S").to_string());
        } else {
            self.live_state.frozen_telemetry = None;
            self.live_state.frozen_at = None;
        }
        self.sync_connection_selection();
        Input::Idle
    }

    pub fn connection_snapshot(&self) -> Option<&Snapshot> {
        self.live()?;
        self.live_state.frozen.as_ref().or_else(|| self.live())
    }

    pub fn connection_rows(&self) -> Vec<ConnectionRow> {
        let now = Instant::now();
        let fresh = self
            .live_state
            .fresh_connections
            .iter()
            .filter_map(|(&id, &until)| (until > now).then_some(id))
            .collect();
        connections::build_rows(
            self.visible_connections(),
            &self.live_state.expanded_targets,
            &fresh,
        )
    }

    pub fn selected_connection_row_key(&self) -> Option<ConnectionRowKey> {
        let rows = self.connection_rows();
        if let Some(target) = &self.live_state.selected_target {
            return rows
                .iter()
                .find(|r| {
                    r.key == ConnectionRowKey::Target(target.clone())
                        || (r.parent.as_ref() == Some(target) && !r.is_group())
                })
                .map(|r| r.key.clone());
        }
        let id = self.live_state.selected?;
        let key = ConnectionRowKey::Connection(id);
        if rows.iter().any(|r| r.key == key) {
            return Some(key);
        }
        let target = self
            .visible_connections()
            .into_iter()
            .find(|c| c.id == id)
            .map(ConnectionTarget::of)?;
        rows.iter()
            .find(|r| r.key == ConnectionRowKey::Target(target.clone()))
            .map(|r| r.key.clone())
    }

    pub fn selected_connection_row_index(&self) -> Option<usize> {
        let key = self.selected_connection_row_key()?;
        self.connection_rows().iter().position(|r| r.key == key)
    }

    pub fn select_connection_row_key(&mut self, key: ConnectionRowKey) {
        if self.selected_connection_row_key().as_ref() != Some(&key) {
            self.live_state.detail_scroll = 0;
        }
        match key {
            ConnectionRowKey::Target(target) => {
                self.live_state.selected_target = Some(target);
                self.live_state.selected = None;
            }
            ConnectionRowKey::Connection(id) => {
                self.live_state.selected_target = None;
                self.live_state.selected = Some(id);
            }
        }
        self.sync_connection_selection();
    }

    pub fn select_connection(&mut self, index: usize) {
        if let Some(key) = self.connection_rows().get(index).map(|r| r.key.clone()) {
            self.select_connection_row_key(key);
        } else {
            self.live_state.selected = None;
            self.live_state.selected_target = None;
            self.connection_selection = 0;
        }
    }

    pub fn select_connection_id(&mut self, id: u64) {
        if !self.visible_connections().iter().any(|c| c.id == id)
            && self
                .connection_snapshot()
                .is_some_and(|s| s.connections.iter().any(|c| c.id == id))
        {
            self.connection_filter.clear();
        }
        if let Some(target) = self
            .visible_connections()
            .into_iter()
            .find(|c| c.id == id)
            .map(ConnectionTarget::of)
        {
            self.live_state.expanded_targets.insert(target);
            self.select_connection_row_key(ConnectionRowKey::Connection(id));
        }
    }

    pub fn reveal_selected_connection(&mut self) {
        if let Some(id) = self.live_state.selected {
            if !self.visible_connections().iter().any(|c| c.id == id)
                && self
                    .connection_snapshot()
                    .is_some_and(|s| s.connections.iter().any(|c| c.id == id))
            {
                self.connection_filter.clear();
            }
        }
        self.sync_connection_selection();
    }

    pub fn connection_is_selected(&self, id: u64) -> bool {
        self.live_state.selected == Some(id)
    }

    pub fn selected_connection(&self) -> Option<&astra_model::runtime::Connection> {
        let ConnectionRowKey::Connection(id) = self.selected_connection_row_key()? else {
            return None;
        };
        self.visible_connections().into_iter().find(|c| c.id == id)
    }

    pub fn toggle_connection_group(&mut self) -> bool {
        let Some(ConnectionRowKey::Target(target)) = self.selected_connection_row_key() else {
            return false;
        };
        if !self.live_state.expanded_targets.remove(&target) {
            self.live_state.expanded_targets.insert(target.clone());
        }
        self.select_connection_row_key(ConnectionRowKey::Target(target));
        true
    }

    pub fn expand_connection_group(&mut self) {
        if let Some(ConnectionRowKey::Target(target)) = self.selected_connection_row_key() {
            self.live_state.expanded_targets.insert(target);
            self.sync_connection_selection();
        }
    }

    pub fn collapse_connection_group(&mut self) {
        let key = self.selected_connection_row_key();
        let row = self
            .connection_rows()
            .into_iter()
            .find(|r| Some(&r.key) == key.as_ref());
        let Some(row) = row else {
            return;
        };
        let target = match row.key {
            ConnectionRowKey::Target(target) => Some(target),
            ConnectionRowKey::Connection(_) => row.parent,
        };
        if let Some(target) = target {
            if self.live_state.expanded_targets.remove(&target) {
                self.select_connection_row_key(ConnectionRowKey::Target(target));
            }
        }
    }

    pub fn runtime_action(&mut self, command: Command) -> Input {
        if matches!(command, Command::Tun(true)) {
            let elevation = self.active_proxy().and_then(|row| {
                let name = row.instance.name.clone();
                let pid = row.pid;
                let supported = self
                    .details
                    .get(&name)
                    .is_some_and(|details| details.elevation_supported);
                (pid.is_some() && supported).then_some((name, pid))
            });
            if let Some((name, pid)) = elevation {
                self.prompt(Prompt::ElevateTun { name, pid });
                return Input::Idle;
            }
        }
        match self.active_proxy() {
            Some(row) if row.pid.is_some() => Input::Run(
                Action::Runtime {
                    name: row.instance.name.clone(),
                    command,
                    expected_pid: row.pid,
                },
                None,
            ),
            _ => Input::Idle,
        }
    }

    pub fn open_runtime_group(&mut self) {
        if let Some(group) = self.live().and_then(|s| s.groups.get(self.group_selection)) {
            let selected = group
                .members
                .iter()
                .position(|m| Some(&m.name) == group.selected.as_ref())
                .unwrap_or(0);
            self.runtime_picker = Some((group.name.clone(), selected));
        }
    }

    pub fn choose_runtime_member(&mut self, index: usize) -> Input {
        if self.runtime_picker.is_none() {
            self.open_runtime_group();
        }
        let Some((name, _)) = &self.runtime_picker else {
            return Input::Idle;
        };
        let group = self
            .live()
            .and_then(|s| s.groups.iter().find(|g| &g.name == name));
        let Some(group) = group else {
            return Input::Idle;
        };
        let (select, count) = (group.mode == "select", group.members.len());
        let command = group.members.get(index).map(|member| Command::Select {
            group: group.name.clone(),
            member: member.name.clone(),
        });
        if !select {
            self.message = t("Automatic groups choose members by latency").into();
            if let Some((_, cursor)) = self.runtime_picker.as_mut() {
                *cursor = index.min(count.saturating_sub(1));
            }
            return Input::Idle;
        }
        if let Some(command) = command {
            if let Some((_, cursor)) = self.runtime_picker.as_mut() {
                *cursor = index;
            }
            return self.runtime_action(command);
        }
        Input::Idle
    }

    pub fn visible_connections(&self) -> Vec<&astra_model::runtime::Connection> {
        let filter = if self.page == CONNECTIONS {
            self.connection_filter.to_lowercase()
        } else {
            String::new()
        };
        let mut connections: Vec<_> = self
            .connection_snapshot()
            .map(|s| {
                s.connections
                    .iter()
                    .filter(|c| {
                        filter.is_empty()
                            || format!(
                                "{} {} {} {}",
                                c.host,
                                c.outbound,
                                c.network,
                                c.source.as_deref().unwrap_or_default()
                            )
                            .to_lowercase()
                            .contains(&filter)
                    })
                    .collect()
            })
            .unwrap_or_default();
        connections.sort_by_key(|c| (std::cmp::Reverse(c.upload.saturating_add(c.download)), c.id));
        connections
    }
}
