use super::dashboard::{card, Target, BLUE, MUTED};
use super::*;
use astra_ipc::runtime::{Command, Mode, Snapshot};
use ratatui::{layout::Rect, widgets::Sparkline};

pub(super) fn bytes(value: u64) -> String {
    for (unit, scale) in [("GiB", 1u64 << 30), ("MiB", 1 << 20), ("KiB", 1 << 10)] {
        if value >= scale {
            return format!("{:.1} {unit}", value as f64 / scale as f64);
        }
    }
    format!("{value} B")
}

impl Ui {
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
    pub(super) fn live(&self) -> Option<&Snapshot> {
        let row = self.selected()?;
        self.runtime_snapshot
            .as_ref()
            .filter(|s| self.runtime_instance == row.instance.name && Some(s.pid) == row.pid)
    }
    pub(super) fn accept_runtime(&mut self, name: String, snapshot: Snapshot) {
        let previous = self
            .runtime_snapshot
            .as_ref()
            .filter(|old| name == self.runtime_instance && old.pid == snapshot.pid);
        let elapsed = self.runtime_sample.elapsed().as_secs_f64();
        self.rates = if let Some(old) = previous {
            if elapsed > 0.1 {
                [
                    (snapshot.upload.saturating_sub(old.upload) as f64 / elapsed) as u64,
                    (snapshot.download.saturating_sub(old.download) as f64 / elapsed) as u64,
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
        self.runtime_instance = name;
        self.runtime_snapshot = Some(snapshot);
        self.group_selection = self
            .group_selection
            .min(self.live().map_or(0, |s| s.groups.len().saturating_sub(1)));
        self.connection_selection = self
            .connection_selection
            .min(self.visible_connections().len().saturating_sub(1));
        self.runtime_error.clear();
    }
    pub(super) fn runtime_action(&self, command: Command) -> Input {
        match self.selected() {
            Some(row) if row.pid.is_some() && row.instance.kind == Kind::Proxy => Input::Run(
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
        }
    }
    pub(super) fn choose_runtime_member(&mut self, index: usize) -> Input {
        let Some((name, _)) = &self.runtime_picker else {
            return Input::Idle;
        };
        let group = self
            .live()
            .and_then(|s| s.groups.iter().find(|g| &g.name == name));
        if let Some(group) = group.filter(|g| g.mode == "select") {
            if let Some(member) = group.members.get(index) {
                let command = Command::Select {
                    group: group.name.clone(),
                    member: member.name.clone(),
                };
                self.runtime_picker = None;
                return self.runtime_action(command);
            }
        }
        Input::Idle
    }
    fn visible_connections(&self) -> Vec<&astra_ipc::runtime::Connection> {
        let filter = self.connection_filter.to_lowercase();
        self.live()
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
            .unwrap_or_default()
    }
    pub(super) fn live_key(&mut self, key: event::KeyEvent) -> Option<Input> {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return None;
        }
        if self.runtime_picker.is_some()
            && key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return Some(Input::Idle);
        }
        if let Some((name, index)) = self.runtime_picker.clone() {
            let count = self
                .live()
                .and_then(|s| s.groups.iter().find(|g| g.name == name))
                .map_or(0, |g| g.members.len());
            let mut next = index;
            match key.code {
                KeyCode::Esc => self.runtime_picker = None,
                KeyCode::Down | KeyCode::Char('j') => {
                    next = (index + 1).min(count.saturating_sub(1))
                }
                KeyCode::Up | KeyCode::Char('k') => next = index.saturating_sub(1),
                KeyCode::PageDown => next = (index + 10).min(count.saturating_sub(1)),
                KeyCode::PageUp => next = index.saturating_sub(10),
                KeyCode::Home => next = 0,
                KeyCode::End => next = count.saturating_sub(1),
                KeyCode::Enter => return Some(self.choose_runtime_member(index)),
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
            self.connection_selection = 0;
            return Some(Input::Idle);
        }
        // Modified shortcuts must never become unmodified network mutations.
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            if matches!(self.page, 1 | 3) && key.modifiers.contains(KeyModifiers::CONTROL) {
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
        // Connection navigation must not operate the hidden instance list.
        if self.live().is_none() && self.page == 3 {
            return match key.code {
                KeyCode::Up
                | KeyCode::Down
                | KeyCode::Home
                | KeyCode::End
                | KeyCode::PageUp
                | KeyCode::PageDown
                | KeyCode::Enter
                | KeyCode::Char('j' | 'k' | 'g' | 'G' | 'd' | 'D') => Some(Input::Idle),
                _ => None,
            };
        }
        let snapshot = self.live()?;
        match key.code {
            KeyCode::Char('0') => return Some(self.runtime_action(Command::Reset)),
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
        if self.page == 1 {
            let count = snapshot.groups.len();
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
                    self.group_selection = (self.group_selection + 10).min(count.saturating_sub(1))
                }
                KeyCode::PageUp => self.group_selection = self.group_selection.saturating_sub(10),
                KeyCode::Enter => self.open_runtime_group(),
                KeyCode::Char('t') => {
                    return Some(self.runtime_action(Command::TestGroup(
                        snapshot.groups.get(self.group_selection)?.name.clone(),
                    )))
                }
                _ => return None,
            }
            return Some(Input::Idle);
        }
        if self.page == 3 {
            let connections = self.visible_connections();
            match key.code {
                KeyCode::Char('/') => self.connection_filtering = true,
                KeyCode::Esc => self.connection_filter.clear(),
                KeyCode::Down | KeyCode::Char('j') => {
                    self.connection_selection =
                        (self.connection_selection + 1).min(connections.len().saturating_sub(1))
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    self.connection_selection = self.connection_selection.saturating_sub(1)
                }
                KeyCode::Home | KeyCode::Char('g') => self.connection_selection = 0,
                KeyCode::End | KeyCode::Char('G') => {
                    self.connection_selection = connections.len().saturating_sub(1)
                }
                KeyCode::PageDown => {
                    self.connection_selection =
                        (self.connection_selection + 10).min(connections.len().saturating_sub(1))
                }
                KeyCode::PageUp => {
                    self.connection_selection = self.connection_selection.saturating_sub(10)
                }
                KeyCode::Enter => return Some(Input::Idle),
                KeyCode::Char('d') => {
                    return Some(self.runtime_action(Command::Disconnect(Some(
                        connections.get(self.connection_selection)?.id,
                    ))))
                }
                KeyCode::Char('D') => return Some(self.runtime_action(Command::Disconnect(None))),
                _ => return None,
            }
            return Some(Input::Idle);
        }
        None
    }

    pub(super) fn render_transfer(&self, f: &mut Frame<'_>, area: Rect, index: usize) {
        let total = self
            .live()
            .map_or(0, |s| if index == 0 { s.upload } else { s.download });
        f.render_widget(
            Paragraph::new(format!(
                " {}/s\n {} {}",
                bytes(self.rates[index]),
                t("Total"),
                bytes(total)
            ))
            .block(card(if index == 0 { "Upload" } else { "Download" })),
            area,
        );
        if area.height >= 5 {
            let data: Vec<_> = self.history[index].iter().copied().collect();
            f.render_widget(
                Sparkline::default()
                    .data(&data)
                    .style(Style::default().fg(BLUE)),
                Rect::new(
                    area.x + 1,
                    area.bottom() - 2,
                    area.width.saturating_sub(2),
                    1,
                ),
            );
        }
    }
    pub(super) fn render_runtime_modes(&self, f: &mut Frame<'_>, area: Rect) {
        let Some(snapshot) = self.live() else {
            return;
        };
        let cells = Layout::horizontal([Constraint::Ratio(1, 6); 6]).split(area);
        for (index, mode) in [Mode::Rule, Mode::Global, Mode::Direct].iter().enumerate() {
            let label = t(match mode {
                Mode::Rule => "Rule",
                Mode::Global => "Global",
                Mode::Direct => "Direct",
            });
            let rect = cells[index].inner(ratatui::layout::Margin::new(1, 1));
            f.render_widget(
                Paragraph::new(format!(" {} ", label)).style(Style::default().fg(Color::White).bg(
                    if snapshot.mode == *mode {
                        BLUE
                    } else {
                        Color::DarkGray
                    },
                )),
                rect,
            );
            self.hit(rect, Target::Runtime(Command::SetMode(*mode)));
        }
        for (index, label, enabled, command) in [
            (
                3,
                t("System proxy"),
                snapshot.system_proxy,
                Command::SystemProxy(!snapshot.system_proxy),
            ),
            (4, "TUN", snapshot.tun, Command::Tun(!snapshot.tun)),
            (
                5,
                "LAN",
                snapshot.allow_lan,
                Command::AllowLan(!snapshot.allow_lan),
            ),
        ] {
            let rect = cells[index].inner(ratatui::layout::Margin::new(1, 1));
            f.render_widget(
                Paragraph::new(format!(" {} {} ", label, if enabled { "●" } else { "○" })).style(
                    Style::default().fg(Color::White).bg(if enabled {
                        BLUE
                    } else {
                        Color::DarkGray
                    }),
                ),
                rect,
            );
            self.hit(rect, Target::Runtime(command));
        }
    }
    pub(super) fn render_live_groups(&self, f: &mut Frame<'_>, area: Rect) {
        let Some(snapshot) = self.live() else {
            return;
        };
        let items: Vec<_> = snapshot
            .groups
            .iter()
            .map(|group| {
                ListItem::new(vec![
                    Line::from(format!(
                        " {}   [{}] {}",
                        group.name,
                        group.mode,
                        if group.testing { t("Testing…") } else { "" }
                    )),
                    Line::styled(
                        format!(
                            "   → {}  ·  {} {}",
                            group.selected.as_deref().unwrap_or("—"),
                            group.members.len(),
                            t("members")
                        ),
                        Style::default().fg(MUTED),
                    ),
                ])
            })
            .collect();
        let mut state = ListState::default().with_selected(
            (!items.is_empty()).then_some(self.group_selection.min(items.len().saturating_sub(1))),
        );
        f.render_stateful_widget(
            List::new(items)
                .block(card("Proxy groups").title_bottom(if self.page == 0 {
                    format!("2 {}", t("Proxy groups"))
                } else {
                    t("Enter members · t test · m mode · S system proxy · T TUN").into()
                }))
                .highlight_style(Style::default().bg(BLUE).fg(Color::White))
                .highlight_symbol("› "),
            area,
            &mut state,
        );
        for visible in 0..area.height.saturating_sub(2) as usize / 2 {
            let index = state.offset() + visible;
            if index >= snapshot.groups.len() {
                break;
            }
            self.hit(
                Rect::new(
                    area.x + 1,
                    area.y + 1 + visible as u16 * 2,
                    area.width.saturating_sub(2),
                    2,
                ),
                Target::Group(index),
            );
        }
    }
    pub(super) fn render_connections(&self, f: &mut Frame<'_>, area: Rect) {
        let Some(snapshot) = self.live() else {
            f.render_widget(
                Paragraph::new(format!(
                    "{}\n{}",
                    t("Select a running proxy in Profiles"),
                    self.runtime_error
                ))
                .wrap(Wrap { trim: false })
                .block(card("Connections")),
                area,
            );
            return;
        };
        let connections = self.visible_connections();
        let items: Vec<_> = connections
            .iter()
            .map(|c| {
                ListItem::new(vec![
                    Line::from(format!(
                        " {}:{}  {}  → {}",
                        c.host, c.port, c.network, c.outbound
                    )),
                    Line::styled(
                        format!(
                            "   ↑ {}  ↓ {}  {:.1}s  {}",
                            bytes(c.upload),
                            bytes(c.download),
                            c.elapsed_ms as f64 / 1000.0,
                            c.source.as_deref().unwrap_or("—")
                        ),
                        Style::default().fg(MUTED),
                    ),
                ])
            })
            .collect();
        let title = format!(
            "{} {} · {} {}{}",
            t("Connections"),
            snapshot.active,
            t("Total"),
            snapshot.total,
            if snapshot.truncated { " +" } else { "" }
        );
        let footer = if self.connection_filtering {
            format!("/{}▏ · Enter", self.connection_filter)
        } else {
            format!(
                "{} {}",
                t("/ filter · d close · D close all"),
                self.connection_filter
            )
        };
        let mut state = ListState::default().with_selected(
            (!items.is_empty())
                .then_some(self.connection_selection.min(items.len().saturating_sub(1))),
        );
        f.render_stateful_widget(
            List::new(items)
                .block(card(&title).title_bottom(footer))
                .highlight_style(Style::default().bg(BLUE).fg(Color::White))
                .highlight_symbol("›"),
            area,
            &mut state,
        );
        for visible in 0..area.height.saturating_sub(2) as usize / 2 {
            let index = state.offset() + visible;
            if index >= connections.len() {
                break;
            }
            self.hit(
                Rect::new(
                    area.x + 1,
                    area.y + 1 + visible as u16 * 2,
                    area.width.saturating_sub(2),
                    2,
                ),
                Target::Connection(index),
            );
        }
        if area.width >= 60 {
            if let Some(connection) = connections.get(self.connection_selection) {
                self.button(
                    f,
                    Rect::new(area.right() - 34, area.y, 17, 1),
                    "Close selected",
                    Target::Runtime(Command::Disconnect(Some(connection.id))),
                );
            }
        }
        if area.width >= 36 {
            let button = Rect::new(area.right() - 16, area.y, 14, 1);
            self.button(
                f,
                button,
                "Close all",
                Target::Runtime(Command::Disconnect(None)),
            );
        }
    }
    pub(super) fn render_runtime_picker(&self, f: &mut Frame<'_>) {
        let Some((name, index)) = &self.runtime_picker else {
            return;
        };
        let Some(group) = self
            .live()
            .and_then(|s| s.groups.iter().find(|g| &g.name == name))
        else {
            return;
        };
        let area = f.area();
        let width = area.width.saturating_sub(4).min(90).max(1).min(area.width);
        let height = (group.members.len() as u16)
            .saturating_add(2)
            .min(area.height.saturating_sub(2))
            .max(1)
            .min(area.height);
        let popup = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + (area.height - height) / 2,
            width,
            height,
        );
        self.hits.borrow_mut().clear();
        f.render_widget(Clear, popup);
        let items: Vec<_> = group
            .members
            .iter()
            .map(|member| {
                let delay = match member.alive {
                    Some(true) => format!("{} ms", member.delay_ms.unwrap_or(0)),
                    Some(false) => t("Timeout").into(),
                    None => "—".into(),
                };
                ListItem::new(format!(
                    "{} {}   {delay}",
                    if group.selected.as_ref() == Some(&member.name) {
                        "●"
                    } else {
                        "○"
                    },
                    member.name
                ))
            })
            .collect();
        let mut state = ListState::default().with_selected(
            (!items.is_empty()).then_some((*index).min(items.len().saturating_sub(1))),
        );
        let title = format!(
            "{} [{}]{}",
            group.name,
            group.mode,
            if group.testing {
                format!(" · {}", t("Testing…"))
            } else {
                String::new()
            }
        );
        f.render_stateful_widget(
            List::new(items)
                .block(card(&title).title_bottom(t("Enter select · t test · Esc back")))
                .highlight_style(Style::default().bg(BLUE).fg(Color::White))
                .highlight_symbol("›"),
            popup,
            &mut state,
        );
        if popup.width >= 24 {
            self.button(
                f,
                Rect::new(popup.right() - 10, popup.y, 8, 1),
                "Test",
                Target::Runtime(Command::TestGroup(group.name.clone())),
            );
        }
        if popup.width >= 28 && popup.height >= 2 {
            self.button(
                f,
                Rect::new(popup.right() - 14, popup.bottom() - 1, 12, 1),
                "Esc back",
                Target::Back,
            );
        }
        for visible in 0..popup.height.saturating_sub(2) as usize {
            if visible + state.offset() >= group.members.len() {
                break;
            }
            self.hit(
                Rect::new(
                    popup.x + 1,
                    popup.y + 1 + visible as u16,
                    popup.width.saturating_sub(2),
                    1,
                ),
                Target::Member(visible + state.offset()),
            );
        }
    }
}

#[cfg(test)]
mod tests {
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
                groups: vec![Group {
                    name: "PROXY".into(),
                    mode: "select".into(),
                    selected: Some("Tokyo".into()),
                    testing: false,
                    members: vec![
                        Member {
                            name: "Tokyo".into(),
                            delay_ms: Some(80),
                            alive: Some(true),
                        },
                        Member {
                            name: "Singapore".into(),
                            delay_ms: Some(110),
                            alive: Some(true),
                        },
                    ],
                }],
                ..Default::default()
            },
        );
        ui
    }
    fn key(ui: &mut Ui, code: KeyCode) -> Input {
        ui.key(event::KeyEvent::new(code, KeyModifiers::NONE))
    }
    #[test]
    fn selecting_nodes_and_closing_filtered_connections_use_runtime_commands_with_pid() {
        let mut ui = sample();
        key(&mut ui, KeyCode::Char('2'));
        key(&mut ui, KeyCode::Enter);
        assert!(ui.runtime_picker.is_some());
        // Editing shortcuts cannot act on the profile behind the member picker.
        assert!(matches!(key(&mut ui, KeyCode::Char('e')), Input::Idle));
        assert!(ui.editor.is_none());
        key(&mut ui, KeyCode::Down);
        assert!(
            matches!(key(&mut ui, KeyCode::Enter), Input::Run(Action::Runtime { command: Command::Select {group, member}, expected_pid: Some(42), .. }, _) if group == "PROXY" && member == "Singapore")
        );
        key(&mut ui, KeyCode::Char('4'));
        key(&mut ui, KeyCode::Char('/'));
        for c in "example".chars() {
            key(&mut ui, KeyCode::Char(c));
        }
        key(&mut ui, KeyCode::Enter);
        assert_eq!(ui.visible_connections().len(), 1);
        assert!(matches!(
            key(&mut ui, KeyCode::Char('d')),
            Input::Run(
                Action::Runtime {
                    command: Command::Disconnect(Some(7)),
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
        for page in [1, 3] {
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
        ui.switch_core(Kind::Rule);
        assert_eq!(ui.page, 2);
        ui.navigate(3);
        ui.runtime_snapshot = None;
        assert!(matches!(key(&mut ui, KeyCode::Char('d')), Input::Idle));
        assert!(ui.prompt.is_none());
        key(&mut ui, KeyCode::Down);
        assert_eq!(ui.selections[0], 0);
    }

    #[test]
    fn page_navigation_clears_search_capture_and_tools_exclude_profile_management() {
        let mut ui = sample();
        ui.kind = Kind::Rule;
        ui.connection_filtering = true;
        ui.navigate(3);
        assert_eq!(ui.kind, Kind::Proxy);
        assert!(!ui.connection_filtering);
        ui.navigate(4);
        assert!(ui.tool_operations().iter().all(|op| matches!(
            op,
            Operation::Logs
                | Operation::Diagnostics
                | Operation::UpdateSubscriptions
                | Operation::Explain
        )));
        key(&mut ui, KeyCode::BackTab);
        assert_eq!(ui.page, 3);
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
                for page in [0, 1, 3] {
                    ui.page = page;
                    for (width, height) in [(140, 40), (90, 24), (40, 12), (1, 1)] {
                        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                        terminal.draw(|f| ui.render(f)).unwrap();
                    }
                }
                if language == crate::i18n::Language::English
                    && std::env::var_os("ASTRA_TUI_PREVIEW").is_some()
                {
                    ui.page = 0;
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
                ui.page = 1;
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
