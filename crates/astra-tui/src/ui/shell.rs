//! Shared frame matching the GUI: status bar, page bar, hint line and command palette.
//! Every page except Profiles follows the active profile, the selected proxy instance.
use super::dashboard::{Target, BLUE, MUTED};
use super::*;
use astra_model::runtime::Command;
use astra_model::runtime::Mode;
use ratatui::{layout::Rect, style::Modifier};

pub(super) const OVERVIEW: usize = 0;
pub(super) const PROXIES: usize = 1;
pub(super) const CONNECTIONS: usize = 2;
pub(super) const RULES: usize = 3;
pub(super) const LOGS: usize = 4;
pub(super) const PROFILES: usize = 5;
pub(super) const SETTINGS: usize = 6;
pub(super) const PAGES: [&str; 7] = [
    "Overview",
    "Proxies",
    "Connections",
    "Rules",
    "Logs",
    "Profiles",
    "Settings",
];

#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub(super) enum PaletteFocus {
    #[default]
    Search,
    Type,
    List,
}
#[derive(Clone)]
pub(super) struct Palette {
    pub(super) query: String,
    pub(super) cursor: usize,
    pub(super) selected: usize,
    /// Opened with Ctrl-P: choose a proxy profile or a rule core.
    pub(super) profiles: bool,
    pub(super) profile_kind: Kind,
    pub(super) focus: PaletteFocus,
    pub(super) viewport_height: std::cell::Cell<u16>,
}
impl Default for Palette {
    fn default() -> Self {
        Self {
            query: String::new(),
            cursor: 0,
            selected: 0,
            profiles: false,
            profile_kind: Kind::Proxy,
            focus: PaletteFocus::Search,
            viewport_height: std::cell::Cell::new(1),
        }
    }
}
impl Palette {
    pub(super) fn profiles(kind: Kind) -> Self {
        Self {
            profiles: true,
            profile_kind: kind,
            ..Default::default()
        }
    }
    fn byte(&self) -> usize {
        self.query
            .char_indices()
            .nth(self.cursor)
            .map_or(self.query.len(), |(index, _)| index)
    }
    pub(super) fn insert(&mut self, text: &str) {
        self.focus = PaletteFocus::Search;
        let text: String = text.chars().filter(|c| !c.is_control()).collect();
        self.query.insert_str(self.byte(), &text);
        self.cursor += text.chars().count();
        self.selected = 0;
    }
}

