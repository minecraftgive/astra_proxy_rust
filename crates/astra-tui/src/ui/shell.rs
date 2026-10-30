//! Shared frame matching the GUI: status bar, page bar, hint line and command palette.
//! Every page except Profiles follows the active profile, the selected proxy instance.
use super::dashboard::{Target, BLUE, MUTED};
use super::*;
use astra_ipc::runtime::{Command, Mode};
use ratatui::{layout::Rect, style::Modifier};

pub(super) const OVERVIEW: usize = 0;
pub(super) const PROXIES: usize = 1;
pub(super) const CONNECTIONS: usize = 2;
pub(super) const RULES: usize = 3;
pub(super) const LOGS: usize = 4;
pub(super) const PROFILES: usize = 5;
pub(super) const PAGES: [&str; 6] = [
    "Overview",
    "Proxies",
    "Connections",
    "Rules",
    "Logs",
    "Profiles",
];

#[derive(Clone, Default)]
pub(super) struct Palette {
    pub(super) query: String,
    pub(super) selected: usize,
    /// Opened with p: only lists profiles to switch to.
    pub(super) profiles: bool,
}

#[derive(Clone)]
enum Run {
    Page(usize),
    /// Navigate, then deliver the key there.
    Key(usize, KeyCode),
    /// Deliver the key on the current page (function keys, ?, q, Q).
    Raw(KeyCode),
    /// Instance operation on the active profile.
    Op(Operation),
    Runtime(Command),
    Activate(usize),
    Connect,
    Validate,
}

struct Entry {
    scope: &'static str,
    label: String,
    key: &'static str,
    run: Run,
    unavailable: Option<&'static str>,
}

pub(super) fn duration(seconds: u64) -> String {
    let (h, m, s) = (seconds / 3600, seconds / 60 % 60, seconds % 60);
    if h > 0 {
        format!("{h}h{m:02}m")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{s}s")
    }
}

impl Ui {
    pub(super) fn active_proxy(&self) -> Option<&Row> {
        self.rows
            .iter()
            .filter(|r| r.instance.kind == Kind::Proxy)
            .nth(self.selections[0])
    }
    /// The rule core a profile uses: its startup binding, or the only rule instance.
    pub(super) fn active_rule(&self) -> Option<&Row> {
        let proxy = self.active_proxy()?;
        let rules: Vec<_> = self
            .rows
            .iter()
            .filter(|r| r.instance.kind == Kind::Rule)
            .collect();
        self.details
            .get(&proxy.instance.name)
            .and_then(|d| {
                d.bindings
                    .iter()
                    .find_map(|b| rules.iter().find(|r| &r.instance.name == b).copied())
            })
            .or_else(|| (rules.len() == 1).then(|| rules[0]))
    }
    fn proxies_count(&self) -> usize {
        self.rows
            .iter()
            .filter(|r| r.instance.kind == Kind::Proxy)
            .count()
    }
    /// Page switching is allowed whenever no dialog, editor or text input owns the keyboard.
    pub(super) fn can_switch_page(&self) -> bool {
        self.editor.is_none()
            && self.prompt.is_none()
            && self.recovery.is_none()
            && self.menu.is_none()
            && self.binding_picker.is_none()
            && self.diagnostics.is_none()
            && self.explanation.is_none()
            && self.help_scroll.is_none()
            && self.palette.is_none()
            && self.runtime_picker.is_none()
            && self.viewer.as_ref().is_none_or(logs::Viewer::browsing)
    }
    pub(super) fn close_viewer(&mut self) {
        if let Some(viewer) = self.viewer.take() {
            self.log_settings
                .insert(viewer.name.clone(), viewer.settings.clone());
        }
        self.active_log = None;
    }
    /// Opens the log of `name` on the Logs page.
    pub(super) fn open_logs(&mut self, name: String) -> Input {
        self.page = LOGS;
        if self.viewer.as_ref().is_some_and(|v| v.name == name) {
            return Input::Idle;
        }
        self.close_viewer();
        Input::Run(Action::Logs(name), None)
    }
    pub(super) fn toggle_active(&mut self) -> Input {
        let Some(row) = self.active_proxy() else {
            self.message = t("No profile yet · 6 then n to create one").into();
            return Input::Idle;
        };
        let name = row.instance.name.clone();
        Input::Run(
            if row.pid.is_some() {
                Action::Stop(name)
            } else {
                Action::Start(name)
            },
            None,
        )
    }
    pub(super) fn active_operation(&mut self, operation: Operation) -> Input {
        if self.active_proxy().is_none() {
            self.message = t("No profile yet · 6 then n to create one").into();
            return Input::Idle;
        }
        self.kind = Kind::Proxy;
        self.operation(operation)
    }
    fn rule_operation(&mut self, operation: Operation) -> Input {
        let Some(name) = self.active_rule().map(|r| r.instance.name.clone()) else {
            self.message = t("No rule core for this profile · bind one with b in Profiles").into();
            return Input::Idle;
        };
        self.kind = Kind::Rule;
        if let Some(index) = self.rows().iter().position(|r| r.instance.name == name) {
            self.selections[1] = index;
        }
        self.operation(operation)
    }

