//! Proxies and Connections pages, driven by the active profile's runtime snapshot.
use super::connections::{ConnectionRow, ConnectionRowKey, ConnectionTarget};
use super::dashboard::{card, Target, BLUE, MUTED};
use super::shell::{CONNECTIONS, OVERVIEW, PROXIES};
use super::*;
use astra_model::runtime::Command;
use astra_model::runtime::Coverage;
use astra_model::runtime::Mode;
use astra_model::runtime::RuntimeTelemetry;
use astra_model::runtime::RuntimeView;
use astra_model::runtime::Snapshot;
use ratatui::{
    layout::Rect,
    style::Modifier,
    widgets::{Cell, Row as TableRow, Table, TableState},
};
use std::collections::HashSet;

#[derive(Default)]
pub(super) struct LiveState {
    telemetry: Option<RuntimeTelemetry>,
    baseline: Option<(Snapshot, RuntimeTelemetry)>,
    received: Option<Instant>,
    selected: Option<u64>,
    selected_target: Option<ConnectionTarget>,
    expanded_targets: HashSet<ConnectionTarget>,
    fresh_connections: HashMap<u64, Instant>,
    frozen: Option<Snapshot>,
    frozen_telemetry: Option<RuntimeTelemetry>,
    frozen_at: Option<String>,
    disconnected: bool,
    rates_valid: bool,
    detail_open: bool,
    detail_scroll: u16,
    viewports: [std::cell::Cell<usize>; 6],
    offsets: [std::cell::Cell<usize>; 6],
}

#[derive(Clone, Copy)]
pub(super) enum Viewport {
    Overview,
    OverviewGroups,
    Groups,
    Members,
    Connections,
    Details,
}

pub(super) fn bytes(value: u64) -> String {
    for (unit, scale) in [("GiB", 1u64 << 30), ("MiB", 1 << 20), ("KiB", 1 << 10)] {
        if value >= scale {
            return format!("{:.1} {unit}", value as f64 / scale as f64);
        }
    }
    format!("{value} B")
}

/// Fixed terminal columns keep live rates still when values or units change.
pub(super) fn rate_value(value: Option<u64>, compact: bool) -> String {
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

/// Latency colored like the GUI: green < 150 ms, yellow < 400 ms, red otherwise.
pub(super) fn delay_span(delay: Option<u64>, alive: Option<bool>) -> Span<'static> {
    match (alive, delay) {
        (Some(false), _) => Span::styled(t("Timeout").to_owned(), Style::default().fg(Color::Red)),
        (_, Some(ms)) => Span::styled(
            format!("{ms} ms"),
            Style::default().fg(if ms < 150 {
                Color::Green
            } else if ms < 400 {
                Color::Yellow
            } else {
                Color::Red
            }),
        ),
        _ => Span::styled("—", Style::default().fg(Color::DarkGray)),
    }
}