#[derive(Clone)]
enum Run {
    Page(usize),
    /// Navigate, then deliver the key there.
    Key(usize, KeyCode),
    /// Instance operation on the active profile.
    Op(Operation),
    Runtime(Command),
    Activate {
        name: String,
        kind: Kind,
    },
    Connect,
    Validate,
    Update(update::Command),
    Registered(commands::Id),
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
        if self.kind == Kind::Server {
            return None;
        }
        if let Some(name) = &self.active_profile {
            self.rows
                .iter()
                .find(|r| r.instance.kind == Kind::Proxy && &r.instance.name == name)
        } else {
            self.rows
                .iter()
                .filter(|r| r.instance.kind == Kind::Proxy)
                .nth(self.selections[0])
        }
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
        self.editor.as_ref().is_none_or(Editor::browsing)
            && self.prompt.is_none()
            && self.recovery.is_none()
            && self.menu.is_none()
            && self.binding_picker.is_none()
            && self.diagnostics.is_none()
            && self.explanation.is_none()
            && self.help_scroll.is_none()
            && self.palette.is_none()
            && self.monitor.is_none()
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
        self.remember_page(LOGS);
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
        if self.kind == Kind::Server {
            return self.operation(operation);
        }
        if self.active_proxy().is_none() {
            self.message = t("No profile yet · 6 then n to create one").into();
            return Input::Idle;
        }
        let previous = self.selections;
        self.kind = Kind::Proxy;
        if let Some(name) = self.active_proxy().map(|r| r.instance.name.clone()) {
            if let Some(index) = self.rows().iter().position(|r| r.instance.name == name) {
                self.selections[0] = index;
            }
        }
        let input = self.operation(operation);
        self.selections = previous;
        input
    }
    fn rule_operation(&mut self, operation: Operation) -> Input {
        let Some(name) = self.active_rule().map(|r| r.instance.name.clone()) else {
            self.message = t("No rule core for this profile · bind one with b in Profiles").into();
            return Input::Idle;
        };
        let previous = self.selections;
        self.kind = Kind::Rule;
        if let Some(index) = self.rows().iter().position(|r| r.instance.name == name) {
            self.selections[1] = index;
        }
        let input = self.operation(operation);
        self.selections = previous;
        input
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
            KeyCode::Char(c @ '1'..='7') => return Some(self.navigate(c as usize - '1' as usize)),
            KeyCode::Char('p') => {
                self.palette = Some(Palette::profiles(self.kind));
                return Some(Input::Idle);
            }
            _ => {}
        }
        if self.page == PROFILES || self.viewer.is_some() {
            return None;
        }
        if self.kind == Kind::Server {
            return self.server_key(key);
        }
        Some(match (self.page, key.code) {
            (_, KeyCode::Char('q')) => Input::Quit,
            (_, KeyCode::Char('c')) => self.toggle_active(),
            (_, KeyCode::Char('r')) => self.active_operation(Operation::Reload),
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
    pub(super) fn rule_search(&mut self) -> Input {
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
        self.find_direction(page, query, false)
    }
    pub(super) fn find_direction(&mut self, page: usize, query: &str, backwards: bool) -> bool {
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
            let candidate = if backwards {
                found
                    .iter()
                    .rev()
                    .find(|(g, m, _)| order(*g, *m) < current)
                    .or(found.last())
            } else {
                found
                    .iter()
                    .find(|(g, m, _)| order(*g, *m) > current)
                    .or(found.first())
            };
            let Some((g, m, name)) = candidate.cloned() else {
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
        let mut matches: Vec<_> = candidates
            .filter(|(_, _, row)| hit(&text(row)))
            .map(|(kind, index, _)| (kind, index))
            .collect();
        if backwards {
            matches.reverse();
            if matches.first() == Some(&(self.kind, selected)) && matches.len() > 1 {
                matches.rotate_left(1);
            }
        }
        let Some((kind, index)) = matches.into_iter().next() else {
            return false;
        };
        self.kind = kind;
        let slot = self.index();
        self.selections[slot] = index;
        true
    }
    /// Proxy log first; `]` (or a second press) moves to the profile's rule core.
    pub(super) fn log_source(&self, proxy: bool) -> Option<String> {
        if self.kind == Kind::Server {
            return self.selected().map(|row| row.instance.name.clone());
        }
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
            KeyCode::Char(c @ '1'..='7') => Some(self.navigate(c as usize - '1' as usize)),
            KeyCode::Char(':') => {
                self.palette = Some(Palette::default());
                Some(Input::Idle)
            }
            _ => None,
        }
    }

    fn palette_entries(&self) -> Vec<Entry> {
        let mut entries = Vec::new();
        let chooser = self.palette.as_ref().filter(|palette| palette.profiles);
        let kind = chooser.map_or(self.kind, |palette| palette.profile_kind);
        let profiles: Vec<_> = self
            .rows
            .iter()
            .filter(|r| r.instance.kind == kind)
            .collect();
        for row in profiles {
            entries.push(Entry {
                scope: match kind {
                    Kind::Proxy => "Profile",
                    Kind::Rule => "Rules",
                    Kind::Server => "Server",
                },
                label: format!(
                    "{} · {}",
                    row.instance.name,
                    t(if row.pid.is_some() {
                        "Running"
                    } else {
                        "Stopped"
                    })
                ),
                key: if chooser.is_some() { "" } else { "p" },
                run: Run::Activate {
                    name: row.instance.name.clone(),
                    kind,
                },
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
                key: ["1", "2", "3", "4", "5", "6", "7"][index],
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
            "",
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
            "",
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
            "Profile",
            "Export configuration",
            "E",
            Run::Op(Operation::ExportConfig),
            no_profile,
        );
        for command in update::Command::ALL {
            add(
                "Updates",
                command.label(),
                "",
                Run::Update(command),
                command.unavailable(self.update_status.as_ref()),
            );
        }
        for spec in commands::SPECS {
            add(
                "General",
                spec.label,
                spec.key,
                Run::Registered(spec.id),
                self.command_available(spec.id),
            );
        }
        entries
    }
    pub(super) fn command_help(&self) -> String {
        if let Some(text) = &self.help_override {
            return text.clone();
        }
        let mut result = commands::help();
        result.push_str("\nPage and profile actions · F2 opens these commands\n");
        for entry in self
            .palette_entries()
            .into_iter()
            .filter(|entry| entry.scope != "General")
        {
            result.push_str(&format!(
                "{}  {}{}\n",
                entry.key,
                entry.label,
                entry
                    .unavailable
                    .map(|reason| format!(" ({})", t(reason)))
                    .unwrap_or_default()
            ));
        }
        result
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
        if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
            let regions: &[PaletteFocus] = if palette.profiles {
                &[PaletteFocus::Search, PaletteFocus::Type, PaletteFocus::List]
            } else {
                &[PaletteFocus::Search, PaletteFocus::List]
            };
            let index = regions
                .iter()
                .position(|focus| *focus == palette.focus)
                .unwrap_or(0);
            let next = if key.code == KeyCode::BackTab {
                (index + regions.len() - 1) % regions.len()
            } else {
                (index + 1) % regions.len()
            };
            palette.focus = regions[next];
            return Input::Idle;
        }
        if palette.profiles && palette.focus == PaletteFocus::Type {
            match key.code {
                KeyCode::Left | KeyCode::Up => {
                    palette.profile_kind = Kind::Proxy;
                    palette.selected = 0;
                }
                KeyCode::Right | KeyCode::Down => {
                    palette.profile_kind = next_kind(palette.profile_kind);
                    palette.selected = 0;
                }
                KeyCode::Enter => palette.focus = PaletteFocus::List,
                KeyCode::Esc => self.palette = None,
                _ => {}
            }
            return Input::Idle;
        }
        let page_size = usize::from(palette.viewport_height.get().max(1));
        match key.code {
            KeyCode::Esc => self.palette = None,
            KeyCode::Char('k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.palette = None
            }
            KeyCode::Down => palette.selected = (palette.selected + 1).min(count.saturating_sub(1)),
            KeyCode::Up => palette.selected = palette.selected.saturating_sub(1),
            KeyCode::PageDown => {
                palette.selected = (palette.selected + page_size).min(count.saturating_sub(1))
            }
            KeyCode::PageUp => palette.selected = palette.selected.saturating_sub(page_size),
            KeyCode::Home if palette.focus == PaletteFocus::List => palette.selected = 0,
            KeyCode::End if palette.focus == PaletteFocus::List => {
                palette.selected = count.saturating_sub(1)
            }
            KeyCode::Left if palette.focus == PaletteFocus::Search => {
                palette.cursor = palette.cursor.saturating_sub(1)
            }
            KeyCode::Right if palette.focus == PaletteFocus::Search => {
                palette.cursor = (palette.cursor + 1).min(palette.query.chars().count())
            }
            KeyCode::Home => palette.cursor = 0,
            KeyCode::End => palette.cursor = palette.query.chars().count(),
            KeyCode::Backspace if palette.focus == PaletteFocus::Search && palette.cursor > 0 => {
                palette.cursor -= 1;
                palette.query.remove(palette.byte());
                palette.selected = 0;
            }
            KeyCode::Delete
                if palette.focus == PaletteFocus::Search
                    && palette.cursor < palette.query.chars().count() =>
            {
                palette.query.remove(palette.byte());
                palette.selected = 0;
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                palette.query.clear();
                palette.cursor = 0;
                palette.selected = 0;
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                palette.insert(&c.to_string());
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
            Run::Page(page) => {
                self.suspend_editor();
                self.navigate(page)
            }
            Run::Key(page, code) => match self.navigate(page) {
                Input::Idle => press(self, code),
                input => input,
            },
            Run::Op(operation) => self.active_operation(operation),
            Run::Runtime(command) => self.runtime_action(command),
            Run::Activate { name, kind } => {
                if let Some(index) = self
                    .rows
                    .iter()
                    .filter(|r| r.instance.kind == kind)
                    .position(|r| r.instance.name == name)
                {
                    if kind == Kind::Proxy {
                        self.activate_profile(name);
                        if self.page == PROFILES {
                            self.kind = Kind::Proxy;
                            self.selections[0] = index;
                        }
                    } else {
                        self.navigate(PROFILES);
                        self.kind = kind;
                        self.selections[kind_index(kind)] = index;
                    }
                }
                Input::Idle
            }
            Run::Connect => self.toggle_active(),
            Run::Validate => match self.active_proxy() {
                Some(row) => Input::Run(Action::Validate(row.instance.name.clone()), None),
                None => Input::Idle,
            },
            Run::Update(command) => command.run(self),
            Run::Registered(id) => self.run_command(id),
        }
    }

    /// Status bar, page bar and hint line; returns the page content area.
    pub(super) fn render_frame(&self, f: &mut Frame<'_>) -> Rect {
        let area = f.area();
        let header_height = self.overview_header_height(area.width);
        let merged = self.page == OVERVIEW
            && self.active_proxy().is_some()
            && area.height >= header_height.saturating_add(4);
        let rows = if merged {
            Layout::vertical([
                Constraint::Length(header_height),
                Constraint::Min(0),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(area)
        } else {
            Layout::vertical([
                Constraint::Length(if self.kind == Kind::Server { 1 } else { 2 }),
                Constraint::Length(1),
                Constraint::Min(0),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .split(area)
        };
        let (content, hint) = if merged {
            self.render_overview_header(f, rows[0]);
            (rows[1], rows[2])
        } else {
            self.render_status_bar(f, rows[0]);
            self.render_page_bar(f, rows[1]);
            (rows[2], rows[3])
        };
        // This page's keys, whole entries only; results live in the status line below.
        let hint_text = format!("[{}] Tab focus · {}", self.focus_label(), self.hint());
        let global = Line::from(crate::theme::key_hints(t("F2 / Ctrl-K commands · F1 help")));
        let global_width = global.width() as u16;
        let hints = if hint.width >= global_width + 22 {
            f.render_widget(
                Paragraph::new(global),
                Rect::new(
                    hint.right() - global_width,
                    hint.y,
                    global_width,
                    hint.height,
                ),
            );
            Rect::new(hint.x, hint.y, hint.width - global_width - 2, hint.height)
        } else {
            hint
        };
        let spans = crate::theme::key_hints_fit(&hint_text, hints.width as usize, t("F1 more"));
        f.render_widget(Paragraph::new(Line::from(spans)), hints);
        content
    }
    fn render_status_bar(&self, f: &mut Frame<'_>, area: Rect) {
        if self.kind == Kind::Server {
            self.render_server_header(f, area);
            return;
        }
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
                if level == 0 && area.height >= 2 {
                    right = self.rate_spans(0, area.width < 110);
                }
            }
            let used = width(&left) + width(&right);
            if used <= area.width as usize || level == LAST {
                if !right.is_empty() {
                    left.push(Span::raw(" ".repeat(area.width as usize - used)));
                    left.extend(right);
                    Self::render_right_aligned(
                        f,
                        Rect::new(area.x, area.y + 1, area.width, 1),
                        vec![Span::styled(format!(" {}", self.runtime_health()), muted)],
                        self.rate_spans(1, area.width < 110),
                    );
                }
                // Clicking the profile segment connects or disconnects, like the GUI switch.
                self.hit(
                    Rect::new(area.x + start as u16, area.y, (end - start) as u16, 1)
                        .intersection(area),
                    Target::Connect,
                );
                f.render_widget(
                    Paragraph::new(Line::from(left)),
                    Rect::new(area.x, area.y, area.width, area.height.min(1)),
                );
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
                Paragraph::new(label).style(
                    if self.focus.region == focus::Region::Tabs && index == self.focus.cursor {
                        Style::default()
                            .bg(Color::Cyan)
                            .fg(Color::Black)
                            .add_modifier(Modifier::BOLD)
                    } else if index == self.page {
                        Style::default()
                            .bg(BLUE)
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(MUTED)
                    },
                ),
                rect,
            );
            self.hit(rect, Target::Page(index));
            x = x.saturating_add(width + 1);
        }
    }
    fn hint(&self) -> String {
        let text = if self.palette.is_some() {
            "↑↓ select · Enter run · Esc close · type to filter"
        } else if self.recovery.is_some() {
            "c retry · b cancel · ↑↓ scroll"
        } else if self.prompt.as_ref().is_some_and(Prompt::confirmation) {
            "Tab / ←→ choose · Enter apply · Esc cancel"
        } else if self.prompt.is_some() {
            "Enter confirm · Esc cancel · Ctrl-U clear"
        } else if self.editor.is_some() {
            "Enter edit · Space toggle · Ctrl-S save · Ctrl-Z/Y undo/redo · Esc retain draft"
        } else if self.viewer.is_some() {
            "End bottom · [ ] source · / search · f filter · v level · Space pause · Esc close · ? all keys"
        } else if self.runtime_picker.is_some() {
            "↑↓ member · Enter select · / find · t test · Esc groups"
        } else {
            match self.page {
                OVERVIEW => "e edit · L logs · ? help",
                PROXIES => "↑↓ group · Enter members · / find · t test · D node tests · U update subscriptions",
                CONNECTIONS => "↑↓ select · / filter · x close · X close all · Esc clear filter",
                RULES => "t test a route · / search rules · e edit rules",
                LOGS => "Enter open · [ proxy log · ] rule core log · F7 TUI log",
                SETTINGS => "↑↓ select · ←→ change · Enter next · Delete default",
                _ => "↑↓ browse · Ctrl-P activate profile · Enter actions · e edit · Insert new · Delete remove",
            }
        };
        format!(" {}", t(text))
    }
    pub(super) fn render_palette(&self, f: &mut Frame<'_>) {
        let Some(palette) = &self.palette else {
            return;
        };
        let before = f.buffer_mut().clone();
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
                t(if palette.profiles {
                    "Tab: search / type / list · arrows choose · Enter select"
                } else {
                    "Right column: the same key on this page"
                })
            ));
        let inner = block.inner(popup);
        f.render_widget(block, popup);
        let rows = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(0),
        ])
        .split(inner);
        palette.viewport_height.set(rows[2].height);
        let cursor = palette.byte();
        f.render_widget(
            Paragraph::new(format!(
                "› {}{}{}",
                &palette.query[..cursor],
                if palette.focus == PaletteFocus::Search {
                    "▏"
                } else {
                    ""
                },
                &palette.query[cursor..]
            ))
            .style(if palette.focus == PaletteFocus::Search {
                Style::default().fg(Color::Cyan)
            } else {
                Style::default()
            }),
            rows[0],
        );
        let content_width = rows[2].width.saturating_sub(2) as usize;
        let scope_width = if content_width >= 40 { 9 } else { 0 };
        let key_width = if content_width >= 40 {
            entries
                .iter()
                .map(|e| Line::from(e.key).width())
                .max()
                .unwrap_or(0)
                .min(18)
        } else {
            0
        };
        let label_width =
            content_width.saturating_sub(scope_width + key_width + usize::from(key_width > 0));
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
                ListItem::new(Line::from(vec![
                    Span::styled(
                        palette_column(t(e.scope), scope_width),
                        Style::default().fg(MUTED),
                    ),
                    Span::styled(palette_column(&label, label_width), style),
                    Span::raw(if key_width > 0 { " " } else { "" }),
                    Span::styled(
                        palette_column(e.key, key_width),
                        Style::default().fg(Color::Cyan),
                    ),
                ]))
            })
            .collect();
        let empty = items.is_empty();
        if palette.profiles {
            let type_style = |kind| {
                if palette.profile_kind == kind {
                    if palette.focus == PaletteFocus::Type {
                        crate::theme::selected()
                    } else {
                        Style::default().fg(Color::Cyan)
                    }
                } else {
                    Style::default().fg(MUTED)
                }
            };
            f.render_widget(
                Paragraph::new(Line::from(vec![
                    Span::raw(format!("{}: ", t("Type"))),
                    Span::styled(
                        format!(
                            "{}{}{}",
                            if palette.profile_kind == Kind::Proxy {
                                "["
                            } else {
                                " "
                            },
                            t("Proxy"),
                            if palette.profile_kind == Kind::Proxy {
                                "]"
                            } else {
                                " "
                            }
                        ),
                        type_style(Kind::Proxy),
                    ),
                    Span::raw("  "),
                    Span::styled(
                        format!(
                            "{}{}{}",
                            if palette.profile_kind == Kind::Rule {
                                "["
                            } else {
                                " "
                            },
                            t("Rules"),
                            if palette.profile_kind == Kind::Rule {
                                "]"
                            } else {
                                " "
                            }
                        ),
                        type_style(Kind::Rule),
                    ),
                ])),
                rows[1],
            );
        } else if let Some(reason) = entries
            .get(palette.selected)
            .and_then(|entry| entry.unavailable)
        {
            f.render_widget(
                Paragraph::new(palette_column(t(reason), rows[1].width as usize))
                    .style(Style::default().fg(Color::Yellow)),
                rows[1],
            );
        }
        let mut state = ListState::default().with_selected(
            (!empty).then_some(palette.selected.min(entries.len().saturating_sub(1))),
        );
        let geometry = astra_tui_kit::SelectableList::render(
            f,
            List::new(items)
                .highlight_style(crate::theme::selected())
                .highlight_symbol(if palette.focus == PaletteFocus::List {
                    "› "
                } else {
                    "  "
                }),
            rows[2],
            rows[2],
            &mut state,
            entries.len(),
            1,
        );
        if empty {
            f.render_widget(Paragraph::new(t("No matching command")), rows[2]);
        }
        self.hits.borrow_mut().clear();
        self.hits.borrow_mut().bind_rows(&geometry, Target::Palette);
        self.animations
            .surface(f, super::animation::Kind::Palette, popup, &before);
    }
    /// Tab-like title: the shown list is highlighted, the other one muted.
    pub(super) fn profiles_title(&self) -> Line<'static> {
        let proxies = self.proxies_count();
        let rules = self
            .rows
            .iter()
            .filter(|r| r.instance.kind == Kind::Rule)
            .count();
        let servers = self
            .rows
            .iter()
            .filter(|r| r.instance.kind == Kind::Server)
            .count();
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
            Span::raw(" "),
            tab(format!("Servers {servers}"), self.kind == Kind::Server),
            Span::styled(
                format!("  {} ", t("Tab controls · Enter switch")),
                Style::default().fg(MUTED),
            ),
        ])
    }
}

