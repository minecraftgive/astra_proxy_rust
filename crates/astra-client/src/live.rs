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
        self.connection_rows_at(self.page)
    }
    pub fn connection_rows_at(&self, page: usize) -> Vec<ConnectionRow> {
        let now = Instant::now();
        let fresh = self
            .live_state
            .fresh_connections
            .iter()
            .filter_map(|(&id, &until)| (until > now).then_some(id))
            .collect();
        connections::build_rows(
            self.visible_connections_at(page),
            &self.live_state.expanded_targets,
            &fresh,
        )
    }

    pub fn selected_connection_row_key(&self) -> Option<ConnectionRowKey> {
        self.selected_connection_row_key_at(self.page)
    }
    pub fn selected_connection_row_key_at(&self, page: usize) -> Option<ConnectionRowKey> {
        let rows = self.connection_rows_at(page);
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
            .visible_connections_at(page)
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
        self.selected_connection_at(self.page)
    }
    pub fn selected_connection_at(&self, page: usize) -> Option<&astra_model::runtime::Connection> {
        let ConnectionRowKey::Connection(id) = self.selected_connection_row_key_at(page)? else {
            return None;
        };
        self.visible_connections_at(page)
            .into_iter()
            .find(|c| c.id == id)
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
        self.visible_connections_at(self.page)
    }
    pub fn visible_connections_at(&self, page: usize) -> Vec<&astra_model::runtime::Connection> {
        let filter = if page == CONNECTIONS {
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PendingProxySelection {
    pub instance: String,
    pub pid: u32,
    pub group: String,
    pub member: String,
}
impl Client {
    pub fn proxy_groups(&self, query: &str) -> Vec<astra_model::runtime::Group> {
        let query = query.to_lowercase();
        self.live()
            .map(|snapshot| {
                snapshot
                    .groups
                    .iter()
                    .filter(|group| {
                        group.name.to_lowercase().contains(&query)
                            || group
                                .members
                                .iter()
                                .any(|member| member.name.to_lowercase().contains(&query))
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }
    pub fn selected_proxy_group(&self) -> Option<&astra_model::runtime::Group> {
        self.live()?.groups.get(self.group_selection)
    }
    pub fn select_proxy_group(&mut self, name: &str) -> bool {
        let Some(index) = self
            .live()
            .and_then(|snapshot| snapshot.groups.iter().position(|group| group.name == name))
        else {
            return false;
        };
        self.group_selection = index;
        self.runtime_picker = None;
        true
    }
    pub fn select_proxy_member(&mut self, group: &str, member: &str) -> Input {
        if !self.select_proxy_group(group) {
            return Input::Idle;
        }
        let Some(index) = self
            .selected_proxy_group()
            .and_then(|group| group.members.iter().position(|item| item.name == member))
        else {
            return Input::Idle;
        };
        self.open_runtime_group();
        let input = self.choose_runtime_member(index);
        if let Input::Run(
            Action::Runtime {
                name,
                expected_pid: Some(pid),
                command: Command::Select { group, member },
            },
            _,
        ) = &input
        {
            self.pending_proxy_selection = Some(PendingProxySelection {
                instance: name.clone(),
                pid: *pid,
                group: group.clone(),
                member: member.clone(),
            });
        }
        input
    }
    pub fn proxy_member_pending(&self, group: &str, member: &str) -> bool {
        self.pending_proxy_selection
            .as_ref()
            .is_some_and(|pending| {
                self.active_proxy().is_some_and(|row| {
                    row.instance.name == pending.instance && row.pid == Some(pending.pid)
                }) && pending.group == group
                    && pending.member == member
            })
    }
    pub fn reconcile_proxy_selection(&mut self, name: &str, snapshot: &Snapshot) {
        if self
            .pending_proxy_selection
            .as_ref()
            .is_some_and(|pending| {
                pending.instance == name
                    && (pending.pid != snapshot.pid
                        || snapshot
                            .groups
                            .iter()
                            .find(|group| group.name == pending.group)
                            .is_none_or(|group| group.selected.as_ref() == Some(&pending.member)))
            })
        {
            self.pending_proxy_selection = None;
        }
    }
    pub fn reject_proxy_selection(&mut self, action: &Action) {
        if let Action::Runtime {
            name,
            expected_pid: Some(pid),
            command: Command::Select { group, member },
        } = action
        {
            if self
                .pending_proxy_selection
                .as_ref()
                .is_some_and(|pending| {
                    &pending.instance == name
                        && pending.pid == *pid
                        && &pending.group == group
                        && &pending.member == member
                })
            {
                self.pending_proxy_selection = None;
            }
        }
    }
    pub fn filter_connections(&mut self, query: String) {
        self.connection_filter = query;
        self.sync_connection_selection();
    }
    pub fn show_connection(&mut self, key: ConnectionRowKey) -> Input {
        if !self.connection_rows().iter().any(|row| row.key == key) {
            return Input::Idle;
        }
        self.select_connection_row_key(key);
        if !self.toggle_connection_group() {
            self.live_state.detail_open = true;
        }
        Input::Idle
    }
    pub fn close_connection(&mut self, id: u64) -> Input {
        if !self.runtime_error.is_empty()
            || !self.live().is_some_and(|snapshot| {
                snapshot
                    .connections
                    .iter()
                    .any(|connection| connection.id == id)
            })
        {
            self.message = t("Connection cannot be verified in the latest sample").into();
            return Input::Idle;
        }
        self.runtime_action(Command::Disconnect(Some(id)))
    }
    pub fn close_connection_detail(&mut self) {
        self.live_state.detail_open = false;
    }
    pub fn toggle_traffic_panel(&mut self) {
        self.traffic_expanded = !self.traffic_expanded;
    }
    pub fn cycle_mode(&mut self) -> Input {
        let Some(snapshot) = self.live() else {
            return Input::Idle;
        };
        self.runtime_action(Command::SetMode(match snapshot.mode {
            Mode::Rule => Mode::Global,
            Mode::Global => Mode::Direct,
            Mode::Direct => Mode::Rule,
        }))
    }
    pub fn toggle_system_proxy(&mut self) -> Input {
        let Some(value) = self.live().map(|s| !s.system_proxy) else {
            return Input::Idle;
        };
        self.runtime_action(Command::SystemProxy(value))
    }
    pub fn toggle_tun(&mut self) -> Input {
        let Some(value) = self.live().map(|s| !s.tun) else {
            return Input::Idle;
        };
        self.runtime_action(Command::Tun(value))
    }
    pub fn toggle_lan(&mut self) -> Input {
        let Some(value) = self.live().map(|s| !s.allow_lan) else {
            return Input::Idle;
        };
        self.runtime_action(Command::AllowLan(value))
    }
}
impl Client {
    pub fn move_proxy_group(&mut self, delta: isize) {
        self.group_selection = self.group_selection.saturating_add_signed(delta).min(
            self.live()
                .map_or(0, |snapshot| snapshot.groups.len().saturating_sub(1)),
        );
        self.runtime_picker = None;
    }
    pub fn proxy_members(&self, query: &str) -> Vec<astra_model::runtime::Member> {
        let query = query.to_lowercase();
        self.selected_proxy_group()
            .map(|group| {
                group
                    .members
                    .iter()
                    .filter(|member| {
                        member.name.to_lowercase().contains(&query)
                            || group.name.to_lowercase().contains(&query)
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }
    pub fn close_connection_row(&mut self, key: ConnectionRowKey) -> Input {
        match key {
            ConnectionRowKey::Connection(id) => self.close_connection(id),
            ConnectionRowKey::Target(_) => Input::Idle,
        }
    }
    pub fn move_connection_selection(&mut self, delta: isize) {
        let current = self.selected_connection_row_index().unwrap_or(0);
        self.select_connection(
            current
                .saturating_add_signed(delta)
                .min(self.connection_rows().len().saturating_sub(1)),
        );
    }
    pub fn activate_connection_selection(&mut self) -> Input {
        if let Some(key) = self.selected_connection_row_key() {
            self.show_connection(key)
        } else {
            Input::Idle
        }
    }
    pub fn close_selected_connection(&mut self) -> Input {
        if let Some(key) = self.selected_connection_row_key() {
            self.close_connection_row(key)
        } else {
            Input::Idle
        }
    }
}
impl Client {
    pub fn filter_proxy_groups(&mut self, query: &str) {
        self.searches
            .insert(crate::navigation::PROXIES, query.to_owned());
        let groups = self.proxy_groups(query);
        if !groups.iter().any(|group| {
            self.selected_proxy_group()
                .is_some_and(|selected| selected.name == group.name)
        }) {
            if let Some(group) = groups.first() {
                self.select_proxy_group(&group.name);
            }
        }
    }
}

pub fn decimal_bytes(value: &str) -> String {
    value
        .parse()
        .map(bytes)
        .unwrap_or_else(|_| format!("{value} B"))
}

#[derive(Clone, Debug)]
pub struct ConnectionActionTarget {
    pub instance: String,
    pub pid: u32,
    pub id: u64,
}
impl Client {
    pub fn connection_action_target(
        &self,
        key: &ConnectionRowKey,
    ) -> Option<ConnectionActionTarget> {
        let ConnectionRowKey::Connection(id) = key else {
            return None;
        };
        let snapshot = self.connection_snapshot()?;
        snapshot
            .connections
            .iter()
            .any(|connection| connection.id == *id)
            .then(|| ConnectionActionTarget {
                instance: self.runtime_instance.clone(),
                pid: snapshot.pid,
                id: *id,
            })
    }
    pub fn close_connection_target(&mut self, target: ConnectionActionTarget) -> Input {
        if !self
            .active_proxy()
            .is_some_and(|row| row.instance.name == target.instance && row.pid == Some(target.pid))
        {
            self.message = t("Connection cannot be verified in the latest sample").into();
            return Input::Idle;
        }
        self.close_connection(target.id)
    }
    pub fn direct_rate_values(&self) -> [Option<u64>; 2] {
        let available = self.live_rates_available();
        self.direct_rates.map(|value| available.then_some(value))
    }
}

#[cfg(test)]
#[path = "test/live_actions_test.rs"]
mod action_tests;