/// Follows group selections from the mode's entry group to the final outbound.
pub(super) fn exit_chain(snapshot: &Snapshot) -> (Vec<String>, Option<u64>, Option<bool>) {
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

impl Ui {
    pub(super) fn record_live_viewport(&self, view: Viewport, rows: usize) {
        self.live_state.viewports[view as usize].set(rows);
    }
    fn live_page_size(&self, view: Viewport) -> usize {
        self.live_state.viewports[view as usize].get().max(1)
    }
    /// Retain the viewport until the selection crosses one of its edges.
    pub(super) fn live_list_offset(
        &self,
        view: Viewport,
        selected: Option<usize>,
        count: usize,
        rows: usize,
    ) -> usize {
        self.record_live_viewport(view, rows);
        let rows = rows.max(1);
        let saved = &self.live_state.offsets[view as usize];
        let mut offset = saved.get().min(count.saturating_sub(rows));
        if let Some(selected) = selected.filter(|&index| index < count) {
            if selected < offset {
                offset = selected;
            } else if selected >= offset.saturating_add(rows) {
                offset = selected + 1 - rows;
            }
        }
        saved.set(offset);
        offset
    }
    fn overview_group_cursor(&self) -> Option<usize> {
        let count = self.connection_rows().len();
        (self.overview_cursor >= count && self.overview_cursor < self.overview_item_count.get())
            .then(|| self.overview_cursor - count)
    }
    fn sync_overview_cursor(&mut self, group: Option<usize>) {
        if let Some(index) = self.selected_connection_row_index() {
            self.connection_selection = index;
        }
        if self.client.page == OVERVIEW {
            self.overview_cursor = group.map_or(self.connection_selection, |group| {
                self.connection_rows().len().saturating_add(group)
            });
        }
    }
    fn track_new_connections(&mut self, snapshot: &Snapshot) {
        let Some(previous) = &self.client.runtime_snapshot else {
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
    pub(super) fn next_connection_expiry(&self) -> Option<Instant> {
        self.live_state.fresh_connections.values().copied().min()
    }
    pub(super) fn expire_connection_highlights(&mut self) -> bool {
        let group = self.overview_group_cursor();
        let now = Instant::now();
        let count = self.live_state.fresh_connections.len();
        self.live_state
            .fresh_connections
            .retain(|_, until| *until > now);
        let changed = count != self.live_state.fresh_connections.len();
        if changed {
            self.sync_overview_cursor(group);
        }
        changed
    }
    fn disabled_live_alias(&self, key: event::KeyEvent) -> bool {
        !self.vim_keys
            && !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            && matches!(key.code, KeyCode::Char('h' | 'j' | 'k' | 'l' | 'g' | 'G'))
    }
    pub(super) fn live_input(&self) -> bool {
        self.editor.is_none()
            && self.client.viewer.is_none()
            && self.prompt.is_none()
            && self.recovery.is_none()
            && self.menu.is_none()
            && self.binding_picker.is_none()
            && self.client.diagnostics.is_none()
            && self.client.explanation.is_none()
            && self.client.help_scroll.is_none()
    }
    /// Runtime data of the active profile; stale data from an older process is hidden.
    pub(super) fn live(&self) -> Option<&Snapshot> {
        let row = self.active_proxy()?;
        self.client
            .runtime_snapshot
            .as_ref()
            .filter(|s| self.client.runtime_instance == row.instance.name && Some(s.pid) == row.pid)
    }
    pub(super) fn accept_runtime(&mut self, name: String, snapshot: Snapshot) {
        self.accept_runtime_view(
            name,
            RuntimeView {
                snapshot,
                telemetry: None,
            },
        );
    }
    /// A transport reconnect preserves the view; a different profile or host PID does not.
    pub(super) fn reset_live_identity(&mut self) {
        self.live_state = LiveState::default();
        self.client.runtime_instance.clear();
        self.client.runtime_snapshot = None;
        self.client.runtime_sample = Instant::now();
        self.client.runtime_error.clear();
        self.client.history = Default::default();
        self.client.rates = [0, 0];
        self.client.direct_rates = [0, 0];
        self.connection_selection = 0;
        self.overview_cursor = 0;
    }
    pub(super) fn accept_runtime_view(&mut self, name: String, view: RuntimeView) {
        let RuntimeView {
            snapshot,
            telemetry,
        } = view;
        let identity_changed = self.client.runtime_instance != name
            || self
                .client
                .runtime_snapshot
                .as_ref()
                .is_some_and(|old| old.pid != snapshot.pid)
            || telemetry
                .as_ref()
                .zip(self.live_state.telemetry.as_ref())
                .is_some_and(|(new, old)| new.host_boot_id != old.host_boot_id);
        if identity_changed {
            if self.client.runtime_snapshot.is_some() && self.client.runtime_instance == name {
                self.notice_host_restart();
            }
            self.reset_live_identity();
        }
        // Command replies without source metadata must not replace a hosted source clock.
        if telemetry.is_none() && self.live_state.telemetry.is_some() && !identity_changed {
            self.track_new_connections(&snapshot);
            self.client.runtime_instance = name;
            self.client.runtime_snapshot = Some(snapshot);
            self.sync_overview_cursor(None);
            return;
        }
        if let Some(meta) = telemetry {
            if self
                .live_state
                .baseline
                .as_ref()
                .is_some_and(|(_, old)| meta.sample_sequence < old.sample_sequence)
            {
                return;
            }
            let valid = meta.coverage == Coverage::Complete && meta.sample_age_ms <= 3000;
            let fresh = self
                .live_state
                .baseline
                .as_ref()
                .is_none_or(|(_, old)| old.sample_sequence != meta.sample_sequence);
            if fresh {
                self.live_state.rates_valid = false;
                let previous = self
                    .live_state
                    .baseline
                    .as_ref()
                    .filter(|(_, old)| {
                        old.host_boot_id == meta.host_boot_id && old.coverage == Coverage::Complete
                    })
                    .cloned();
                if let Some((old, old_meta)) = previous.filter(|_| valid) {
                    let elapsed = meta
                        .sample_elapsed_ms
                        .saturating_sub(old_meta.sample_elapsed_ms)
                        as f64
                        / 1000.0;
                    if elapsed > 0.0 {
                        self.set_runtime_rates(&old, &snapshot, elapsed);
                        self.live_state.rates_valid = true;
                        self.record_runtime_rates();
                    }
                } else {
                    self.live_state.rates_valid = false;
                }
                self.live_state.baseline = Some((snapshot.clone(), meta.clone()));
            }
            if !valid {
                self.live_state.rates_valid = false;
            }
            self.live_state.telemetry = Some(meta);
        } else {
            self.accept_legacy_rates(&name, &snapshot);
        }
        self.live_state.received = Some(Instant::now());
        self.live_state.disconnected = false;
        self.client.runtime_sample = Instant::now();
        let overview_group = self.overview_group_cursor();
        self.track_new_connections(&snapshot);
        self.client.runtime_instance = name;
        self.client.runtime_snapshot = Some(snapshot);
        self.group_selection = self
            .group_selection
            .min(self.live().map_or(0, |s| s.groups.len().saturating_sub(1)));
        if self.live_state.selected.is_none() && self.live_state.selected_target.is_none() {
            self.select_connection(0);
        }
        self.sync_overview_cursor(overview_group);
        self.client.runtime_error.clear();
    }
    fn accept_legacy_rates(&mut self, name: &str, snapshot: &Snapshot) {
        self.client.accept_legacy_rates(name, snapshot);
        self.live_state.rates_valid = self.client.runtime_rates_valid;
    }
    pub(super) fn runtime_disconnected(&mut self, error: String) {
        self.live_state.disconnected = true;
        self.live_state.rates_valid = false;
        self.live_state.baseline = None;
        self.client.runtime_error = error;
    }
    pub(super) fn runtime_rates_available(&self) -> bool {
        self.live().is_some()
            && self.live_state.rates_valid
            && !self.live_state.disconnected
            && self
                .live_state
                .received
                .is_some_and(|at| at.elapsed() <= Duration::from_secs(3))
            && self.live_state.telemetry.as_ref().is_none_or(|meta| {
                meta.sample_age_ms.saturating_add(
                    self.live_state
                        .received
                        .map_or(0, |at| at.elapsed().as_millis() as u64),
                ) <= 3000
            })
    }
    pub(super) fn runtime_health(&self) -> String {
        if self.live_state.disconnected {
            return t("Reconnecting · last received data").into();
        }
        if self
            .live_state
            .received
            .is_some_and(|at| at.elapsed() > Duration::from_secs(3))
        {
            return t("Runtime data stale").into();
        }
        match &self.live_state.telemetry {
            Some(meta) if meta.ingress_paused => t("Update: new connections paused").into(),
            Some(meta) if meta.coverage == Coverage::Partial => {
                t("Partial worker telemetry").into()
            }
            Some(meta)
                if matches!(meta.coverage, Coverage::Stale | Coverage::Unknown)
                    || meta.sample_age_ms.saturating_add(
                        self.live_state
                            .received
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
    pub(super) fn connections_paused(&self) -> bool {
        self.live_state.frozen.is_some()
    }
    pub(super) fn live_detail_open(&self) -> bool {
        self.live_state.detail_open
    }
    pub(super) fn connection_badges(&self) -> String {
        let mut badges = Vec::new();
        let paused = tr!(
            "PAUSED {}",
            self.live_state.frozen_at.as_deref().unwrap_or("—")
        );
        if self.connections_paused() {
            badges.push(paused.as_str());
        }
        if self.live_state.disconnected {
            badges.push(t("RECONNECTING"));
        } else if self
            .live_state
            .received
            .is_some_and(|at| at.elapsed() > Duration::from_secs(3))
        {
            badges.push(t("STALE"));
        } else if let Some(meta) = &self.live_state.telemetry {
            if meta.ingress_paused {
                badges.push(t("INGRESS PAUSED"));
            }
            if matches!(meta.coverage, Coverage::Stale | Coverage::Unknown)
                || meta.sample_age_ms.saturating_add(
                    self.live_state
                        .received
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
    pub(super) fn connection_pause_hint(&self) -> Option<String> {
        self.connections_paused().then(|| {
            tr!(
                "Paused {} · background updates continue · Space resume",
                self.live_state.frozen_at.as_deref().unwrap_or("—")
            )
        })
    }
    pub(super) fn toggle_live_pause(&mut self) -> Input {
        let overview_group = self.overview_group_cursor();
        if self.live_state.frozen.take().is_none() {
            self.live_state.frozen = self.live().cloned();
            self.live_state.frozen_telemetry = self.live_state.telemetry.clone();
            self.live_state.frozen_at = self
                .live_state
                .frozen
                .as_ref()
                .map(|_| chrono::Local::now().format("%H:%M:%S").to_string());
        } else {
            self.live_state.frozen_telemetry = None;
            self.live_state.frozen_at = None;
        }
        self.sync_overview_cursor(overview_group);
        Input::Idle
    }
    pub(super) fn connection_snapshot(&self) -> Option<&Snapshot> {
        self.live()?;
        self.live_state.frozen.as_ref().or_else(|| self.live())
    }
    pub(super) fn connection_rows(&self) -> Vec<ConnectionRow> {
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
    pub(super) fn selected_connection_row_key(&self) -> Option<ConnectionRowKey> {
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
    fn selected_connection_row_index(&self) -> Option<usize> {
        let key = self.selected_connection_row_key()?;
        self.connection_rows().iter().position(|r| r.key == key)
    }
    pub(super) fn select_connection_row_key(&mut self, key: ConnectionRowKey) {
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
        self.sync_overview_cursor(None);
    }
    pub(super) fn select_connection(&mut self, index: usize) {
        if let Some(key) = self.connection_rows().get(index).map(|r| r.key.clone()) {
            self.select_connection_row_key(key);
        } else {
            self.live_state.selected = None;
            self.live_state.selected_target = None;
            self.connection_selection = 0;
        }
    }
    pub(super) fn select_connection_id(&mut self, id: u64) {
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
    pub(super) fn reveal_selected_connection(&mut self) {
        if let Some(id) = self.live_state.selected {
            if !self.visible_connections().iter().any(|c| c.id == id)
                && self
                    .connection_snapshot()
                    .is_some_and(|s| s.connections.iter().any(|c| c.id == id))
            {
                self.connection_filter.clear();
            }
        }
        self.sync_overview_cursor(None);
    }
    #[cfg(test)]
    pub(super) fn connection_is_selected(&self, id: u64) -> bool {
        self.live_state.selected == Some(id)
    }
    fn selected_connection(&self) -> Option<&astra_model::runtime::Connection> {
        let ConnectionRowKey::Connection(id) = self.selected_connection_row_key()? else {
            return None;
        };
        self.visible_connections().into_iter().find(|c| c.id == id)
    }
    pub(super) fn toggle_connection_group(&mut self) -> bool {
        let Some(ConnectionRowKey::Target(target)) = self.selected_connection_row_key() else {
            return false;
        };
        if !self.live_state.expanded_targets.remove(&target) {
            self.live_state.expanded_targets.insert(target.clone());
        }
        self.select_connection_row_key(ConnectionRowKey::Target(target));
        true
    }
    fn expand_connection_group(&mut self) {
        if let Some(ConnectionRowKey::Target(target)) = self.selected_connection_row_key() {
            self.live_state.expanded_targets.insert(target);
            self.sync_overview_cursor(None);
        }
    }
    fn collapse_connection_group(&mut self) {
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
    pub(super) fn runtime_action(&mut self, command: Command) -> Input {
        if matches!(command, Command::Tun(true)) {
            let elevation = self.active_proxy().and_then(|row| {
                let name = row.instance.name.clone();
                let pid = row.pid;
                let supported = self
                    .client
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
    pub(super) fn open_runtime_group(&mut self) {
        if let Some(group) = self.live().and_then(|s| s.groups.get(self.group_selection)) {
            let selected = group
                .members
                .iter()
                .position(|m| Some(&m.name) == group.selected.as_ref())
                .unwrap_or(0);
            self.runtime_picker = Some((group.name.clone(), selected));
            self.focus.region = focus::Region::Secondary;
        }
    }
    pub(super) fn choose_runtime_member(&mut self, index: usize) -> Input {
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
            self.client.message = t("Automatic groups choose members by latency").into();
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
    pub(super) fn visible_connections(&self) -> Vec<&astra_model::runtime::Connection> {
        let filter = if self.client.page == CONNECTIONS {
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
    pub(super) fn live_secondary_key(&mut self, key: event::KeyEvent) -> Option<Input> {
        if self.disabled_live_alias(key) {
            return Some(Input::Idle);
        }
        if self.client.page == OVERVIEW {
            let count = self.live()?.groups.len();
            let page = self.live_page_size(Viewport::OverviewGroups);
            match key.code {
                KeyCode::Down | KeyCode::Char('j') => {
                    self.group_selection = (self.group_selection + 1).min(count.saturating_sub(1))
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.group_selection = self.group_selection.saturating_sub(1)
                }
                KeyCode::PageDown => {
                    self.group_selection =
                        (self.group_selection + page).min(count.saturating_sub(1))
                }
                KeyCode::PageUp => self.group_selection = self.group_selection.saturating_sub(page),
                KeyCode::Home => self.group_selection = 0,
                KeyCode::End => self.group_selection = count.saturating_sub(1),
                KeyCode::Enter => {
                    self.navigate(PROXIES);
                    self.open_runtime_group();
                }
                _ => return None,
            }
        } else if self.client.page == CONNECTIONS {
            let page = self
                .live_page_size(Viewport::Details)
                .min(u16::MAX as usize) as u16;
            match key.code {
                KeyCode::Down | KeyCode::Char('j') => {
                    self.live_state.detail_scroll = self.live_state.detail_scroll.saturating_add(1)
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.live_state.detail_scroll = self.live_state.detail_scroll.saturating_sub(1)
                }
                KeyCode::PageDown => {
                    self.live_state.detail_scroll =
                        self.live_state.detail_scroll.saturating_add(page)
                }
                KeyCode::PageUp => {
                    self.live_state.detail_scroll =
                        self.live_state.detail_scroll.saturating_sub(page)
                }
                KeyCode::Home => self.live_state.detail_scroll = 0,
                KeyCode::Enter => {
                    self.live_state.detail_open = true;
                    self.animations.open(animation::Kind::Detail);
                }
                _ => return None,
            }
        } else {
            return None;
        }
        Some(Input::Idle)
    }
    pub(super) fn live_key(&mut self, key: event::KeyEvent) -> Option<Input> {
        if !self.connection_filtering && self.disabled_live_alias(key) {
            return Some(Input::Idle);
        }
        if self.live_state.detail_open && self.client.page == CONNECTIONS {
            match key.code {
                KeyCode::Esc | KeyCode::Enter => {
                    self.live_state.detail_open = false;
                    self.animations.close(animation::Kind::Detail);
                    return Some(Input::Idle);
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    self.live_state.detail_scroll = self.live_state.detail_scroll.saturating_add(1);
                    return Some(Input::Idle);
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.live_state.detail_scroll = self.live_state.detail_scroll.saturating_sub(1);
                    return Some(Input::Idle);
                }
                KeyCode::PageDown | KeyCode::PageUp | KeyCode::Home => {
                    return self.live_secondary_key(key)
                }
                _ => {}
            }
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return None;
        }
        if self.runtime_picker.is_some()
            && (key.code == KeyCode::Char('/')
                || (key.code == KeyCode::Char('f')
                    && key.modifiers.contains(KeyModifiers::CONTROL)))
        {
            self.prompt(Prompt::Find(PROXIES));
            return Some(Input::Idle);
        }
        if self.runtime_picker.is_some()
            && key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return Some(Input::Idle);
        }
        if let Some((name, index)) = self.runtime_picker.clone() {
            let page = self.live_page_size(Viewport::Members);
            let count = self
                .live()
                .and_then(|s| s.groups.iter().find(|g| g.name == name))
                .map_or(0, |g| g.members.len());
            let mut next = index;
            match key.code {
                KeyCode::Esc | KeyCode::Left | KeyCode::Char('h') => self.runtime_picker = None,
                KeyCode::Down | KeyCode::Char('j') => {
                    next = (index + 1).min(count.saturating_sub(1))
                }
                KeyCode::Up | KeyCode::Char('k') => next = index.saturating_sub(1),
                KeyCode::PageDown => next = (index + page).min(count.saturating_sub(1)),
                KeyCode::PageUp => next = index.saturating_sub(page),
                KeyCode::Home => next = 0,
                KeyCode::End => next = count.saturating_sub(1),
                KeyCode::Enter | KeyCode::Char(' ') => {
                    return Some(self.choose_runtime_member(index))
                }
                KeyCode::Char('t') => return Some(self.runtime_action(Command::TestGroup(name))),
                _ => return Some(Input::Idle),
            }
            if let Some((_, cursor)) = self.runtime_picker.as_mut() {
                *cursor = next;
            }
            return Some(Input::Idle);
        }
        if self.connection_filtering {
            match key.code {
                KeyCode::Esc | KeyCode::Enter => self.connection_filtering = false,
                KeyCode::Backspace => {
                    self.connection_filter.pop();
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.connection_filter.push(c)
                }
                _ => return Some(Input::Idle),
            }
            self.select_connection(0);
            return Some(Input::Idle);
        }
        // Modified shortcuts must never become unmodified network mutations.
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            if matches!(self.client.page, PROXIES | CONNECTIONS)
                && key.modifiers.contains(KeyModifiers::CONTROL)
            {
                match key.code {
                    KeyCode::Char('d') => {
                        return self
                            .live_key(event::KeyEvent::new(KeyCode::PageDown, KeyModifiers::NONE))
                    }
                    KeyCode::Char('u') => {
                        return self
                            .live_key(event::KeyEvent::new(KeyCode::PageUp, KeyModifiers::NONE))
                    }
                    _ => {}
                }
            }
            return None;
        }
        // List keys on runtime pages never fall through to other handlers.
        if self.live().is_none() && matches!(self.client.page, PROXIES | CONNECTIONS) {
            return match key.code {
                KeyCode::Up
                | KeyCode::Down
                | KeyCode::Home
                | KeyCode::End
                | KeyCode::PageUp
                | KeyCode::PageDown
                | KeyCode::Enter
                | KeyCode::Delete
                | KeyCode::Char('j' | 'k' | 'g' | 'G' | 'x' | 'X' | 't' | '/') => Some(Input::Idle),
                _ => None,
            };
        }
        let snapshot = self.live()?.clone();
        if matches!(self.client.page, OVERVIEW | CONNECTIONS) {
            let group = self.overview_group_cursor();
            self.sync_overview_cursor(group);
            match key.code {
                KeyCode::Right => {
                    self.expand_connection_group();
                    return Some(Input::Idle);
                }
                KeyCode::Left => {
                    self.collapse_connection_group();
                    return Some(Input::Idle);
                }
                KeyCode::Enter if self.toggle_connection_group() => return Some(Input::Idle),
                _ => {}
            }
        }
        if self.client.page == OVERVIEW {
            let connection_count = self.connection_rows().len();
            let page = self.live_page_size(Viewport::Overview);
            let count = self.overview_item_count.get().max(connection_count);
            // The narrow overview continues through the group panel after the connections.
            let current = if self.overview_cursor < connection_count {
                self.connection_selection
            } else {
                self.overview_cursor
            }
            .min(count.saturating_sub(1));
            let next = match key.code {
                KeyCode::Char('j') | KeyCode::Down => Some(current.saturating_add(1)),
                KeyCode::Char('k') | KeyCode::Up => Some(current.saturating_sub(1)),
                KeyCode::PageDown => Some(current.saturating_add(page)),
                KeyCode::PageUp => Some(current.saturating_sub(page)),
                KeyCode::Home | KeyCode::Char('g') => Some(0),
                KeyCode::End | KeyCode::Char('G') => Some(count.saturating_sub(1)),
                _ => None,
            };
            if let Some(next) = next {
                let next = next.min(count.saturating_sub(1));
                self.select_connection(next.min(connection_count.saturating_sub(1)));
                self.overview_cursor = next;
                if next == 0 {
                    self.live_state.offsets[Viewport::OverviewGroups as usize].set(0);
                }
                return Some(Input::Idle);
            }
            match key.code {
                KeyCode::Char('z') => {
                    self.traffic_expanded = !self.traffic_expanded;
                    return Some(Input::Idle);
                }
                KeyCode::Enter => return Some(self.navigate(CONNECTIONS)),
                KeyCode::Char(' ') => return Some(self.toggle_live_pause()),
                _ => {}
            }
        }
        match key.code {
            KeyCode::Char('m') => {
                return Some(self.runtime_action(Command::SetMode(match snapshot.mode {
                    Mode::Rule => Mode::Global,
                    Mode::Global => Mode::Direct,
                    Mode::Direct => Mode::Rule,
                })))
            }
            KeyCode::Char('S') => {
                return Some(self.runtime_action(Command::SystemProxy(!snapshot.system_proxy)))
            }
            KeyCode::Char('A') => {
                return Some(self.runtime_action(Command::AllowLan(!snapshot.allow_lan)))
            }
            KeyCode::Char('T') => return Some(self.runtime_action(Command::Tun(!snapshot.tun))),
            _ => {}
        }
        if self.client.page == PROXIES {
            let count = snapshot.groups.len();
            let page = self.live_page_size(Viewport::Groups);
            match key.code {
                KeyCode::Down | KeyCode::Char('j') => {
                    self.group_selection = (self.group_selection + 1).min(count.saturating_sub(1))
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.group_selection = self.group_selection.saturating_sub(1)
                }
                KeyCode::Home | KeyCode::Char('g') => self.group_selection = 0,
                KeyCode::End | KeyCode::Char('G') => self.group_selection = count.saturating_sub(1),
                KeyCode::PageDown => {
                    self.group_selection =
                        (self.group_selection + page).min(count.saturating_sub(1))
                }
                KeyCode::PageUp => self.group_selection = self.group_selection.saturating_sub(page),
                KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => self.open_runtime_group(),
                KeyCode::Char('t') => {
                    return Some(self.runtime_action(Command::TestGroup(
                        snapshot.groups.get(self.group_selection)?.name.clone(),
                    )))
                }
                _ => return None,
            }
            return Some(Input::Idle);
        }
        if self.client.page == CONNECTIONS {
            let page = self.live_page_size(Viewport::Connections);
            let count = self.connection_rows().len();
            match key.code {
                KeyCode::Char('/') => self.connection_filtering = true,
                KeyCode::Esc => self.connection_filter.clear(),
                KeyCode::Down | KeyCode::Char('j') => self.select_connection(
                    (self.connection_selection + 1).min(count.saturating_sub(1)),
                ),
                KeyCode::Up | KeyCode::Char('k') => {
                    self.select_connection(self.connection_selection.saturating_sub(1))
                }
                KeyCode::Home | KeyCode::Char('g') => self.select_connection(0),
                KeyCode::End | KeyCode::Char('G') => {
                    self.select_connection(count.saturating_sub(1))
                }
                KeyCode::PageDown => self.select_connection(
                    (self.connection_selection + page).min(count.saturating_sub(1)),
                ),
                KeyCode::PageUp => {
                    self.select_connection(self.connection_selection.saturating_sub(page))
                }
                KeyCode::Enter => {
                    self.live_state.detail_open = true;
                    self.live_state.detail_scroll = 0;
                    self.animations.open(animation::Kind::Detail);
                    return Some(Input::Idle);
                }
                KeyCode::Char('x') | KeyCode::Delete => {
                    let Some(id) = self.selected_connection().map(|c| c.id) else {
                        self.client.message = if matches!(
                            self.selected_connection_row_key(),
                            Some(ConnectionRowKey::Target(_))
                        ) {
                            t("Expand the group to close an individual connection").into()
                        } else {
                            t("Selected connection is not present in this sample").into()
                        };
                        return Some(Input::Idle);
                    };
                    if self.live_state.disconnected
                        || !self
                            .live()
                            .is_some_and(|s| s.connections.iter().any(|c| c.id == id))
                    {
                        self.client.message =
                            "Connection cannot be verified in the latest sample".into();
                        return Some(Input::Idle);
                    }
                    return Some(self.runtime_action(Command::Disconnect(Some(id))));
                }
                KeyCode::Char(' ') => return Some(self.toggle_live_pause()),
                KeyCode::Char('X') => {
                    if count > 0 {
                        let row = self.active_proxy()?;
                        let (name, pid) = (row.instance.name.clone(), row.pid);
                        self.prompt(Prompt::CloseAll { name, pid });
                    }
                }
                _ => return None,
            }
            return Some(Input::Idle);
        }
        None
    }

    fn runtime_control_items(&self, compact: bool) -> Vec<(Line<'static>, Command)> {
        let Some(snapshot) = self.live() else {
            return Vec::new();
        };
        let mut items = Vec::new();
        for mode in [Mode::Rule, Mode::Global, Mode::Direct] {
            let label = t(match mode {
                Mode::Rule => "Rule",
                Mode::Global => "Global",
                Mode::Direct => "Direct",
            });
            let style = if snapshot.mode == mode {
                Style::default()
                    .fg(Color::White)
                    .bg(BLUE)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(MUTED)
            };
            items.push((
                Line::styled(format!(" m {label} "), style),
                Command::SetMode(mode),
            ));
        }
        let switches = [
            (
                'S',
                if compact {
                    t("proxy")
                } else {
                    t("System proxy")
                },
                snapshot.system_proxy,
                Command::SystemProxy(!snapshot.system_proxy),
            ),
            (
                'T',
                if compact { t("TUN") } else { t("TUN · admin") },
                snapshot.tun,
                Command::Tun(!snapshot.tun),
            ),
            (
                'A',
                if compact { t("LAN") } else { t("Allow LAN") },
                snapshot.allow_lan,
                Command::AllowLan(!snapshot.allow_lan),
            ),
        ];
        for (key, label, enabled, command) in switches {
            items.push((
                Line::from(vec![
                    Span::raw(format!(" {key} {label} ")),
                    Span::styled(
                        if enabled { "●" } else { "○" },
                        Style::default().fg(if enabled { Color::Green } else { MUTED }),
                    ),
                    Span::raw(" "),
                ])
                .style(if enabled {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(MUTED)
                }),
                command,
            ));
        }
        items
    }

    fn runtime_control_actions(&self) -> Vec<(&'static str, Target)> {
        let Some(row) = self.active_proxy() else {
            return Vec::new();
        };
        vec![
            (t("N Network tools"), Target::NetTools),
            (
                t(if row.pid.is_some() {
                    "c Disconnect"
                } else {
                    "c Connect"
                }),
                Target::Connect,
            ),
        ]
    }

    pub(super) fn runtime_controls_height(&self, width: u16) -> u16 {
        if self.active_proxy().is_none() {
            return 0;
        }
        let inner_width = width.saturating_sub(2) as usize;
        let items = self.runtime_control_items(inner_width < 90);
        let controls_width = items.iter().map(|(line, _)| line.width()).sum::<usize>()
            + items.len().saturating_sub(1);
        let actions = self.runtime_control_actions();
        let actions_width = actions
            .iter()
            .map(|(label, _)| Line::from(*label).width() + 2)
            .sum::<usize>()
            + actions.len().saturating_sub(1);
        if !items.is_empty() && controls_width + 2 + actions_width > inner_width {
            4
        } else {
            3
        }
    }

    /// Mode switches stay left; profile actions occupy the right edge of Controls.
    pub(super) fn render_runtime_modes(&self, f: &mut Frame<'_>, area: Rect) {
        if self.active_proxy().is_none() || area.width < 4 || area.height < 3 {
            return;
        }
        let block = self.focused_card("CONTROLS", focus::Region::Controls);
        let inner = block.inner(area);
        f.render_widget(block, area);
        if inner.is_empty() {
            return;
        }
        let items = self.runtime_control_items(inner.width < 90);
        let action_index = items.len();
        let actions = self.runtime_control_actions();
        let second_row = self.runtime_controls_height(area.width) == 4 && inner.height > 1;
        let action_y = inner.y + u16::from(second_row);
        let mut action_left = inner.right();
        for (index, (label, target)) in actions.into_iter().enumerate().rev() {
            let width = (Line::from(label).width() + 2) as u16;
            if action_left.saturating_sub(inner.x) < width {
                continue;
            }
            let rect = Rect::new(action_left - width, action_y, width, 1);
            let mut style = Style::default().bg(BLUE).fg(Color::White);
            if self.focus.region == focus::Region::Controls
                && self.focus.cursor == action_index + index
            {
                style = style.add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
            }
            f.render_widget(Paragraph::new(format!(" {label} ")).style(style), rect);
            self.hit(rect, target);
            action_left = rect.x.saturating_sub(1).max(inner.x);
        }
        let controls_right = if second_row {
            inner.right()
        } else {
            action_left.saturating_sub(1).max(inner.x)
        };
        let mut x = inner.x;
        for (index, (mut line, command)) in items.into_iter().enumerate() {
            let width = line.width() as u16;
            if x.saturating_add(width) > controls_right {
                break;
            }
            if self.focus.region == focus::Region::Controls && self.focus.cursor == index {
                line.style = line
                    .style
                    .add_modifier(Modifier::BOLD | Modifier::UNDERLINED);
            }
            let rect = Rect::new(x, inner.y, width, 1);
            f.render_widget(Paragraph::new(line), rect);
            self.hit(rect, Target::Runtime(command));
            x = x.saturating_add(width + 1);
        }
    }

    fn render_not_live(&self, f: &mut Frame<'_>, area: Rect, title: &str) {
        let text = match self.active_proxy() {
            None => t("No profile yet · 6 then n to create one").to_owned(),
            Some(row) if row.pid.is_none() => {
                tr!("{} is not connected · c connect", row.instance.name)
            }
            Some(_) => format!(
                "{}\n{}",
                t("Waiting for runtime data…"),
                self.client.runtime_error
            ),
        };
        f.render_widget(
            Paragraph::new(text)
                .wrap(Wrap { trim: false })
                .style(Style::default().fg(MUTED))
                .block(card(title)),
            area,
        );
    }

    /// Groups on the left, members of the focused group on the right.
    pub(super) fn render_live_groups(&self, f: &mut Frame<'_>, area: Rect) {
        self.record_live_viewport(Viewport::Groups, 0);
        self.record_live_viewport(Viewport::Members, 0);
        let Some(snapshot) = self.live() else {
            self.render_not_live(f, area, "Proxy groups");
            return;
        };
        let wide = area.width >= 80;
        let columns = if wide {
            Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)]).split(area)
        } else if self.runtime_picker.is_some() {
            Layout::horizontal([Constraint::Length(0), Constraint::Min(0)]).split(area)
        } else {
            Layout::horizontal([Constraint::Min(0), Constraint::Length(0)]).split(area)
        };
        let focus_members = self.runtime_picker.is_some();
        let items: Vec<_> = snapshot
            .groups
            .iter()
            .map(|group| {
                ListItem::new(vec![
                    Line::from(vec![
                        Span::styled(
                            format!(" {}", group.name),
                            Style::default().add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(
                            format!(
                                "  {}{}",
                                t(if group.mode == "select" {
                                    "manual"
                                } else {
                                    "auto"
                                }),
                                if group.testing {
                                    format!(" · {}", t("Testing…"))
                                } else {
                                    String::new()
                                }
                            ),
                            Style::default().fg(MUTED),
                        ),
                    ]),
                    Line::styled(
                        format!("   → {}", group.selected.as_deref().unwrap_or("—")),
                        Style::default().fg(MUTED),
                    ),
                ])
            })
            .collect();
        let mut state = ListState::default().with_selected(
            (!items.is_empty()).then_some(self.group_selection.min(items.len().saturating_sub(1))),
        );
        let groups_area = columns[0];
        self.record_live_viewport(
            Viewport::Groups,
            if groups_area.width > 0 {
                groups_area.height.saturating_sub(2) as usize / 2
            } else {
                0
            },
        );
        if groups_area.width > 0 {
            *state.offset_mut() = self.live_list_offset(
                Viewport::Groups,
                state.selected(),
                snapshot.groups.len(),
                groups_area.height.saturating_sub(2) as usize / 2,
            );
            f.render_stateful_widget(
                List::new(items)
                    .block(card("Proxy groups").border_style(self.focus_border(
                        focus::Region::Main,
                        if focus_members { Color::DarkGray } else { BLUE },
                    )))
                    .highlight_style(if focus_members {
                        crate::theme::selected_inactive()
                    } else {
                        crate::theme::selected()
                    })
                    .highlight_symbol("› "),
                groups_area,
                &mut state,
            );
            let rows = astra_tui_kit::RenderedRows::fixed(
                card("").inner(groups_area),
                state.offset(),
                snapshot.groups.len(),
                2,
            );
            self.record_live_viewport(Viewport::Groups, rows.visible_capacity);
            self.hits.borrow_mut().bind_rows(&rows, Target::Group);
        }
        let members_area = columns[1];
        self.record_live_viewport(
            Viewport::Members,
            if members_area.width > 0 {
                members_area.height.saturating_sub(2) as usize
            } else {
                0
            },
        );
        let Some(group) = snapshot.groups.get(self.group_selection) else {
            return;
        };
        if members_area.width == 0 {
            return;
        }
        let cursor = self
            .runtime_picker
            .as_ref()
            .filter(|(name, _)| name == &group.name)
            .map(|(_, index)| *index);
        let items: Vec<_> = group
            .members
            .iter()
            .map(|member| {
                let chosen = group.selected.as_ref() == Some(&member.name);
                ListItem::new(Line::from(vec![
                    Span::styled(
                        if chosen { " ● " } else { " ○ " },
                        Style::default().fg(if chosen { BLUE } else { Color::DarkGray }),
                    ),
                    Span::styled(
                        format!("{:<28}", member.name),
                        if chosen {
                            Style::default().add_modifier(Modifier::BOLD)
                        } else {
                            Style::default()
                        },
                    ),
                    delay_span(member.delay_ms, member.alive),
                ]))
            })
            .collect();
        let mut state = ListState::default()
            .with_selected(cursor.map(|c| c.min(items.len().saturating_sub(1))));
        *state.offset_mut() = self.live_list_offset(
            Viewport::Members,
            state.selected(),
            group.members.len(),
            members_area.height.saturating_sub(2) as usize,
        );
        let title = format!(
            "{} · {}",
            group.name,
            t(if group.mode == "select" {
                "Enter selects"
            } else {
                "chosen by latency"
            })
        );
        f.render_stateful_widget(
            List::new(items)
                .block(card(&title).border_style(self.focus_border(
                    focus::Region::Secondary,
                    if focus_members { BLUE } else { Color::DarkGray },
                )))
                .highlight_style(crate::theme::selected())
                .highlight_symbol("›"),
            members_area,
            &mut state,
        );
        if members_area.width >= 24 {
            self.button(
                f,
                Rect::new(members_area.right() - 10, members_area.y, 8, 1),
                "t Test",
                Target::Runtime(Command::TestGroup(group.name.clone())),
            );
        }
        let rows = astra_tui_kit::RenderedRows::fixed(
            card("").inner(members_area),
            state.offset(),
            group.members.len(),
            1,
        );
        self.record_live_viewport(Viewport::Members, rows.visible_capacity);
        self.hits.borrow_mut().bind_rows(&rows, Target::Member);
    }

    /// Connection table with a detail pane for the selected row.
    pub(super) fn render_connections(&self, f: &mut Frame<'_>, area: Rect) {
        self.record_live_viewport(Viewport::Connections, 0);
        self.record_live_viewport(Viewport::Details, 0);
        let Some(snapshot) = self.connection_snapshot() else {
            self.render_not_live(f, area, "Connections");
            return;
        };
        let connections = self.connection_rows();
        let selected_key = self.selected_connection_row_key();
        let selected_index = connections
            .iter()
            .position(|r| Some(&r.key) == selected_key.as_ref());
        let selected_row = selected_index.and_then(|index| connections.get(index));
        let selected = selected_row.map(|row| &row.connection);
        if self.live_state.detail_open {
            self.record_live_viewport(Viewport::Details, area.height.saturating_sub(2) as usize);
            let before = f.buffer_mut().clone();
            let mut text = selected.map(|c| format!("Target: {}:{}\nExit: {}\nNetwork: {} · {}\nSource: {}\nUpload: {}\nDownload: {}\nElapsed: {:.1}s\n{}",
                c.host,c.port,c.outbound,c.network,c.inbound,c.source.as_deref().unwrap_or("—"),bytes(c.upload),bytes(c.download),c.elapsed_ms as f64/1000.0,
                self.live_state.frozen_telemetry.as_ref().or(self.live_state.telemetry.as_ref()).and_then(|m| m.connection_generations.get(&c.id)).map_or(String::new(),|generation|format!("Worker generation: {generation}"))))
                .unwrap_or_else(|| "Selected connection is not present in this sample".into());
            if let Some(hint) = self.connection_pause_hint() {
                text.push_str(&format!("\n{hint}"));
            }
            text.push_str(&format!("\n{}", self.runtime_health()));
            f.render_widget(
                Paragraph::new(text)
                    .wrap(Wrap { trim: false })
                    .scroll((self.live_state.detail_scroll, 0))
                    .block(card("Connection details · ↑↓ scroll · Esc back")),
                area,
            );
            self.animations
                .surface(f, animation::Kind::Detail, area, &before);
            return;
        }
        if area.width < 80 {
            self.record_live_viewport(
                Viewport::Connections,
                area.height.saturating_sub(2) as usize / 2,
            );
            let items: Vec<_> = connections
                .iter()
                .map(|row| {
                    let c = &row.connection;
                    ListItem::new(vec![
                        Line::raw(row.label()),
                        Line::raw(format!(
                            "{} · {} · ↑{} ↓{}",
                            c.outbound,
                            c.network,
                            bytes(c.upload),
                            bytes(c.download)
                        )),
                    ])
                    .style(crate::theme::connection_row_style(
                        Some(&row.key) == selected_key.as_ref(),
                        row.fresh,
                    ))
                })
                .collect();
            let title = format!(
                "{}Connections {}/{}{}",
                self.connection_badges(),
                self.visible_connections().len(),
                snapshot.active,
                if snapshot.truncated {
                    " · partial"
                } else {
                    ""
                }
            );
            let mut state = ListState::default().with_selected(selected_index);
            *state.offset_mut() = self.live_list_offset(
                Viewport::Connections,
                state.selected(),
                connections.len(),
                area.height.saturating_sub(2) as usize / 2,
            );
            let hint = if selected.is_none() && self.live_state.selected.is_some() {
                "Selection not in sample · ↑↓ choose another".into()
            } else {
                self.connection_pause_hint()
                    .unwrap_or_else(|| t("←→ fold/unfold · Enter open · Space pause").into())
            };
            f.render_stateful_widget(
                List::new(items)
                    .highlight_style(Style::default())
                    .highlight_symbol("› ")
                    .block(
                        self.focused_card(&title, focus::Region::Main)
                            .title_bottom(hint),
                    ),
                area,
                &mut state,
            );
            let rows = astra_tui_kit::RenderedRows::fixed(
                card("").inner(area),
                state.offset(),
                connections.len(),
                2,
            );
            self.hits
                .borrow_mut()
                .bind_rows(&rows, |index| match &connections[index].key {
                    ConnectionRowKey::Connection(id) => Target::Connection(*id),
                    key => Target::ConnectionRow(key.clone()),
                });
            return;
        }
        let (table_area, detail_area) = if area.width >= 120 {
            let split =
                Layout::horizontal([Constraint::Min(0), Constraint::Length(38)]).split(area);
            (split[0], Some(split[1]))
        } else if area.height >= 20 {
            let split = Layout::vertical([Constraint::Min(0), Constraint::Length(6)]).split(area);
            (split[0], Some(split[1]))
        } else {
            (area, None)
        };
        self.record_live_viewport(
            Viewport::Connections,
            table_area.height.saturating_sub(3) as usize,
        );
        self.record_live_viewport(
            Viewport::Details,
            detail_area.map_or(0, |area| area.height.saturating_sub(2) as usize),
        );
        let header = TableRow::new(
            [t("Target"), t("Network"), t("Exit"), "↑", "↓", t("Time")]
                .map(|h| Cell::from(h).style(Style::default().fg(MUTED))),
        );
        let muted = Style::default().fg(MUTED);
        let rows: Vec<_> = connections
            .iter()
            .map(|row| {
                let c = &row.connection;
                let label = row.label();
                let port = format!(":{}", c.port);
                let split = label.rfind(&port).unwrap_or(label.len());
                let mut target = crate::theme::mark_matches(
                    &label[..split],
                    &self.connection_filter,
                    Style::default(),
                );
                if split < label.len() {
                    target.push(Span::styled(port.clone(), muted));
                    target.extend(crate::theme::mark_matches(
                        &label[split + port.len()..],
                        &self.connection_filter,
                        Style::default(),
                    ));
                }
                TableRow::new(vec![
                    Cell::from(Line::from(target)),
                    Cell::from(Line::from(vec![
                        Span::styled(
                            c.network.clone(),
                            Style::default().fg(crate::theme::network_color(&c.network)),
                        ),
                        Span::styled(format!(" · {}", c.inbound), muted),
                    ])),
                    Cell::from(Line::from(crate::theme::mark_matches(
                        &c.outbound,
                        &self.connection_filter,
                        Style::default().fg(crate::theme::exit_color(&c.outbound)),
                    ))),
                    Cell::from(bytes(c.upload)).style(crate::theme::volume_style(c.upload)),
                    Cell::from(bytes(c.download)).style(crate::theme::volume_style(c.download)),
                    Cell::from(format!("{:.0}s", c.elapsed_ms as f64 / 1000.0)).style(muted),
                ])
                .style(crate::theme::connection_row_style(
                    Some(&row.key) == selected_key.as_ref(),
                    row.fresh,
                ))
            })
            .collect();
        let title = format!(
            "{}{} {}/{} · {} {}{}",
            self.connection_badges(),
            t("Active"),
            snapshot.connections.len(),
            snapshot.active,
            t("Total"),
            snapshot.total,
            if snapshot.truncated {
                " · partial sample"
            } else {
                ""
            }
        );
        let footer = Line::from(
            if let Some(hint) = self
                .connection_pause_hint()
                .filter(|_| !self.connection_filtering)
            {
                crate::theme::key_hints(&hint)
            } else if self.connection_filtering {
                vec![
                    Span::styled(
                        format!(" /{}▏", self.connection_filter),
                        crate::theme::matched(),
                    ),
                    Span::styled(" · Enter ", muted),
                ]
            } else if self.connection_filter.is_empty() {
                let mut spans = crate::theme::key_hints(&format!(
                    " {}",
                    t("←→ fold/unfold · / filter · x close · X close all")
                ));
                spans.push(Span::raw(" "));
                spans
            } else {
                vec![
                    Span::styled(format!(" {} ", t("Filter")), muted),
                    Span::styled(
                        format!("“{}”", self.connection_filter),
                        crate::theme::matched(),
                    ),
                    Span::styled(format!(" · Esc {} ", t("clear")), muted),
                ]
            },
        );
        let empty = rows.is_empty();
        let mut state = TableState::default().with_selected(selected_index);
        *state.offset_mut() = self.live_list_offset(
            Viewport::Connections,
            state.selected(),
            connections.len(),
            table_area.height.saturating_sub(3) as usize,
        );
        f.render_stateful_widget(
            Table::new(
                rows,
                [
                    Constraint::Min(18),
                    Constraint::Length(14),
                    Constraint::Min(10),
                    Constraint::Length(10),
                    Constraint::Length(10),
                    Constraint::Length(7),
                ],
            )
            .header(header)
            .block(
                self.focused_card(&title, focus::Region::Main)
                    .title_bottom(footer),
            )
            .row_highlight_style(Style::default()),
            table_area,
            &mut state,
        );
        if empty {
            f.render_widget(
                Paragraph::new(if snapshot.truncated {
                    "No matches in the loaded sample"
                } else {
                    t("No matching connections")
                })
                .style(Style::default().fg(MUTED)),
                table_area.inner(ratatui::layout::Margin::new(2, 2)),
            );
        }
        let body = card("").inner(table_area);
        let content = Rect::new(
            body.x,
            body.y.saturating_add(1),
            body.width,
            body.height.saturating_sub(1),
        );
        let geometry =
            astra_tui_kit::RenderedRows::fixed(content, state.offset(), connections.len(), 1);
        self.record_live_viewport(Viewport::Connections, geometry.visible_capacity);
        self.hits
            .borrow_mut()
            .bind_rows(&geometry, |index| match &connections[index].key {
                ConnectionRowKey::Connection(id) => Target::Connection(*id),
                key => Target::ConnectionRow(key.clone()),
            });
        if table_area.width >= 44 {
            self.button(
                f,
                Rect::new(table_area.right() - 15, table_area.y, 13, 1),
                "X Close all",
                Target::Runtime(Command::Disconnect(None)),
            );
        }
        if let (Some(detail), Some(c)) = (detail_area, selected) {
            let lines = vec![
                Line::from(vec![
                    Span::styled(format!("{} ", t("Target")), Style::default().fg(MUTED)),
                    Span::raw(format!("{}:{}", c.host, c.port)),
                ]),
                Line::from(vec![
                    Span::styled(format!("{} ", t("Exit")), Style::default().fg(MUTED)),
                    Span::styled(
                        c.outbound.clone(),
                        Style::default().fg(crate::theme::exit_color(&c.outbound)),
                    ),
                ]),
                Line::raw(format!(
                    "{} · {} · {}",
                    c.network,
                    c.inbound,
                    c.source.as_deref().unwrap_or("—")
                )),
                Line::raw(format!(
                    "↑ {}  ↓ {}  {:.1}s",
                    bytes(c.upload),
                    bytes(c.download),
                    c.elapsed_ms as f64 / 1000.0
                )),
                Line::styled(
                    if selected_row.is_some_and(|r| r.is_group()) {
                        t("←→ fold/unfold · totals for this target")
                    } else {
                        t("x close · 4 then t explains a route")
                    },
                    Style::default().fg(MUTED),
                ),
            ];
            f.render_widget(
                Paragraph::new(lines)
                    .wrap(Wrap { trim: false })
                    .scroll((self.live_state.detail_scroll, 0))
                    .block(self.focused_card("Details", focus::Region::Secondary)),
                detail,
            );
        }
        if selected.is_none() && self.live_state.selected.is_some() {
            f.render_widget(
                Paragraph::new("Selected connection is not present in this sample")
                    .style(Style::default().fg(Color::Yellow)),
                Rect::new(
                    table_area.x + 1,
                    table_area.y,
                    table_area.width.saturating_sub(2),
                    1,
                ),
            );
        }
    }
}

#[cfg(test)]
#[path = "../test/ui_live_test.rs"]
mod tests;