/// Pad terminal columns rather than Unicode scalar count (Chinese scopes are double width).
fn palette_column(text: &str, width: usize) -> String {
    astra_tui_kit::text::fit_column(text, width)
}

#[cfg(test)]
mod palette_layout_tests {
    use super::*;
    fn profile_ui() -> Ui {
        let mut ui = Ui::new(Session::default());
        for (name, kind) in [
            ("proxy-a", Kind::Proxy),
            ("proxy-b", Kind::Proxy),
            ("rules-a", Kind::Rule),
            ("rules-b", Kind::Rule),
        ] {
            ui.rows.push(Row {
                instance: Instance {
                    name: name.into(),
                    kind,
                    config: format!("{name}.toml").into(),
                },
                pid: None,
                state: "Stopped".into(),
                uptime: 0,
                changed: false,
            });
        }
        ui.active_profile = Some("proxy-b".into());
        ui
    }
    fn press(ui: &mut Ui, code: KeyCode) -> Input {
        ui.key(event::KeyEvent::new(code, KeyModifiers::NONE))
    }
    #[test]
    fn configuration_type_control_selects_rules_without_changing_active_proxy() {
        let mut ui = profile_ui();
        ui.editor =
            Some(Editor::new("proxy-a".into(), "standalone=true\n".into()).with_kind(Kind::Proxy));
        ui.key(event::KeyEvent::new(
            KeyCode::Char('p'),
            KeyModifiers::CONTROL,
        ));
        assert_eq!(ui.palette_matches().len(), 2);
        press(&mut ui, KeyCode::Tab);
        assert_eq!(ui.palette.as_ref().unwrap().focus, PaletteFocus::Type);
        press(&mut ui, KeyCode::Right);
        assert_eq!(ui.palette.as_ref().unwrap().profile_kind, Kind::Rule);
        assert!(ui
            .palette_matches()
            .iter()
            .all(|entry| entry.label.starts_with("rules-")));
        press(&mut ui, KeyCode::Enter);
        assert_eq!(ui.palette.as_ref().unwrap().focus, PaletteFocus::List);
        press(&mut ui, KeyCode::Down);
        press(&mut ui, KeyCode::Enter);
        assert!(ui.palette.is_none());
        assert_eq!(ui.page, PROFILES);
        assert_eq!(ui.kind, Kind::Rule);
        assert_eq!(ui.selected().unwrap().instance.name, "rules-b");
        assert_eq!(ui.active_proxy().unwrap().instance.name, "proxy-b");
        assert_eq!(ui.drafts.len(), 1);
        ui.run_command(commands::Id::Profiles);
        press(&mut ui, KeyCode::Tab);
        press(&mut ui, KeyCode::Left);
        press(&mut ui, KeyCode::Enter);
        press(&mut ui, KeyCode::Enter);
        assert_eq!(ui.active_proxy().unwrap().instance.name, "proxy-a");
        assert_eq!(ui.kind, Kind::Proxy);
    }
    #[test]
    fn palette_page_keys_follow_visible_rows_after_resize() {
        use ratatui::backend::TestBackend;
        let mut ui = profile_ui();
        for index in 0..30 {
            let mut row = ui.rows[0].clone();
            row.instance.name = format!("extra-{index}");
            ui.rows.push(row);
        }
        for profiles in [false, true] {
            ui.palette = Some(if profiles {
                Palette::profiles(Kind::Proxy)
            } else {
                Palette::default()
            });
            for height in [10, 6] {
                let mut terminal = Terminal::new(TestBackend::new(70, height)).unwrap();
                terminal.draw(|frame| ui.render_palette(frame)).unwrap();
                let visible = usize::from(ui.palette.as_ref().unwrap().viewport_height.get());
                assert_eq!(visible, usize::from(height - 4));
                let before = ui.palette.as_ref().unwrap().selected;
                press(&mut ui, KeyCode::PageDown);
                assert_eq!(ui.palette.as_ref().unwrap().selected, before + visible);
                press(&mut ui, KeyCode::PageUp);
                assert_eq!(ui.palette.as_ref().unwrap().selected, before);
            }
        }
    }
    #[test]
    fn multilingual_columns_keep_the_shortcut_column_inside_the_popup() {
        for width in [0, 1, 9, 24, 51] {
            for text in ["通用", "Command palette", "右列为本页可直接使用的按键"] {
                assert_eq!(Line::from(palette_column(text, width)).width(), width);
            }
        }
        let line = Line::from(vec![
            Span::raw(palette_column("通用", 9)),
            Span::raw(palette_column("命令面板", 51)),
            Span::raw(" "),
            Span::raw(palette_column("Ctrl-K / F2", 11)),
        ]);
        assert_eq!(line.width(), 72);
    }
}