    /// Keys shared by every page; `None` lets the page handle the key.
    pub(super) fn page_key(&mut self, key: event::KeyEvent) -> Option<Input> {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        if (control && key.code == KeyCode::Char('k')) || key.code == KeyCode::Char(':') {
            self.palette = Some(Palette::default());
            return Some(Input::Idle);
        }
        let find = (control && key.code == KeyCode::Char('f'))
            || (key.code == KeyCode::Char('/') && !control);
        if find {
            match self.page {
                PROXIES | PROFILES => {
                    self.prompt(Prompt::Find(self.page));
                    return Some(Input::Idle);
                }
                RULES => return Some(self.rule_search()),
                CONNECTIONS => {
                    self.connection_filtering = true;
                    return Some(Input::Idle);
                }
                _ => {}
            }
        }
        if key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return None;
        }
        match key.code {
            KeyCode::Char(c @ '1'..='6') => return Some(self.navigate(c as usize - '1' as usize)),
            KeyCode::BackTab => {
                return Some(self.navigate((self.page + PAGES.len() - 1) % PAGES.len()))
            }
            KeyCode::Char('p') => {
                self.palette = Some(Palette {
                    profiles: true,
                    ..Palette::default()
                });
                return Some(Input::Idle);
            }
            _ => {}
        }
        if self.page == PROFILES || self.viewer.is_some() {
            return None;
        }
        Some(match (self.page, key.code) {
            (_, KeyCode::Char('q')) => Input::Quit,
            (_, KeyCode::Char('Q')) => {
                self.prompt(Prompt::Shutdown);
                Input::Idle
            }
            (_, KeyCode::Char('c')) | (OVERVIEW, KeyCode::Char(' ')) => self.toggle_active(),
            (_, KeyCode::Char('r')) => self.active_operation(Operation::Reload),
            (_, KeyCode::Char('R')) => self.active_operation(Operation::Restart),
            (_, KeyCode::Char('U')) => self.active_operation(Operation::UpdateSubscriptions),
            (OVERVIEW | PROXIES, KeyCode::Char('e')) => self.active_operation(Operation::Edit),
            (OVERVIEW | PROXIES, KeyCode::Char('D')) => {
                self.active_operation(Operation::Diagnostics)
            }
            (OVERVIEW | PROXIES, KeyCode::Char('N')) => self.active_operation(Operation::NetTools),
            (OVERVIEW, KeyCode::Char('L')) => {
                match self.active_proxy().map(|r| r.instance.name.clone()) {
                    Some(name) => self.open_logs(name),
                    None => Input::Idle,
                }
            }
            (RULES, KeyCode::Char('t') | KeyCode::Enter) => self.rule_operation(Operation::Explain),
            (RULES, KeyCode::Char('e')) => self.rule_operation(Operation::Edit),
            (LOGS, KeyCode::Enter | KeyCode::Char('[' | ']')) => {
                match self.log_source(key.code != KeyCode::Char(']')) {
                    Some(name) => self.open_logs(name),
                    None => Input::Idle,
                }
            }
            _ => return None,
        })
    }
    /// Opens the profile's rule core in the editor and starts its search.
    fn rule_search(&mut self) -> Input {
        if self.active_rule().is_none() {
            self.message = t("No rule core for this profile · bind one with b in Profiles").into();
            return Input::Idle;
        }
        self.search_on_open = true;
        let input = self.rule_operation(Operation::Edit);
        self.apply_pending_search();
        input
    }
    pub(super) fn apply_pending_search(&mut self) {
        if !self.search_on_open {
            return;
        }
        if self.editor.as_ref().is_some_and(Editor::browsing) {
            self.search_on_open = false;
            if let Some(editor) = self.editor.as_mut() {
                let _ = editor.key(event::KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
            }
        }
    }
    /// Jumps to the next group/member (Proxies) or instance (Profiles) containing `query`.
    pub(super) fn find(&mut self, page: usize, query: &str) -> bool {
        let query = query.to_lowercase();
        let hit = |text: &str| text.to_lowercase().contains(&query);
        if page == PROXIES {
            let Some(snapshot) = self.live() else {
                return false;
            };
            // Candidates in display order: each group, then its members.
            let mut found = vec![];
            for (g, group) in snapshot.groups.iter().enumerate() {
                if hit(&group.name) {
                    found.push((g, None, group.name.clone()));
                }
                for (m, member) in group.members.iter().enumerate() {
                    if hit(&member.name) {
                        found.push((g, Some(m), group.name.clone()));
                    }
                }
            }
            let order = |g: usize, m: Option<usize>| (g, m.map_or(0, |m| m + 1));
            let current = order(
                self.group_selection,
                self.runtime_picker.as_ref().map(|(_, cursor)| *cursor),
            );
            let Some((g, m, name)) = found
                .iter()
                .find(|(g, m, _)| order(*g, *m) > current)
                .or(found.first())
                .cloned()
            else {
                return false;
            };
            self.group_selection = g;
            self.runtime_picker = m.map(|m| (name, m));
            return true;
        }
        let text = |row: &Row| {
            let mut text = format!("{} {}", row.instance.name, row.instance.config.display());
            if let Some(d) = self.details.get(&row.instance.name) {
                text.push(' ');
                text.push_str(&d.listeners.join(" "));
                text.push(' ');
                text.push_str(&d.outbounds.join(" "));
            }
            text
        };
        let other = if self.kind == Kind::Proxy {
            Kind::Rule
        } else {
            Kind::Proxy
        };
        let list = |kind: Kind| -> Vec<&Row> {
            self.rows
                .iter()
                .filter(|r| r.instance.kind == kind)
                .collect()
        };
        let selected = self.selections[self.index()];
        let same = list(self.kind);
        // After the selection, then the other list, then wrap around.
        let candidates = (selected + 1..same.len())
            .map(|i| (self.kind, i, same[i]))
            .chain(
                list(other)
                    .into_iter()
                    .enumerate()
                    .map(|(i, r)| (other, i, r)),
            )
            .chain(
                (0..=selected.min(same.len().saturating_sub(1)))
                    .filter(|&i| i < same.len())
                    .map(|i| (self.kind, i, same[i])),
            );
        let Some((kind, index)) = candidates
            .filter(|(_, _, row)| hit(&text(row)))
            .map(|(kind, index, _)| (kind, index))
            .next()
        else {
            return false;
        };
        self.kind = kind;
        let slot = self.index();
        self.selections[slot] = index;
        true
    }
    /// Proxy log first; `]` (or a second press) moves to the profile's rule core.
    pub(super) fn log_source(&self, proxy: bool) -> Option<String> {
        let rule = self.active_rule().map(|r| r.instance.name.clone());
        let proxy_name = self.active_proxy().map(|r| r.instance.name.clone());
        if proxy {
            proxy_name.or(rule)
        } else {
            rule.or(proxy_name)
        }
    }
    /// `[` / `]` and page numbers while a log is open on the Logs page.
    pub(super) fn viewer_page_key(&mut self, key: event::KeyEvent) -> Option<Input> {
        if self.page == LOGS
            && key.code == KeyCode::Char('f')
            && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            let viewer = self.viewer.as_mut().filter(|v| v.browsing())?;
            viewer.key(event::KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
            return Some(Input::Idle);
        }
        if self.page != LOGS
            || !self.viewer.as_ref().is_some_and(logs::Viewer::browsing)
            || key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT)
        {
            return None;
        }
        match key.code {
            KeyCode::Char('[' | ']') => {
                let name = self.log_source(key.code == KeyCode::Char('['))?;
                Some(self.open_logs(name))
            }
            KeyCode::Char(c @ '1'..='6') => Some(self.navigate(c as usize - '1' as usize)),
            KeyCode::Char(':') => {
                self.palette = Some(Palette::default());
                Some(Input::Idle)
            }
            _ => None,
        }
    }

    fn palette_entries(&self) -> Vec<Entry> {
        let mut entries = Vec::new();
        let proxies: Vec<_> = self
            .rows
            .iter()
            .filter(|r| r.instance.kind == Kind::Proxy)
            .collect();
        for (index, row) in proxies.iter().enumerate() {
            entries.push(Entry {
                scope: "Profile",
                label: format!(
                    "{} · {}",
                    row.instance.name,
                    t(if row.pid.is_some() {
                        "Running"
                    } else {
                        "Stopped"
                    })
                ),
                key: "p",
                run: Run::Activate(index),
                unavailable: None,
            });
        }
        if self.palette.as_ref().is_some_and(|p| p.profiles) {
            return entries;
        }
        for (index, page) in PAGES.iter().enumerate() {
            entries.push(Entry {
                scope: "Go to",
                label: t(page).into(),
                key: ["1", "2", "3", "4", "5", "6"][index],
                run: Run::Page(index),
                unavailable: None,
            });
        }
        let profile = self.active_proxy();
        let running = profile.is_some_and(|r| r.pid.is_some());
        let no_profile = profile.is_none().then_some("No profile yet");
        let not_running = if profile.is_none() {
            no_profile
        } else {
            (!running).then_some("Not running")
        };
        let live = if self.live().is_some() {
            None
        } else {
            not_running.or(Some("Runtime unavailable"))
        };
        let no_rule = self
            .active_rule()
            .is_none()
            .then_some("No rule core for this profile");
        let mut add = |scope, label: &str, key, run, unavailable| {
            entries.push(Entry {
                scope,
                label: t(label).into(),
                key,
                run,
                unavailable,
            })
        };
        add(
            "Run",
            if running {
                "Disconnect profile"
            } else {
                "Connect profile"
            },
            "c",
            Run::Connect,
            no_profile,
        );
        add(
            "Run",
            "Apply changes (hot reload)",
            "r",
            Run::Op(Operation::Reload),
            not_running,
        );
        add(
            "Run",
            "Full restart",
            "R",
            Run::Op(Operation::Restart),
            not_running,
        );
        for (label, mode) in [
            ("Mode: rule", Mode::Rule),
            ("Mode: global", Mode::Global),
            ("Mode: direct", Mode::Direct),
        ] {
            add(
                "Run",
                label,
                "m",
                Run::Runtime(Command::SetMode(mode)),
                live,
            );
        }
        if let Some(snapshot) = self.live() {
            add(
                "Takeover",
                "Toggle system proxy",
                "S",
                Run::Runtime(Command::SystemProxy(!snapshot.system_proxy)),
                None,
            );
            add(
                "Takeover",
                "Toggle TUN (administrator)",
                "T",
                Run::Runtime(Command::Tun(!snapshot.tun)),
                None,
            );
            add(
                "Takeover",
                "Toggle LAN access",
                "A",
                Run::Runtime(Command::AllowLan(!snapshot.allow_lan)),
                None,
            );
        } else {
            for (label, key) in [
                ("Toggle system proxy", "S"),
                ("Toggle TUN (administrator)", "T"),
                ("Toggle LAN access", "A"),
            ] {
                add("Takeover", label, key, Run::Page(OVERVIEW), live);
            }
        }
        add(
            "Takeover",
            "Reset runtime state",
            "0",
            Run::Runtime(Command::Reset),
            live,
        );
        add(
            "Proxies",
            "Test selected group",
            "t",
            Run::Key(PROXIES, KeyCode::Char('t')),
            live,
        );
        add(
            "Proxies",
            "Node tests · exit IP",
            "D",
            Run::Op(Operation::Diagnostics),
            no_profile,
        );
        add(
            "Proxies",
            "Network tools · DNS / TCP / ICMP",
            "N",
            Run::Op(Operation::NetTools),
            no_profile,
        );
        add(
            "Proxies",
            "Update subscriptions",
            "U",
            Run::Op(Operation::UpdateSubscriptions),
            no_profile,
        );
        add(
            "Connections",
            "Close all connections",
            "X",
            Run::Key(CONNECTIONS, KeyCode::Char('X')),
            live,
        );
        add(
            "Rules",
            "Test a route",
            "t",
            Run::Key(RULES, KeyCode::Char('t')),
            no_rule,
        );
        add(
            "Rules",
            "Edit rules",
            "e",
            Run::Key(RULES, KeyCode::Char('e')),
            no_rule,
        );
        add(
            "Profile",
            "Edit profile",
            "e",
            Run::Op(Operation::Edit),
            no_profile,
        );
        add(
            "Profile",
            "New instance",
            "n",
            Run::Key(PROFILES, KeyCode::Char('n')),
            None,
        );
        add(
            "Profile",
            "Validate configuration",
            "v",
            Run::Validate,
            no_profile,
        );
        add(
            "Profile",
            "Startup bindings",
            "b",
            Run::Op(Operation::Bind),
            no_profile,
        );
        add(
            "Profile",
            "Configuration path",
            "",
            Run::Op(Operation::Config),
            no_profile,
        );
        add(
            "Profile",
            "Copy configuration",
            "y",
            Run::Op(Operation::Copy),
            no_profile,
        );
        add(
            "General",
            "Task details",
            "F6",
            Run::Raw(KeyCode::F(6)),
            None,
        );
        add("General", "TUI log", "F7", Run::Raw(KeyCode::F(7)), None);
        add(
            "General",
            "Open page in new window",
            "F8",
            Run::Raw(KeyCode::F(8)),
            None,
        );
        add(
            "General",
            "Keyboard help",
            "?",
            Run::Raw(KeyCode::Char('?')),
            None,
        );
        add(
            "General",
            "Leave interface (cores keep running)",
            "q",
            Run::Raw(KeyCode::Char('q')),
            None,
        );
        add(
            "General",
            "Stop all instances and manager",
            "Q",
            Run::Raw(KeyCode::Char('Q')),
            None,
        );
        entries
    }
    fn palette_matches(&self) -> Vec<Entry> {
        let query = self
            .palette
            .as_ref()
            .map(|p| p.query.to_lowercase())
            .unwrap_or_default();
        self.palette_entries()
            .into_iter()
            .filter(|e| {
                query.is_empty()
                    || format!("{} {} {}", t(e.scope), e.label, e.key)
                        .to_lowercase()
                        .contains(&query)
            })
            .collect()
    }
    pub(super) fn palette_key(&mut self, key: event::KeyEvent) -> Input {
        let count = self.palette_matches().len();
        let palette = self.palette.as_mut().unwrap();
        match key.code {
            KeyCode::Esc => self.palette = None,
            KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.palette = None
            }
            KeyCode::Down => palette.selected = (palette.selected + 1).min(count.saturating_sub(1)),
            KeyCode::Up => palette.selected = palette.selected.saturating_sub(1),
            KeyCode::PageDown => {
                palette.selected = (palette.selected + 10).min(count.saturating_sub(1))
            }
            KeyCode::PageUp => palette.selected = palette.selected.saturating_sub(10),
            KeyCode::Backspace => {
                palette.query.pop();
                palette.selected = 0;
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                palette.query.clear();
                palette.selected = 0;
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                palette.query.push(c);
                palette.selected = 0;
            }
            KeyCode::Enter => {
                let index = palette.selected;
                return self.palette_run(index);
            }
            _ => {}
        }
        Input::Idle
    }
    pub(super) fn palette_run(&mut self, index: usize) -> Input {
        let Some(entry) = self.palette_matches().into_iter().nth(index) else {
            return Input::Idle;
        };
        if let Some(reason) = entry.unavailable {
            self.message = t(reason).into();
            return Input::Idle;
        }
        self.palette = None;
        let press = |ui: &mut Ui, code| ui.key(event::KeyEvent::new(code, KeyModifiers::NONE));
        match entry.run {
            Run::Page(page) => self.navigate(page),
            Run::Key(page, code) => match self.navigate(page) {
                Input::Idle => press(self, code),
                input => input,
            },
            Run::Raw(code) => press(self, code),
            Run::Op(operation) => self.active_operation(operation),
            Run::Runtime(command) => self.runtime_action(command),
            Run::Activate(index) => {
                self.selections[0] = index;
                self.kind = Kind::Proxy;
                if self.page == LOGS {
                    self.close_viewer();
                }
                Input::Idle
            }
            Run::Connect => self.toggle_active(),
            Run::Validate => match self.active_proxy() {
                Some(row) => Input::Run(Action::Validate(row.instance.name.clone()), None),
                None => Input::Idle,
            },
        }
    }

    /// Status bar, page bar and hint line; returns the page content area.
    pub(super) fn render_frame(&self, f: &mut Frame<'_>) -> Rect {
        let area = f.area();
        let rows = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .split(area);
        self.render_status_bar(f, rows[0]);
        self.render_page_bar(f, rows[1]);
        // This page's keys, whole entries only; results live in the status line below.
        let spans = crate::theme::key_hints_fit(&self.hint(), rows[3].width as usize, t("? more"));
        f.render_widget(Paragraph::new(Line::from(spans)), rows[3]);
        rows[2]
    }
    fn render_status_bar(&self, f: &mut Frame<'_>, area: Rect) {
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let muted = Style::default().fg(MUTED);
        let sep = || Span::styled("  │  ", Style::default().fg(Color::DarkGray));
        let brand = vec![Span::styled(" Astra", bold.fg(BLUE)), sep()];
        let Some(row) = self.active_proxy() else {
            let mut spans = brand;
            spans.push(Span::styled(
                t("No profile yet · 6 then n to create one"),
                muted,
            ));
            f.render_widget(Paragraph::new(Line::from(spans)), area);
            return;
        };
        let (dot, color, state) = if row.pid.is_some() {
            ("● ", Color::Green, t("Profile connected"))
        } else if row.state.starts_with("Exited") || row.state == "Unresponsive" {
            ("● ", Color::Red, t("Failed"))
        } else {
            ("○ ", MUTED, t("Not connected"))
        };
        let live = self.live();
        let width = |spans: &[Span]| spans.iter().map(Span::width).sum::<usize>();
        // Detail levels, richest first; the first that fits the bar is drawn, so a
        // narrow terminal drops rates, off switches, uptime, the state word, then
        // the switches and finally the mode.
        const LAST: usize = 6;
        for level in 0..=LAST {
            let mut profile = vec![
                Span::styled(dot, Style::default().fg(color)),
                Span::styled(row.instance.name.clone(), bold),
            ];
            if level < 4 {
                profile.push(Span::styled(
                    format!(" {state}"),
                    Style::default().fg(color),
                ));
            }
            if level < 3 && row.pid.is_some() {
                profile.push(Span::styled(format!(" {}", duration(row.uptime)), muted));
            }
            let mut left = brand.clone();
            let start = width(&left);
            left.extend(profile);
            let end = width(&left);
            let mut right = vec![];
            if let Some(snapshot) = live {
                let (mode, mode_color) = match snapshot.mode {
                    Mode::Rule => ("Rule", Color::Cyan),
                    Mode::Global => ("Global", BLUE),
                    Mode::Direct => ("Direct", Color::Green),
                };
                if level < 6 {
                    left.push(sep());
                    left.push(Span::styled(t(mode), bold.fg(mode_color)));
                }
                let switches = [
                    (t("System proxy"), snapshot.system_proxy),
                    ("TUN", snapshot.tun),
                    ("LAN", snapshot.allow_lan),
                ];
                // Full: every switch with its state. Compact: only those that are on.
                let shown: Vec<_> = switches
                    .iter()
                    .filter(|(_, on)| level < 2 || (*on && level < 5))
                    .collect();
                if !shown.is_empty() {
                    left.push(sep());
                    for (index, (label, on)) in shown.into_iter().enumerate() {
                        if index > 0 {
                            left.push(Span::raw("  "));
                        }
                        let style = if *on {
                            Style::default().fg(Color::Green)
                        } else {
                            muted
                        };
                        left.push(Span::styled(
                            format!("{} {label}", if *on { "●" } else { "○" }),
                            style,
                        ));
                    }
                }
                if level == 0 {
                    // Rates sit at the right edge; idle traffic is muted rather than loud.
                    let rate = |arrow: &str, bytes: u64, active: Color| {
                        Span::styled(
                            format!("{arrow} {}/s", live::bytes(bytes)),
                            if bytes == 0 {
                                muted
                            } else {
                                Style::default().fg(active)
                            },
                        )
                    };
                    right = vec![
                        rate("↑", self.rates[0], Color::Magenta),
                        Span::raw("  "),
                        rate("↓", self.rates[1], Color::Cyan),
                        Span::raw(" "),
                    ];
                }
            }
            let used = width(&left) + width(&right);
            if used <= area.width as usize || level == LAST {
                if !right.is_empty() {
                    left.push(Span::raw(" ".repeat(area.width as usize - used)));
                    left.extend(right);
                }
                // Clicking the profile segment connects or disconnects, like the GUI switch.
                self.hit(
                    Rect::new(area.x + start as u16, area.y, (end - start) as u16, 1)
                        .intersection(area),
                    Target::Connect,
                );
                f.render_widget(Paragraph::new(Line::from(left)), area);
                return;
            }
        }
    }
    fn render_page_bar(&self, f: &mut Frame<'_>, area: Rect) {
        let mut x = area.x + 1;
        let compact = area.width < 72;
        for (index, page) in PAGES.iter().enumerate() {
            let label = if compact && index != self.page {
                format!(" {} ", index + 1)
            } else {
                format!(" {} {} ", index + 1, t(page))
            };
            let width = Line::from(label.as_str()).width() as u16;
            let rect = Rect::new(x, area.y, width, 1).intersection(area);
            f.render_widget(
                Paragraph::new(label).style(if index == self.page {
                    Style::default()
                        .bg(BLUE)
                        .fg(Color::White)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(MUTED)
                }),
                rect,
            );
            self.hit(rect, Target::Page(index));
            x = x.saturating_add(width + 1);
        }
        let right = t(": / Ctrl-K commands · ? help");
        let width = Line::from(right).width() as u16;
        if x + width + 2 < area.right() {
            f.render_widget(
                Paragraph::new(right).style(Style::default().fg(Color::DarkGray)),
                Rect::new(area.right() - width - 1, area.y, width, 1),
            );
        }
    }
    fn hint(&self) -> String {
        let text = if self.palette.is_some() {
            "↑↓ select · Enter run · Esc close · type to filter"
        } else if self.recovery.is_some() {
            "c retry · b cancel · ↑↓ scroll"
        } else if self.prompt.as_ref().is_some_and(Prompt::confirmation) {
            "Enter confirm · Esc cancel"
        } else if self.prompt.is_some() {
            "Enter confirm · Esc cancel · Ctrl-U clear"
        } else if self.editor.is_some() {
            "Ctrl-O import · F4 external editor · F5 read draft · Ctrl-C leave, keep draft"
        } else if self.viewer.is_some() {
            "[ ] source · / search · f filter · v level · Space pause · Esc close · ? all keys"
        } else if self.runtime_picker.is_some() {
            "↑↓ member · Enter select · / find · t test · Esc groups"
        } else {
            match self.page {
                OVERVIEW => "c connect/disconnect · m mode · S system proxy · T TUN · A LAN · r apply · e edit · L logs · N network tools",
                PROXIES => "↑↓ group · Enter members · / find · t test · D node tests · U update subscriptions",
                CONNECTIONS => "↑↓ select · / filter · x close · X close all · Esc clear filter",
                RULES => "t test a route · / search rules · e edit rules",
                LOGS => "Enter open · [ proxy log · ] rule core log · F7 TUI log",
                _ => "↑↓ select · / find · Space/c connect · Enter actions · e edit · n new · Tab proxy/rule cores",
            }
        };
        format!(" {}", t(text))
    }
    pub(super) fn render_palette(&self, f: &mut Frame<'_>) {
        let Some(palette) = &self.palette else {
            return;
        };
        let entries = self.palette_matches();
        let area = f.area();
        let width = area.width.saturating_sub(4).min(76);
        let height = (entries.len() as u16 + 4).clamp(6, 22).min(area.height);
        let popup = Rect::new(
            area.x + (area.width - width) / 2,
            area.y + area.height.saturating_sub(height) / 3,
            width,
            height,
        );
        f.render_widget(Clear, popup);
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(ratatui::widgets::BorderType::Rounded)
            .border_style(Style::default().fg(BLUE))
            .title(format!(
                " {} ",
                t(if palette.profiles {
                    "Switch profile"
                } else {
                    "Commands"
                })
            ))
            .title_bottom(format!(
                " {} ",
                t("Right column: the same key on this page")
            ));
        let inner = block.inner(popup);
        f.render_widget(block, popup);
        let rows = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(inner);
        f.render_widget(Paragraph::new(format!("› {}▏", palette.query)), rows[0]);
        let label_width = rows[2].width.saturating_sub(16) as usize;
        let items: Vec<_> = entries
            .iter()
            .map(|e| {
                let style = if e.unavailable.is_some() {
                    Style::default().fg(Color::DarkGray)
                } else {
                    Style::default()
                };
                let mut label = e.label.clone();
                if let Some(reason) = e.unavailable {
                    label = format!("{label} · {}", t(reason));
                }
                let used = Line::from(label.as_str()).width();
                ListItem::new(Line::from(vec![
                    Span::styled(format!("{:<9}", t(e.scope)), Style::default().fg(MUTED)),
                    Span::styled(label, style),
                    Span::raw(" ".repeat(label_width.saturating_sub(used + 1).max(1))),
                    Span::styled(e.key, Style::default().fg(Color::Cyan)),
                ]))
            })
            .collect();
        let empty = items.is_empty();
        let mut state = ListState::default().with_selected(
            (!empty).then_some(palette.selected.min(entries.len().saturating_sub(1))),
        );
        f.render_stateful_widget(
            List::new(items)
                .highlight_style(crate::theme::selected())
                .highlight_symbol("› "),
            rows[2],
            &mut state,
        );
        if empty {
            f.render_widget(Paragraph::new(t("No matching command")), rows[2]);
        }
        self.hits.borrow_mut().clear();
        for visible in 0..rows[2].height as usize {
            let index = visible + state.offset();
            if index >= entries.len() {
                break;
            }
            self.hit(
                Rect::new(rows[2].x, rows[2].y + visible as u16, rows[2].width, 1),
                Target::Palette(index),
            );
        }
    }
    /// Tab-like title: the shown list is highlighted, the other one muted.
    pub(super) fn profiles_title(&self) -> Line<'static> {
        let proxies = self.proxies_count();
        let rules = self.rows.len() - proxies;
        let tab = |label: String, on: bool| {
            Span::styled(
                format!(" {label} "),
                if on {
                    crate::theme::selected()
                } else {
                    Style::default().fg(MUTED)
                },
            )
        };
        Line::from(vec![
            Span::raw(" "),
            tab(tr!("Profiles {}", proxies), self.kind == Kind::Proxy),
            Span::raw(" "),
            tab(tr!("Rule cores {}", rules), self.kind == Kind::Rule),
            Span::styled(
                format!("  {} ", t("Tab switch")),
                Style::default().fg(MUTED),
            ),
        ])
    }
}
