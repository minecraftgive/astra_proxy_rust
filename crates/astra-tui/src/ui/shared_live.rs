//! Proxies and Connections pages, driven by the active profile's runtime snapshot.
use super::dashboard::{card, Target, BLUE, MUTED};
use super::shell::{CONNECTIONS, OVERVIEW, PROXIES};
use super::*;
use astra_ipc::host::{Coverage, RuntimeTelemetry, RuntimeView};
use astra_ipc::runtime::{Command, Mode, Snapshot};
use ratatui::{
    layout::Rect,
    style::Modifier,
    widgets::{Cell, Row as TableRow, Table, TableState},
};

#[derive(Default)]
pub(super) struct LiveState {
    telemetry: Option<RuntimeTelemetry>,
    baseline: Option<(Snapshot, RuntimeTelemetry)>,
    received: Option<Instant>,
    selected: Option<u64>,
    frozen: Option<Snapshot>,
    frozen_telemetry: Option<RuntimeTelemetry>,
    frozen_at: Option<String>,
    disconnected: bool,
    rates_valid: bool,
    detail_open: bool,
    detail_scroll: u16,
    viewports: [std::cell::Cell<usize>; 5],
}

#[derive(Clone, Copy)]
pub(super) enum Viewport {
    Overview,
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
    fn disabled_live_alias(&self, key: event::KeyEvent) -> bool {
        !self.vim_keys
            && !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
            && matches!(key.code, KeyCode::Char('h' | 'j' | 'k' | 'l' | 'g' | 'G'))
    }
    pub(super) fn live_input(&self) -> bool {
        self.editor.is_none()
            && self.viewer.is_none()
            && self.prompt.is_none()
            && self.recovery.is_none()
            && self.menu.is_none()
            && self.binding_picker.is_none()
            && self.diagnostics.is_none()
            && self.explanation.is_none()
            && self.help_scroll.is_none()
    }
    /// Runtime data of the active profile; stale data from an older process is hidden.
    pub(super) fn live(&self) -> Option<&Snapshot> {
        let row = self.active_proxy()?;
        self.runtime_snapshot
            .as_ref()
            .filter(|s| self.runtime_instance == row.instance.name && Some(s.pid) == row.pid)
    }
    pub(super) fn accept_runtime_json(&mut self, name: String, text: &str) -> Result<()> {
        let view: RuntimeView = serde_json::from_str(text)?;
        self.accept_runtime_view(name, view);
        Ok(())
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
        self.runtime_instance.clear();
        self.runtime_snapshot = None;
        self.runtime_sample = Instant::now();
        self.runtime_error.clear();
        self.history = Default::default();
        self.rates = [0, 0];
        self.direct_rates = [0, 0];
        self.connection_selection = 0;
        self.overview_scroll = 0;
    }
    pub(super) fn accept_runtime_view(&mut self, name: String, view: RuntimeView) {
        let RuntimeView {
            snapshot,
            telemetry,
        } = view;
        let identity_changed = self.runtime_instance != name
            || self
                .runtime_snapshot
                .as_ref()
                .is_some_and(|old| old.pid != snapshot.pid)
            || telemetry
                .as_ref()
                .zip(self.live_state.telemetry.as_ref())
                .is_some_and(|(new, old)| new.host_boot_id != old.host_boot_id);
        if identity_changed {
            if self.runtime_snapshot.is_some() && self.runtime_instance == name {
                self.notice_host_restart();
            }
            self.reset_live_identity();
        }
        // Command replies without source metadata must not replace a hosted source clock.
        if telemetry.is_none() && self.live_state.telemetry.is_some() && !identity_changed {
            self.runtime_instance = name;
            self.runtime_snapshot = Some(snapshot);
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
                        if !self.connections_paused() {
                            self.animations.trigger(animation::Kind::Sample);
                        }
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
        self.runtime_sample = Instant::now();
        self.runtime_instance = name;
        self.runtime_snapshot = Some(snapshot);
        self.group_selection = self
            .group_selection
            .min(self.live().map_or(0, |s| s.groups.len().saturating_sub(1)));
        let ids: Vec<_> = self.visible_connections().iter().map(|c| c.id).collect();
        if let Some(id) = self.live_state.selected {
            if let Some(index) = ids.iter().position(|value| *value == id) {
                self.connection_selection = index;
            }
        } else if let Some(id) = ids.first() {
            self.live_state.selected = Some(*id);
            self.select_connection(0);
        }
        self.runtime_error.clear();
    }
    fn set_runtime_rates(&mut self, old: &Snapshot, snapshot: &Snapshot, elapsed: f64) {
        self.rates = [
            (snapshot.proxy_upload.saturating_sub(old.proxy_upload) as f64 / elapsed) as u64,
            (snapshot.proxy_download.saturating_sub(old.proxy_download) as f64 / elapsed) as u64,
        ];
        self.direct_rates = [
            (snapshot
                .upload
                .saturating_sub(snapshot.proxy_upload)
                .saturating_sub(old.upload.saturating_sub(old.proxy_upload)) as f64
                / elapsed) as u64,
            (snapshot
                .download
                .saturating_sub(snapshot.proxy_download)
                .saturating_sub(old.download.saturating_sub(old.proxy_download))
                as f64
                / elapsed) as u64,
        ];
    }
    fn record_runtime_rates(&mut self) {
        for index in 0..2 {
            self.history[index].push_back(self.rates[index]);
            if self.history[index].len() > 64 {
                self.history[index].pop_front();
            }
        }
    }
    fn accept_legacy_rates(&mut self, name: &str, snapshot: &Snapshot) {
        let previous = self
            .runtime_snapshot
            .as_ref()
            .filter(|old| name == self.runtime_instance && old.pid == snapshot.pid);
        let elapsed = self.runtime_sample.elapsed().as_secs_f64();
        self.direct_rates = if let Some(old) = previous {
            if elapsed > 0.1 {
                [
                    (snapshot
                        .upload
                        .saturating_sub(snapshot.proxy_upload)
                        .saturating_sub(old.upload.saturating_sub(old.proxy_upload))
                        as f64
                        / elapsed) as u64,
                    (snapshot
                        .download
                        .saturating_sub(snapshot.proxy_download)
                        .saturating_sub(old.download.saturating_sub(old.proxy_download))
                        as f64
                        / elapsed) as u64,
                ]
            } else {
                self.direct_rates
            }
        } else {
            self.history = Default::default();
            [0, 0]
        };
        self.rates = if let Some(old) = previous {
            if elapsed > 0.1 {
                [
                    (snapshot.proxy_upload.saturating_sub(old.proxy_upload) as f64 / elapsed)
                        as u64,
                    (snapshot.proxy_download.saturating_sub(old.proxy_download) as f64 / elapsed)
                        as u64,
                ]
            } else {
                self.rates
            }
        } else {
            self.history = Default::default();
            [0, 0]
        };
        for index in 0..2 {
            self.history[index].push_back(self.rates[index]);
            if self.history[index].len() > 64 {
                self.history[index].pop_front();
            }
        }
        self.runtime_sample = Instant::now();
        self.live_state.rates_valid = previous.is_some();
    }
    pub(super) fn runtime_disconnected(&mut self, error: String) {
        self.live_state.disconnected = true;
        self.live_state.rates_valid = false;
        self.live_state.baseline = None;
        self.runtime_error = error;
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
        if self.live_state.frozen.take().is_none() {
            self.live_state.frozen = self.live().cloned();
            self.live_state.frozen_telemetry = self.live_state.telemetry.clone();
            self.live_state.frozen_at = self
                .live_state
                .frozen
                .as_ref()
                .map(|_| chrono::Local::now().format("%H:%M:%S").to_string());
            self.animations.finish(animation::Kind::Sample);
        } else {
            self.live_state.frozen_telemetry = None;
            self.live_state.frozen_at = None;
        }
        if let Some(id) = self.live_state.selected {
            if let Some(index) = self.visible_connections().iter().position(|c| c.id == id) {
                self.connection_selection = index;
            }
        }
        Input::Idle
    }
    pub(super) fn connection_snapshot(&self) -> Option<&Snapshot> {
        self.live()?;
        self.live_state.frozen.as_ref().or_else(|| self.live())
    }
    pub(super) fn select_connection(&mut self, index: usize) {
        let id = self.visible_connections().get(index).map(|c| c.id);
        if self.live_state.selected != id {
            self.live_state.detail_scroll = 0;
        }
        self.connection_selection = index;
        self.live_state.selected = id;
    }
    pub(super) fn select_connection_id(&mut self, id: u64) {
        if !self.visible_connections().iter().any(|c| c.id == id)
            && self
                .connection_snapshot()
                .is_some_and(|s| s.connections.iter().any(|c| c.id == id))
        {
            self.connection_filter.clear();
        }
        if let Some(index) = self.visible_connections().iter().position(|c| c.id == id) {
            self.select_connection(index);
        }
    }
    pub(super) fn reveal_selected_connection(&mut self) {
        if let Some(id) = self.live_state.selected {
            self.select_connection_id(id);
        }
    }
    pub(super) fn connection_is_selected(&self, id: u64) -> bool {
        self.live_state.selected == Some(id)
    }
    fn selected_connection(&self) -> Option<&astra_ipc::runtime::Connection> {
        let id = self.live_state.selected?;
        self.visible_connections().into_iter().find(|c| c.id == id)
    }
    pub(super) fn runtime_action(&mut self, command: Command) -> Input {
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
    pub(super) fn visible_connections(&self) -> Vec<&astra_ipc::runtime::Connection> {
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
    pub(super) fn live_secondary_key(&mut self, key: event::KeyEvent) -> Option<Input> {
        if self.disabled_live_alias(key) {
            return Some(Input::Idle);
        }
        if self.page == OVERVIEW {
            let count = self.live()?.groups.len();
            let page = self.live_page_size(Viewport::Groups);
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
        } else if self.page == CONNECTIONS {
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
        if self.live_state.detail_open && self.page == CONNECTIONS {
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
            if matches!(self.page, PROXIES | CONNECTIONS)
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
        if self.live().is_none() && matches!(self.page, PROXIES | CONNECTIONS) {
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
        if self.page == OVERVIEW {
            let page = self.live_page_size(Viewport::Overview);
            let count = self
                .overview_scroll_limit
                .get()
                .max(snapshot.connections.len());
            match key.code {
                KeyCode::Char('z') => {
                    self.traffic_expanded = !self.traffic_expanded;
                    return Some(Input::Idle);
                }
                KeyCode::Char('j') | KeyCode::Down => {
                    self.overview_scroll = (self.overview_scroll + 1).min(count.saturating_sub(1));
                    self.select_connection(
                        self.overview_scroll
                            .min(snapshot.connections.len().saturating_sub(1)),
                    );
                    return Some(Input::Idle);
                }
                KeyCode::Char('k') | KeyCode::Up => {
                    self.overview_scroll = self.overview_scroll.saturating_sub(1);
                    self.select_connection(self.overview_scroll);
                    return Some(Input::Idle);
                }
                KeyCode::PageDown => {
                    self.overview_scroll =
                        (self.overview_scroll + page).min(count.saturating_sub(1));
                    self.select_connection(
                        self.overview_scroll
                            .min(snapshot.connections.len().saturating_sub(1)),
                    );
                    return Some(Input::Idle);
                }
                KeyCode::PageUp => {
                    self.overview_scroll = self.overview_scroll.saturating_sub(page);
                    self.select_connection(self.overview_scroll);
                    return Some(Input::Idle);
                }
                KeyCode::Home | KeyCode::Char('g') => {
                    self.overview_scroll = 0;
                    self.select_connection(0);
                    return Some(Input::Idle);
                }
                KeyCode::End | KeyCode::Char('G') => {
                    self.overview_scroll = count.saturating_sub(1);
                    self.select_connection(snapshot.connections.len().saturating_sub(1));
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
        if self.page == PROXIES {
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
        if self.page == CONNECTIONS {
            let page = self.live_page_size(Viewport::Connections);
            let connections = self.visible_connections();
            let count = connections.len();
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
                        self.message = "Selected connection is not present in this sample".into();
                        return Some(Input::Idle);
                    };
                    if self.live_state.disconnected
                        || !self
                            .live()
                            .is_some_and(|s| s.connections.iter().any(|c| c.id == id))
                    {
                        self.message = "Connection cannot be verified in the latest sample".into();
                        return Some(Input::Idle);
                    }
                    return Some(self.runtime_action(Command::Disconnect(Some(id))));
                }
                KeyCode::Char(' ') => return Some(self.toggle_live_pause()),
                KeyCode::Char('X') => {
                    if !connections.is_empty() {
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

    /// Mode segments and takeover switches, shared by Overview and the status bar.
    pub(super) fn render_runtime_modes(&self, f: &mut Frame<'_>, area: Rect) {
        let Some(snapshot) = self.live() else {
            return;
        };
        if area.width < 4 || area.height < 3 {
            return;
        }
        let block = card("CONTROLS");
        let inner = block.inner(area);
        f.render_widget(block, area);
        if inner.width == 0 || inner.height == 0 {
            return;
        }
        let area = inner;
        let compact = area.width < 90;
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
        let switch_width =
            |key: char, label: &str| Line::from(format!(" {key} {label} ○ ")).width() as u16 + 2;
        let cells = Layout::horizontal([
            Constraint::Length(10),
            Constraint::Length(12),
            Constraint::Length(12),
            Constraint::Length(switch_width(switches[0].0, switches[0].1)),
            Constraint::Length(switch_width(switches[1].0, switches[1].1)),
            Constraint::Min(0),
        ])
        .split(area);
        for (index, mode) in [Mode::Rule, Mode::Global, Mode::Direct].iter().enumerate() {
            let label = t(match mode {
                Mode::Rule => "Rule",
                Mode::Global => "Global",
                Mode::Direct => "Direct",
            });
            let rect = cells[index].inner(ratatui::layout::Margin::new(1, 0));
            f.render_widget(
                Paragraph::new(format!(" m {label} ")).style(if snapshot.mode == *mode {
                    Style::default()
                        .fg(Color::White)
                        .bg(BLUE)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(MUTED)
                }),
                rect,
            );
            self.hit(rect, Target::Runtime(Command::SetMode(*mode)));
        }
        for (index, (key, label, enabled, command)) in switches.into_iter().enumerate() {
            let rect = cells[index + 3].inner(ratatui::layout::Margin::new(1, 0));
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::raw(format!(" {key} {label} ")),
                    Span::styled(
                        if enabled { "●" } else { "○" },
                        Style::default().fg(if enabled { Color::Green } else { MUTED }),
                    ),
                ]))
                .style(if enabled {
                    Style::default().add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(MUTED)
                }),
                rect,
            );
            self.hit(rect, Target::Runtime(command));
        }
    }

    fn render_not_live(&self, f: &mut Frame<'_>, area: Rect, title: &str) {
        let text = match self.active_proxy() {
            None => t("No profile yet · 6 then n to create one").to_owned(),
            Some(row) if row.pid.is_none() => {
                tr!("{} is not connected · c connect", row.instance.name)
            }
            Some(_) => format!("{}\n{}", t("Waiting for runtime data…"), self.runtime_error),
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
            f.render_stateful_widget(
                List::new(items)
                    .block(card("Proxy groups").border_style(
                        Style::default().fg(if focus_members { Color::DarkGray } else { BLUE }),
                    ))
                    .highlight_style(if focus_members {
                        crate::theme::selected_inactive()
                    } else {
                        crate::theme::selected()
                    })
                    .highlight_symbol("› "),
                groups_area,
                &mut state,
            );
            for visible in 0..groups_area.height.saturating_sub(2) as usize / 2 {
                let index = state.offset() + visible;
                if index >= snapshot.groups.len() {
                    break;
                }
                self.hit(
                    Rect::new(
                        groups_area.x + 1,
                        groups_area.y + 1 + visible as u16 * 2,
                        groups_area.width.saturating_sub(2),
                        2,
                    ),
                    Target::Group(index),
                );
            }
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
                .block(
                    card(&title).border_style(Style::default().fg(if focus_members {
                        BLUE
                    } else {
                        Color::DarkGray
                    })),
                )
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
        for visible in 0..members_area.height.saturating_sub(2) as usize {
            if visible + state.offset() >= group.members.len() {
                break;
            }
            self.hit(
                Rect::new(
                    members_area.x + 1,
                    members_area.y + 1 + visible as u16,
                    members_area.width.saturating_sub(2),
                    1,
                ),
                Target::Member(visible + state.offset()),
            );
        }
        self.focus_region(f, groups_area, focus::Region::Main);
        self.focus_region(f, members_area, focus::Region::Secondary);
    }

    /// Connection table with a detail pane for the selected row.
    pub(super) fn render_connections(&self, f: &mut Frame<'_>, area: Rect) {
        self.record_live_viewport(Viewport::Connections, 0);
        self.record_live_viewport(Viewport::Details, 0);
        let Some(snapshot) = self.connection_snapshot() else {
            self.render_not_live(f, area, "Connections");
            return;
        };
        let connections = self.visible_connections();
        let selected = self.selected_connection();
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
                .map(|c| {
                    ListItem::new(vec![
                        Line::raw(format!("{}:{}", c.host, c.port)),
                        Line::raw(format!(
                            "{} · {} · ↑{} ↓{}",
                            c.outbound,
                            c.network,
                            bytes(c.upload),
                            bytes(c.download)
                        )),
                    ])
                })
                .collect();
            let title = format!(
                "{}Connections {}/{}{}",
                self.connection_badges(),
                connections.len(),
                snapshot.active,
                if snapshot.truncated {
                    " · partial"
                } else {
                    ""
                }
            );
            let mut state =
                ListState::default().with_selected(selected.map(|_| self.connection_selection));
            let hint = if selected.is_none() && self.live_state.selected.is_some() {
                "Selection not in sample · ↑↓ choose another".into()
            } else {
                self.connection_pause_hint()
                    .unwrap_or_else(|| " Enter details · Space pause ".into())
            };
            f.render_stateful_widget(
                List::new(items)
                    .highlight_style(crate::theme::selected())
                    .highlight_symbol("› ")
                    .block(card(&title).title_bottom(hint)),
                area,
                &mut state,
            );
            self.focus_region(f, area, focus::Region::Main);
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
            .map(|c| {
                // Host stands out, port recedes; the active filter is marked in place.
                let mut target =
                    crate::theme::mark_matches(&c.host, &self.connection_filter, Style::default());
                target.push(Span::styled(format!(":{}", c.port), muted));
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
                let mut spans =
                    crate::theme::key_hints(&format!(" {}", t("/ filter · x close · X close all")));
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
        let mut state = TableState::default().with_selected(
            selected.is_some().then_some(
                self.connection_selection
                    .min(connections.len().saturating_sub(1)),
            ),
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
            .block(card(&title).title_bottom(footer))
            .row_highlight_style(crate::theme::selected()),
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
        for visible in 0..table_area.height.saturating_sub(3) as usize {
            let index = state.offset() + visible;
            if index >= connections.len() {
                break;
            }
            self.hit(
                Rect::new(
                    table_area.x + 1,
                    table_area.y + 2 + visible as u16,
                    table_area.width.saturating_sub(2),
                    1,
                ),
                Target::Connection(connections[index].id),
            );
        }
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
                    t("x close · 4 then t explains a route"),
                    Style::default().fg(MUTED),
                ),
            ];
            f.render_widget(
                Paragraph::new(lines)
                    .wrap(Wrap { trim: false })
                    .scroll((self.live_state.detail_scroll, 0))
                    .block(card("Details")),
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
        self.focus_region(f, table_area, focus::Region::Main);
        if let Some(area) = detail_area {
            self.focus_region(f, area, focus::Region::Secondary);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::shell::{OVERVIEW, PROFILES};
    use super::*;
    use astra_ipc::runtime::{Connection, Group, Member};
    use ratatui::backend::TestBackend;
    fn sample() -> Ui {
        let mut ui = Ui::new(Session::default());
        ui.rows.push(Row {
            instance: Instance {
                name: "proxy".into(),
                kind: Kind::Proxy,
                config: "/profiles/proxy.toml".into(),
            },
            pid: Some(42),
            state: "Running".into(),
            uptime: 60,
            changed: false,
        });
        ui.accept_runtime(
            "proxy".into(),
            Snapshot {
                pid: 42,
                upload: 2048,
                download: 8192,
                active: 1,
                total: 8,
                connections: vec![Connection {
                    id: 7,
                    source: Some("127.0.0.1:50000".into()),
                    host: "example.test".into(),
                    port: 443,
                    network: "Tcp".into(),
                    inbound: "Socks5".into(),
                    outbound: "Tokyo".into(),
                    upload: 2048,
                    download: 8192,
                    elapsed_ms: 3000,
                }],
                groups: vec![
                    Group {
                        name: "PROXY".into(),
                        mode: "select".into(),
                        selected: Some("auto".into()),
                        testing: false,
                        members: vec![
                            Member {
                                name: "auto".into(),
                                delay_ms: None,
                                alive: None,
                            },
                            Member {
                                name: "Singapore".into(),
                                delay_ms: Some(110),
                                alive: Some(true),
                            },
                        ],
                    },
                    Group {
                        name: "auto".into(),
                        mode: "auto".into(),
                        selected: Some("Tokyo".into()),
                        testing: false,
                        members: vec![Member {
                            name: "Tokyo".into(),
                            delay_ms: Some(80),
                            alive: Some(true),
                        }],
                    },
                ],
                ..Default::default()
            },
        );
        ui
    }
    fn key(ui: &mut Ui, code: KeyCode) -> Input {
        ui.key(event::KeyEvent::new(code, KeyModifiers::NONE))
    }
    fn telemetry(snapshot: Snapshot, boot: &str, sequence: u64, time: u64) -> RuntimeView {
        RuntimeView {
            snapshot,
            telemetry: Some(RuntimeTelemetry {
                host_boot_id: boot.into(),
                active_generation: Some(1),
                sample_sequence: sequence,
                sample_elapsed_ms: time,
                coverage: Coverage::Complete,
                ..Default::default()
            }),
        }
    }
    #[test]
    fn live_paging_tracks_rendered_viewports_and_two_line_rows() {
        let mut ui = sample();
        let mut snapshot = ui.live().unwrap().clone();
        let connection = snapshot.connections[0].clone();
        snapshot.connections = (0..80)
            .map(|id| {
                let mut row = connection.clone();
                row.id = id;
                row.host = format!("host-{id}.test");
                row
            })
            .collect();
        snapshot.active = 80;
        let mut group = snapshot.groups[0].clone();
        group.members = (0..60)
            .map(|id| Member {
                name: format!("member-{id}"),
                delay_ms: None,
                alive: None,
            })
            .collect();
        group.selected = Some("member-0".into());
        snapshot.groups = (0..50)
            .map(|id| {
                let mut row = group.clone();
                row.name = format!("group-{id}");
                row
            })
            .collect();
        ui.accept_runtime("proxy".into(), snapshot);
        ui.navigate(CONNECTIONS);
        for (width, step) in [(120, 17), (70, 9)] {
            ui.select_connection(0);
            let mut terminal = Terminal::new(TestBackend::new(width, 20)).unwrap();
            terminal
                .draw(|f| ui.render_connections(f, f.area()))
                .unwrap();
            key(&mut ui, KeyCode::PageDown);
            assert_eq!(ui.connection_selection, step);
            key(&mut ui, KeyCode::PageUp);
            assert_eq!(ui.connection_selection, 0);
        }
        ui.navigate(PROXIES);
        let mut terminal = Terminal::new(TestBackend::new(100, 20)).unwrap();
        terminal
            .draw(|f| ui.render_live_groups(f, f.area()))
            .unwrap();
        key(&mut ui, KeyCode::PageDown);
        assert_eq!(ui.group_selection, 9);
        key(&mut ui, KeyCode::PageUp);
        key(&mut ui, KeyCode::Enter);
        terminal
            .draw(|f| ui.render_live_groups(f, f.area()))
            .unwrap();
        key(&mut ui, KeyCode::PageDown);
        assert_eq!(ui.runtime_picker.as_ref().unwrap().1, 18);
        key(&mut ui, KeyCode::PageUp);
        assert_eq!(ui.runtime_picker.as_ref().unwrap().1, 0);
        ui.navigate(OVERVIEW);
        for (width, step) in [(140, 18), (70, 6)] {
            ui.overview_scroll = 0;
            let mut terminal = Terminal::new(TestBackend::new(width, 24)).unwrap();
            terminal.draw(|f| ui.render_overview(f, f.area())).unwrap();
            key(&mut ui, KeyCode::PageDown);
            assert_eq!(ui.overview_scroll, step);
            key(&mut ui, KeyCode::PageUp);
            assert_eq!(ui.overview_scroll, 0);
        }
        ui.focus.region = focus::Region::Secondary;
        key(&mut ui, KeyCode::PageDown);
        assert_eq!(ui.group_selection, 3);
    }
    #[test]
    fn live_aliases_respect_opt_in_and_delete_never_repeats_or_edits_filters() {
        let mut ui = sample();
        ui.navigate(PROXIES);
        ui.live_key(event::KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(ui.group_selection, 0);
        ui.live_secondary_key(event::KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(ui.group_selection, 0);
        ui.vim_keys = true;
        ui.live_key(event::KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(ui.group_selection, 1);
        assert!(ui
            .live_key(event::KeyEvent::new(KeyCode::Char('0'), KeyModifiers::NONE))
            .is_none());
        ui.navigate(CONNECTIONS);
        assert!(matches!(
            key(&mut ui, KeyCode::Delete),
            Input::Run(
                Action::Runtime {
                    command: Command::Disconnect(Some(7)),
                    ..
                },
                _
            )
        ));
        let mut repeated = event::KeyEvent::new(KeyCode::Delete, KeyModifiers::NONE);
        repeated.kind = KeyEventKind::Repeat;
        assert!(matches!(ui.key(repeated), Input::Idle));
        ui.vim_keys = false;
        ui.connection_filtering = true;
        ui.live_key(event::KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE));
        assert_eq!(ui.connection_filter, "j");
        assert!(matches!(key(&mut ui, KeyCode::Delete), Input::Idle));
    }
    #[test]
    fn hosted_source_clock_deduplicates_cached_responses_and_resets_on_boot() {
        let mut ui = sample();
        let mut snapshot = ui.live().unwrap().clone();
        snapshot.proxy_upload = 100;
        ui.accept_runtime_view(
            "proxy".into(),
            telemetry(snapshot.clone(), "first", 1, 1000),
        );
        let count = ui.history[0].len();
        ui.accept_runtime_view(
            "proxy".into(),
            telemetry(snapshot.clone(), "first", 1, 1000),
        );
        assert_eq!(ui.history[0].len(), count);
        snapshot.proxy_upload = 1100;
        ui.accept_runtime_view(
            "proxy".into(),
            telemetry(snapshot.clone(), "first", 2, 2000),
        );
        assert_eq!(ui.rates[0], 1000);
        assert!(ui.runtime_rates_available());
        let count = ui.history[0].len();
        ui.accept_runtime_view(
            "proxy".into(),
            telemetry(snapshot.clone(), "first", 2, 2000),
        );
        assert_eq!(ui.history[0].len(), count);
        ui.accept_runtime_view("proxy".into(), telemetry(snapshot, "restarted", 1, 10));
        assert!(ui.history[0].is_empty());
        assert!(!ui.runtime_rates_available());
    }
    #[test]
    fn feed_identity_change_clears_frozen_targets_before_legacy_snapshot_arrives() {
        for enhanced in [false, true] {
            let mut ui = sample();
            let mut next = ui.live().unwrap().clone();
            if enhanced {
                ui.accept_runtime_view(
                    "proxy".into(),
                    telemetry(next.clone(), "old-boot", 1, 1000),
                );
            }
            ui.toggle_live_pause();
            assert_eq!(ui.selected_connection().unwrap().id, 7);
            // Feeds calls this exactly when its (profile, host PID) key changes.
            ui.reset_live_identity();
            assert!(ui.live_state.selected.is_none());
            assert!(ui.live_state.telemetry.is_none());
            assert!(!ui.connections_paused());
            assert!(ui.history.iter().all(|lane| lane.is_empty()));
            ui.rows[0].pid = Some(84);
            next.pid = 84;
            next.connections[0].host = "replacement.test".into();
            ui.accept_runtime("proxy".into(), next);
            assert_eq!(ui.connection_snapshot().unwrap().pid, 84);
            assert_eq!(ui.selected_connection().unwrap().host, "replacement.test");
            assert!(!ui.runtime_rates_available());
        }
    }
    #[test]
    fn same_host_reconnect_and_worker_generation_change_preserve_frozen_view() {
        let mut ui = sample();
        let mut snapshot = ui.live().unwrap().clone();
        ui.accept_runtime_view("proxy".into(), telemetry(snapshot.clone(), "boot", 1, 1000));
        ui.toggle_live_pause();
        let frozen_at = ui.live_state.frozen_at.clone();
        ui.runtime_disconnected("reconnecting".into());
        snapshot.connections[0].host = "latest.test".into();
        let mut changed_worker = telemetry(snapshot, "boot", 2, 2000);
        changed_worker.telemetry.as_mut().unwrap().active_generation = Some(2);
        ui.accept_runtime_view("proxy".into(), changed_worker);
        assert!(ui.connections_paused());
        assert_eq!(ui.live_state.frozen_at, frozen_at);
        assert_eq!(ui.selected_connection().unwrap().host, "example.test");
        assert_eq!(ui.live().unwrap().connections[0].host, "latest.test");
        assert_eq!(
            ui.live_state.telemetry.as_ref().unwrap().active_generation,
            Some(2)
        );
    }
    #[test]
    fn selection_survives_reorder_and_missing_sample_never_closes_another_flow() {
        let mut ui = sample();
        ui.page = CONNECTIONS;
        let mut snapshot = ui.live().unwrap().clone();
        let mut newer = snapshot.connections[0].clone();
        newer.id = 99;
        newer.download = u64::MAX / 2;
        snapshot.connections.insert(0, newer);
        ui.accept_runtime("proxy".into(), snapshot.clone());
        assert_eq!(ui.selected_connection().unwrap().id, 7);
        assert_eq!(ui.connection_selection, 1);
        snapshot.connections.retain(|c| c.id != 7);
        snapshot.truncated = true;
        ui.accept_runtime("proxy".into(), snapshot);
        assert!(ui.selected_connection().is_none());
        assert!(matches!(
            ui.live_key(event::KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
            Some(Input::Idle)
        ));
        assert!(ui.message.contains("not present"));
    }
    #[test]
    fn overview_selection_is_revealed_on_connections_despite_an_old_filter() {
        let mut ui = sample();
        ui.connection_filter = "unrelated.test".into();
        ui.navigate(OVERVIEW);
        assert_eq!(ui.visible_connections().len(), 1);
        ui.select_connection_id(7);
        ui.navigate(CONNECTIONS);
        assert_eq!(ui.selected_connection().unwrap().id, 7);
        assert!(ui.connection_filter.is_empty());
        key(&mut ui, KeyCode::Enter);
        assert!(ui.live_detail_open());
        key(&mut ui, KeyCode::Esc);
        assert!(!ui.live_detail_open());
        assert_eq!(ui.page, CONNECTIONS);
    }
    #[test]
    fn explicit_pause_freezes_rows_while_health_and_latest_data_continue() {
        let mut ui = sample();
        ui.toggle_live_pause();
        let frozen_at = ui.live_state.frozen_at.clone();
        assert!(frozen_at.is_some());
        let mut snapshot = ui.live().unwrap().clone();
        snapshot.connections[0].host = "updated.example".into();
        ui.accept_runtime("proxy".into(), snapshot);
        assert_eq!(ui.live_state.frozen_at, frozen_at);
        assert_eq!(ui.visible_connections()[0].host, "example.test");
        assert_eq!(ui.live().unwrap().connections[0].host, "updated.example");
        ui.runtime_disconnected("lost transport".into());
        crate::i18n::with_language(crate::i18n::Language::English, || {
            assert!(ui.runtime_health().contains("Reconnecting"));
        });
        assert!(!ui.runtime_rates_available());
        ui.toggle_live_pause();
        assert_eq!(ui.visible_connections()[0].host, "updated.example");
    }
    #[test]
    fn partial_samples_do_not_fabricate_rates_and_late_samples_do_not_rewind() {
        let mut ui = sample();
        let mut snapshot = ui.live().unwrap().clone();
        ui.accept_runtime_view("proxy".into(), telemetry(snapshot.clone(), "boot", 1, 1000));
        snapshot.proxy_download = 2000;
        ui.accept_runtime_view("proxy".into(), telemetry(snapshot.clone(), "boot", 2, 2000));
        let count = ui.history[1].len();
        let mut partial = telemetry(snapshot.clone(), "boot", 3, 3000);
        partial.telemetry.as_mut().unwrap().coverage = Coverage::Partial;
        ui.accept_runtime_view("proxy".into(), partial);
        assert!(!ui.runtime_rates_available());
        assert_eq!(ui.history[1].len(), count);
        snapshot.proxy_download = 1;
        ui.accept_runtime_view("proxy".into(), telemetry(snapshot, "boot", 2, 2000));
        assert_eq!(ui.live().unwrap().proxy_download, 2000);
        assert_eq!(ui.live_state.telemetry.as_ref().unwrap().sample_sequence, 3);
    }
    #[test]
    fn narrow_views_keep_pause_and_transport_health_visible() {
        crate::i18n::with_language(crate::i18n::Language::English, || {
            let mut ui = sample();
            ui.toggle_live_pause();
            ui.runtime_disconnected("transport lost".into());
            for page in [OVERVIEW, CONNECTIONS] {
                ui.navigate(page);
                let text = screen(&ui, 70, 24);
                assert!(
                    text.contains("PAUSED") && text.contains("RECONNECTING"),
                    "{text}"
                );
                assert!(
                    text.contains("example.test"),
                    "last received rows remain visible"
                );
            }
        });
    }
    #[test]
    fn cursor_tabs_and_states_are_highlighted_on_every_list_page() {
        let selected = crate::theme::selected();
        let mut ui = sample();
        ui.rows.push(Row {
            instance: Instance {
                name: "rules".into(),
                kind: Kind::Rule,
                config: "/r.toml".into(),
            },
            pid: None,
            state: "Stopped".into(),
            uptime: 0,
            changed: false,
        });
        let draw = |ui: &Ui| {
            let mut t = ratatui::Terminal::new(TestBackend::new(110, 30)).unwrap();
            t.draw(|f| ui.render(f)).unwrap();
            t.backend().buffer().clone()
        };
        // Styles of the cells spelling `text` on the first row that shows it.
        let styles = |buffer: &ratatui::buffer::Buffer, text: &str| {
            (0..buffer.area.height)
                .find_map(|y| {
                    let line: String = (0..buffer.area.width)
                        .map(|x| buffer[(x, y)].symbol())
                        .collect();
                    let at = line.find(text)?;
                    let column = line[..at].chars().count() as u16;
                    Some(
                        (column..column + text.chars().count() as u16)
                            .map(|x| buffer[(x, y)].style())
                            .collect::<Vec<_>>(),
                    )
                })
                .unwrap_or_default()
        };
        crate::i18n::with_language(crate::i18n::Language::English, || {
            ui.navigate(PROFILES);
            let buffer = draw(&ui);
            assert!(styles(&buffer, "Profiles 1")
                .iter()
                .all(|s| s.bg == selected.bg));
            assert!(styles(&buffer, "Rule cores 1")
                .iter()
                .all(|s| s.bg != selected.bg));
            let state = styles(&buffer, "State: Running");
            assert!(
                state.iter().any(|s| s.fg == Some(Color::Green)),
                "state is colored"
            );
            key(&mut ui, KeyCode::Tab);
            key(&mut ui, KeyCode::Right);
            key(&mut ui, KeyCode::Enter);
            let buffer = draw(&ui);
            assert!(styles(&buffer, "Rule cores 1")
                .iter()
                .all(|s| s.bg == selected.bg));
            // With focus on members, the group cursor stays visible without competing.
            ui.navigate(PROXIES);
            key(&mut ui, KeyCode::Enter);
            let buffer = draw(&ui);
            assert!(styles(&buffer, "● auto")
                .iter()
                .all(|s| s.bg == selected.bg));
            // The selected group's second line, in the groups pane on the left.
            let group = styles(&buffer, "→ auto");
            assert!(!group.is_empty());
            assert!(group
                .iter()
                .all(|s| s.add_modifier.contains(Modifier::UNDERLINED) && s.bg != selected.bg));
            // Connections: exit, network and volume are colored on unselected rows.
            let mut snapshot = ui.live().unwrap().clone();
            snapshot.connections.push(Connection {
                id: 8,
                source: None,
                host: "quiet.test".into(),
                port: 53,
                network: "Udp".into(),
                inbound: "Tun".into(),
                outbound: "DIRECT".into(),
                upload: 100,
                download: 3 * 1024 * 1024,
                elapsed_ms: 1000,
            });
            ui.accept_runtime("proxy".into(), snapshot);
            ui.navigate(CONNECTIONS);
            let buffer = draw(&ui);
            let fg = |text: &str| styles(&buffer, text).first().and_then(|s| s.fg);
            assert_eq!(fg("DIRECT"), Some(Color::Green));
            assert_eq!(fg("Udp"), Some(Color::Magenta));
            assert_eq!(fg(":53"), Some(MUTED));
            assert_eq!(fg("100 B"), Some(MUTED), "idle counters fade");
            assert!(styles(&buffer, "3.0 MiB")[0]
                .add_modifier
                .contains(Modifier::BOLD));
            // The page's key hints color keys apart from what they do.
            assert_eq!(fg("x close"), Some(Color::Cyan));
            assert_eq!(styles(&buffer, "x close")[2].fg, Some(MUTED));
            // An active filter is marked in the footer and in unselected matching rows.
            ui.connection_filter = "test".into();
            let buffer = draw(&ui);
            let matched = crate::theme::matched().bg;
            assert_eq!(styles(&buffer, "“test”")[0].bg, matched);
            let row = styles(&buffer, "quiet.test");
            assert!(row[..6].iter().all(|s| s.bg != matched) && row[6].bg == matched);
        });
    }
    #[test]
    fn status_bar_colors_state_and_drops_detail_instead_of_cutting_words() {
        let mut ui = sample();
        let mut snapshot = ui.live().unwrap().clone();
        snapshot.tun = true;
        ui.accept_runtime("proxy".into(), snapshot);
        // Overview folds its status into the Astra header; the standalone status bar
        // still serves every other page, so test it there.
        ui.navigate(PROXIES);
        ui.rates = [0, 48_000];
        let bar = |ui: &Ui, width: u16| {
            let mut t = ratatui::Terminal::new(TestBackend::new(width, 12)).unwrap();
            t.draw(|f| ui.render(f)).unwrap();
            let buffer = t.backend().buffer().clone();
            let cells: Vec<_> = (0..width).map(|x| buffer[(x, 0)].clone()).collect();
            let text: String = cells.iter().map(|c| c.symbol()).collect();
            (text, cells)
        };
        let color_of = |cells: &[ratatui::buffer::Cell], text: &str, word: &str| {
            let at = text.find(word).unwrap();
            cells[text[..at].chars().count()].fg
        };
        crate::i18n::with_language(crate::i18n::Language::English, || {
            let (text, cells) = bar(&ui, 220);
            assert_eq!(color_of(&cells, &text, "TUN"), Color::Green);
            assert_eq!(color_of(&cells, &text, "LAN"), MUTED);
            assert_eq!(color_of(&cells, &text, "Rule"), Color::Cyan);
            assert_eq!(
                color_of(&cells, &text, "↑ 0 B/s"),
                MUTED,
                "idle rate is muted"
            );
            assert_eq!(color_of(&cells, &text, "↓"), Color::Cyan);
            assert!(
                text.contains("Proxy")
                    && text.contains("Direct")
                    && text.trim_end().ends_with("B/s"),
                "rates are right-aligned: {text}"
            );
            // Narrower bars lose whole parts in order, never half a word.
            let (text, _) = bar(&ui, 70);
            assert!(
                !text.contains("B/s") && text.contains("TUN") && !text.contains("LAN"),
                "{text}"
            );
            for width in [48, 36, 24] {
                let (text, _) = bar(&ui, width);
                let words: Vec<_> = text.split_whitespace().collect();
                for word in words {
                    assert!(
                        [
                            "Astra",
                            "│",
                            "●",
                            "proxy",
                            "Connected",
                            "Profile",
                            "connected",
                            "Rule",
                            "TUN"
                        ]
                        .contains(&word)
                            || word
                                .chars()
                                .all(|c| c.is_ascii_digit() || "hms".contains(c)),
                        "partial word {word:?} at {width}: {text}"
                    );
                }
            }
        });
    }
    #[test]
    fn find_jumps_to_matching_groups_and_members() {
        let mut ui = sample();
        ui.navigate(PROXIES);
        let find = |ui: &mut Ui, text: &str| {
            key(ui, KeyCode::Char('/'));
            assert!(matches!(ui.prompt, Some(Prompt::Find(PROXIES))));
            ui.key(event::KeyEvent::new(
                KeyCode::Char('u'),
                KeyModifiers::CONTROL,
            ));
            for c in text.chars() {
                key(ui, KeyCode::Char(c));
            }
            key(ui, KeyCode::Enter);
        };
        find(&mut ui, "sing");
        assert_eq!(ui.group_selection, 0);
        assert_eq!(ui.runtime_picker, Some(("PROXY".into(), 1)));
        // Inside the member list, / searches again and finds members of other groups.
        find(&mut ui, "tokyo");
        assert_eq!(ui.group_selection, 1);
        assert_eq!(ui.runtime_picker, Some(("auto".into(), 0)));
        // "auto" is a member of PROXY and a group; after the last match the search wraps.
        find(&mut ui, "auto");
        assert_eq!(ui.runtime_picker, Some(("PROXY".into(), 0)));
        find(&mut ui, "auto");
        assert_eq!(ui.runtime_picker, None);
        assert_eq!(ui.group_selection, 1);
        find(&mut ui, "nothing");
        assert!(ui.message.contains("nothing"));
    }
    #[test]
    fn exit_chain_follows_group_selections_to_the_final_node() {
        let ui = sample();
        let (chain, delay, alive) = exit_chain(ui.live().unwrap());
        assert_eq!(chain, ["PROXY", "auto", "Tokyo"]);
        assert_eq!((delay, alive), (Some(80), Some(true)));
    }
    #[test]
    fn selecting_nodes_and_closing_filtered_connections_use_runtime_commands_with_pid() {
        let mut ui = sample();
        key(&mut ui, KeyCode::Char('2'));
        key(&mut ui, KeyCode::Enter);
        assert!(ui.runtime_picker.is_some());
        // Editing shortcuts cannot act on the profile behind the member list.
        assert!(matches!(key(&mut ui, KeyCode::Char('e')), Input::Idle));
        assert!(ui.editor.is_none());
        key(&mut ui, KeyCode::Down);
        assert!(
            matches!(key(&mut ui, KeyCode::Enter), Input::Run(Action::Runtime { command: Command::Select {group, member}, expected_pid: Some(42), .. }, _) if group == "PROXY" && member == "Singapore")
        );
        // Automatic groups stay read-only.
        key(&mut ui, KeyCode::Esc);
        key(&mut ui, KeyCode::Down);
        key(&mut ui, KeyCode::Enter);
        assert!(matches!(key(&mut ui, KeyCode::Enter), Input::Idle));
        key(&mut ui, KeyCode::Esc);
        key(&mut ui, KeyCode::Char('3'));
        key(&mut ui, KeyCode::Char('/'));
        for c in "example".chars() {
            key(&mut ui, KeyCode::Char(c));
        }
        key(&mut ui, KeyCode::Enter);
        assert_eq!(ui.visible_connections().len(), 1);
        // d never closes connections; x does.
        assert!(matches!(key(&mut ui, KeyCode::Char('d')), Input::Idle));
        assert!(matches!(
            key(&mut ui, KeyCode::Char('x')),
            Input::Run(
                Action::Runtime {
                    command: Command::Disconnect(Some(7)),
                    expected_pid: Some(42),
                    ..
                },
                _
            )
        ));
        // Closing everything asks first.
        assert!(matches!(key(&mut ui, KeyCode::Char('X')), Input::Idle));
        assert!(matches!(ui.prompt, Some(Prompt::CloseAll { .. })));
        assert!(matches!(key(&mut ui, KeyCode::Enter), Input::Idle));
        assert!(ui.prompt.is_none(), "confirmation defaults to cancel");
        key(&mut ui, KeyCode::Char('X'));
        key(&mut ui, KeyCode::Tab);
        assert!(matches!(
            key(&mut ui, KeyCode::Enter),
            Input::Run(
                Action::Runtime {
                    command: Command::Disconnect(None),
                    expected_pid: Some(42),
                    ..
                },
                _
            )
        ));
        assert!(matches!(
            key(&mut ui, KeyCode::Char('m')),
            Input::Run(
                Action::Runtime {
                    command: Command::SetMode(Mode::Global),
                    ..
                },
                _
            )
        ));
    }
    #[test]
    fn runtime_navigation_never_changes_hidden_instances_or_disconnects_on_ctrl_d() {
        let mut ui = sample();
        ui.rows.push(ui.rows[0].clone());
        ui.rows[1].instance.name = "other".into();
        for page in [PROXIES, CONNECTIONS] {
            ui.navigate(page);
            for code in [
                KeyCode::End,
                KeyCode::Home,
                KeyCode::PageDown,
                KeyCode::PageUp,
                KeyCode::Char('G'),
            ] {
                assert!(matches!(key(&mut ui, code), Input::Idle));
                assert_eq!(ui.selections[0], 0);
            }
            assert!(matches!(
                ui.key(event::KeyEvent::new(
                    KeyCode::Char('d'),
                    KeyModifiers::CONTROL
                )),
                Input::Idle
            ));
            assert_eq!(ui.selections[0], 0);
        }
        ui.runtime_snapshot = None;
        assert!(matches!(key(&mut ui, KeyCode::Char('x')), Input::Idle));
        assert!(ui.prompt.is_none());
        key(&mut ui, KeyCode::Down);
        assert_eq!(ui.selections[0], 0);
        // Rule cores selected in Profiles never replace the active profile's runtime.
        ui.navigate(PROFILES);
        ui.switch_core(Kind::Rule);
        assert_eq!(ui.active_proxy().unwrap().instance.name, "proxy");
    }

    #[test]
    fn page_navigation_clears_search_capture() {
        let mut ui = sample();
        ui.connection_filtering = true;
        ui.navigate(CONNECTIONS);
        assert!(!ui.connection_filtering);
        key(&mut ui, KeyCode::Char('2'));
        assert_eq!(ui.page, PROXIES);
        key(&mut ui, KeyCode::Char('1'));
        assert_eq!(ui.page, OVERVIEW);
    }

    fn screen(ui: &Ui, width: u16, height: u16) -> String {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| ui.render(f)).unwrap();
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
    #[test]
    fn proxies_show_members_beside_groups_and_connections_show_details() {
        crate::i18n::with_language(crate::i18n::Language::English, || {
            let mut ui = sample();
            ui.navigate(PROXIES);
            ui.open_runtime_group();
            let proxies = screen(&ui, 120, 24);
            for value in [
                "Proxy groups",
                "PROXY",
                "Singapore",
                "110 ms",
                "Enter selects",
            ] {
                assert!(proxies.contains(value), "{value}\n{proxies}");
            }
            ui.navigate(CONNECTIONS);
            let connections = screen(&ui, 130, 24);
            for value in ["example.test:443", "Tokyo", "Details", "127.0.0.1:50000"] {
                assert!(connections.contains(value), "{value}\n{connections}");
            }
            if std::env::var_os("ASTRA_TUI_PREVIEW").is_some() {
                std::fs::write(
                    concat!(
                        env!("CARGO_MANIFEST_DIR"),
                        "/../../target/tui-pages-preview.txt"
                    ),
                    format!("{proxies}\n\n{connections}"),
                )
                .unwrap();
            }
        });
    }
    #[test]
    fn tun_toggle_asks_for_elevation_only_when_supported() {
        let mut ui = sample();
        ui.page = OVERVIEW;
        let mut details = crate::protocol::InstanceDetails::default();
        details.elevation_supported = true;
        ui.details.insert("proxy".into(), details);

        assert!(matches!(ui.runtime_action(Command::Tun(true)), Input::Idle));
        assert!(matches!(
            &ui.prompt,
            Some(Prompt::ElevateTun { name, pid }) if name == "proxy" && *pid == Some(42)
        ));
        let enter = event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
        assert!(matches!(ui.key(enter), Input::Idle));
        assert!(ui.prompt.is_none(), "elevation also defaults to cancel");
        ui.runtime_action(Command::Tun(true));
        key(&mut ui, KeyCode::Tab);
        assert!(matches!(
            ui.key(enter),
            Input::Run(Action::PrepareElevation { name, expected_pid: Some(42) }, _) if name == "proxy"
        ));

        // Turning TUN off never needs sudo; the running core handles it.
        assert!(matches!(
            ui.runtime_action(Command::Tun(false)),
            Input::Run(Action::Runtime { .. }, _)
        ));

        let mut ui = sample();
        ui.page = OVERVIEW;
        ui.details
            .insert("proxy".into(), crate::protocol::InstanceDetails::default());
        assert!(matches!(
            ui.runtime_action(Command::Tun(true)),
            Input::Run(Action::Runtime { .. }, _)
        ));
        assert!(ui.prompt.is_none());
    }

    #[test]
    fn live_cards_groups_and_connections_resize_and_old_process_data_is_hidden() {
        for language in [
            crate::i18n::Language::English,
            crate::i18n::Language::Simplified,
            crate::i18n::Language::Traditional,
        ] {
            crate::i18n::with_language(language, || {
                let mut ui = sample();
                for page in [OVERVIEW, PROXIES, CONNECTIONS] {
                    ui.page = page;
                    for (width, height) in [(140, 40), (90, 24), (40, 12), (1, 1)] {
                        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                        terminal.draw(|f| ui.render(f)).unwrap();
                    }
                }
                if language == crate::i18n::Language::English
                    && std::env::var_os("ASTRA_TUI_PREVIEW").is_some()
                {
                    ui.page = OVERVIEW;
                    ui.rates = [1024, 8192];
                    ui.history = [
                        std::collections::VecDeque::from(vec![0, 100, 300, 800, 1024]),
                        std::collections::VecDeque::from(vec![0, 600, 2800, 4096, 8192]),
                    ];
                    let mut preview = Terminal::new(TestBackend::new(140, 40)).unwrap();
                    preview.draw(|f| ui.render(f)).unwrap();
                    let cells: Vec<_> = preview.backend().buffer().content.iter().map(|cell| serde_json::json!({"text":cell.symbol(),"fg":format!("{:?}",cell.fg),"bg":format!("{:?}",cell.bg)})).collect();
                    std::fs::write(
                        concat!(
                            env!("CARGO_MANIFEST_DIR"),
                            "/../../target/tui-live-preview.json"
                        ),
                        serde_json::to_vec(&cells).unwrap(),
                    )
                    .unwrap();
                }
                ui.page = PROXIES;
                ui.open_runtime_group();
                let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
                terminal.draw(|f| ui.render(f)).unwrap();
                assert!(ui
                    .hits
                    .borrow()
                    .iter()
                    .any(|(_, target)| matches!(target, Target::Member(1))));
                ui.rows[0].pid = Some(99);
                assert!(ui.live().is_none());
            });
        }
    }
}
