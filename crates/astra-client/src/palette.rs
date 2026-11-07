//! Shared command palette entries, localized matching and typed dispatch.
use crate::{
    commands,
    editor::Editor,
    i18n::{language, t, translate, Language},
    navigation::*,
    protocol::{Action, Kind},
    state::*,
    update,
};
use astra_model::runtime::{Command, Mode};
#[derive(Clone, Copy, Default)]
pub enum PaletteMode {
    #[default]
    Commands,
    Profiles(Kind),
}
#[derive(Clone)]
pub enum Intent {
    Page(usize),
    TestSelectedGroup,
    CloseAllConnections,
    ExplainRoute,
    EditRules,
    NewInstance,
    /// Instance operation on the active profile.
    Op(Operation),
    Runtime(Command),
    Activate {
        name: String,
        kind: Kind,
    },
    Connect,
    ForceStart,
    NodeExport(bool),
    NodeImport,
    Validate,
    Update(update::Command),
    Registered(commands::Id),
}

pub struct Entry {
    pub scope: &'static str,
    pub label: String,
    pub key: &'static str,
    pub run: Intent,
    pub unavailable: Option<&'static str>,
}

impl Entry {
    pub fn requires_editor(&self) -> bool {
        match &self.run {
            Intent::NodeImport | Intent::NodeExport(_) => true,
            Intent::Registered(id) => commands::SPECS
                .iter()
                .any(|spec| spec.id == *id && spec.scope == commands::Scope::Editor),
            _ => false,
        }
    }
}
impl Client {
    pub fn palette_matches_for_view(
        &self,
        mode: PaletteMode,
        query: &str,
        page: usize,
        editor_active: bool,
    ) -> Vec<Entry> {
        self.palette_matches_for_page(mode, query, page)
            .into_iter()
            .filter(|entry| editor_active || !entry.requires_editor())
            .collect()
    }
    pub fn run_palette_entry_for_view(
        &mut self,
        entry: Entry,
        page: usize,
        editor_active: bool,
    ) -> Input {
        if entry.requires_editor() && !editor_active {
            return Input::Idle;
        }
        self.page = page;
        if !editor_active && entry.unavailable.is_none() {
            if let Intent::Registered(id) = &entry.run {
                return self.run_page_command(page, *id);
            }
        }
        self.run_palette_entry(entry)
    }

    pub fn palette_entries_in(&self, language: Language, mode: PaletteMode) -> Vec<Entry> {
        self.palette_entries_for_page(language, mode, self.page)
    }
    pub fn palette_entries_for_page(
        &self,
        language: Language,
        mode: PaletteMode,
        page: usize,
    ) -> Vec<Entry> {
        let mut entries = Vec::new();
        let chooser = match mode {
            PaletteMode::Commands => None,
            PaletteMode::Profiles(kind) => Some(kind),
        };
        let kind = chooser.unwrap_or(self.kind);
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
                    translate(
                        if row.pid.is_some() {
                            "Running"
                        } else {
                            "Stopped"
                        },
                        language,
                    )
                ),
                key: if chooser.is_some() { "" } else { "p" },
                run: Intent::Activate {
                    name: row.instance.name.clone(),
                    kind,
                },
                unavailable: None,
            });
        }
        if chooser.is_some() {
            return entries;
        }
        for (index, page) in PAGES.iter().enumerate() {
            entries.push(Entry {
                scope: "Go to",
                label: translate(page, language).into(),
                key: ["1", "2", "3", "4", "5", "6", "7"][index],
                run: Intent::Page(index),
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
                label: translate(label, language).into(),
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
            Intent::Connect,
            no_profile,
        );
        add(
            "Run",
            "Apply changes (hot reload)",
            "r",
            Intent::Op(Operation::Reload),
            not_running,
        );
        add(
            "Run",
            "Full restart",
            "",
            Intent::Op(Operation::Restart),
            not_running,
        );
        let force_target = if page == PROFILES {
            self.selected()
        } else {
            profile
        };
        if let Some(row) =
            force_target.filter(|row| row.pid.is_none() && row.instance.kind != Kind::Server)
        {
            add(
                "Run",
                &format!(
                    "{} · {}",
                    translate("Force start", language),
                    row.instance.name
                ),
                "",
                Intent::ForceStart,
                None,
            );
        }
        for (label, mode) in [
            ("Mode: rule", Mode::Rule),
            ("Mode: global", Mode::Global),
            ("Mode: direct", Mode::Direct),
        ] {
            add(
                "Run",
                label,
                "m",
                Intent::Runtime(Command::SetMode(mode)),
                live,
            );
        }
        if let Some(snapshot) = self.live() {
            add(
                "Takeover",
                "Toggle system proxy",
                "S",
                Intent::Runtime(Command::SystemProxy(!snapshot.system_proxy)),
                None,
            );
            add(
                "Takeover",
                "Toggle TUN (administrator)",
                "T",
                Intent::Runtime(Command::Tun(!snapshot.tun)),
                None,
            );
            add(
                "Takeover",
                "Toggle LAN access",
                "A",
                Intent::Runtime(Command::AllowLan(!snapshot.allow_lan)),
                None,
            );
        } else {
            for (label, key) in [
                ("Toggle system proxy", "S"),
                ("Toggle TUN (administrator)", "T"),
                ("Toggle LAN access", "A"),
            ] {
                add("Takeover", label, key, Intent::Page(OVERVIEW), live);
            }
        }
        add(
            "Takeover",
            "Reset runtime state",
            "",
            Intent::Runtime(Command::Reset),
            live,
        );
        add(
            "Proxies",
            "Test selected group",
            "t",
            Intent::TestSelectedGroup,
            live,
        );
        add(
            "Proxies",
            "Node tests · exit IP",
            "D",
            Intent::Op(Operation::Diagnostics),
            no_profile,
        );
        add(
            "Proxies",
            "Network tools · DNS / TCP / ICMP",
            "N",
            Intent::Op(Operation::NetTools),
            no_profile,
        );
        add(
            "Proxies",
            "Update subscriptions",
            "U",
            Intent::Op(Operation::UpdateSubscriptions),
            no_profile,
        );
        add(
            "Connections",
            "Close all connections",
            "X",
            Intent::CloseAllConnections,
            live,
        );
        add("Rules", "Test a route", "t", Intent::ExplainRoute, no_rule);
        add("Rules", "Edit rules", "e", Intent::EditRules, no_rule);
        add(
            "Profile",
            "Edit profile",
            "e",
            Intent::Op(Operation::Edit),
            no_profile,
        );
        add("Profile", "New instance", "n", Intent::NewInstance, None);
        add(
            "Profile",
            "Validate configuration",
            "v",
            Intent::Validate,
            no_profile,
        );
        add(
            "Profile",
            "Startup bindings",
            "b",
            Intent::Op(Operation::Bind),
            no_profile,
        );
        add(
            "Profile",
            "Configuration path",
            "",
            Intent::Op(Operation::Config),
            no_profile,
        );
        add(
            "Profile",
            "Copy configuration",
            "y",
            Intent::Op(Operation::Copy),
            no_profile,
        );
        add(
            "Profile",
            "Export configuration",
            "E",
            Intent::Op(Operation::ExportConfig),
            no_profile,
        );
        if self.kind == Kind::Proxy && self.editor.as_ref().is_some_and(Editor::browsing) {
            add("Nodes", "Import nodes", "Ctrl-I", Intent::NodeImport, None);
            let unavailable = self
                .editor
                .as_ref()
                .unwrap()
                .selected_node_tags()
                .err()
                .map(|_| "Select a node to export");
            add(
                "Nodes",
                "Export selected nodes (TOML)",
                "Ctrl-E",
                Intent::NodeExport(false),
                unavailable,
            );
            add(
                "Nodes",
                "Export selected nodes (share links)",
                "Ctrl-Shift-E",
                Intent::NodeExport(true),
                unavailable,
            );
        }
        for command in update::Command::ALL {
            add(
                "Updates",
                command.label(),
                "",
                Intent::Update(command),
                command.unavailable(self.update_status.as_ref()),
            );
        }
        for spec in commands::SPECS {
            add(
                "General",
                spec.label,
                spec.key,
                Intent::Registered(spec.id),
                self.command_available(spec.id),
            );
        }
        entries
    }
    pub fn palette_matches(&self, mode: PaletteMode, query: &str) -> Vec<Entry> {
        self.palette_matches_for_page(mode, query, self.page)
    }
    pub fn palette_matches_for_page(
        &self,
        mode: PaletteMode,
        query: &str,
        page: usize,
    ) -> Vec<Entry> {
        let query = query.to_lowercase();
        let current_language = language();
        let entries = self.palette_entries_for_page(current_language, mode, page);
        if query.is_empty() {
            return entries;
        }
        let matches = |entry: &Entry, language| {
            format!(
                "{} {} {}",
                translate(entry.scope, language),
                entry.label,
                entry.key
            )
            .to_lowercase()
            .contains(&query)
        };
        if current_language == Language::English {
            return entries
                .into_iter()
                .filter(|entry| matches(entry, Language::English))
                .collect();
        }
        // The same UI state produces the same actions in either language. Keep
        // localized entries for display and dispatch; English is a search alias.
        entries
            .into_iter()
            .zip(self.palette_entries_for_page(Language::English, mode, page))
            .filter_map(|(entry, english)| {
                (matches(&entry, current_language) || matches(&english, Language::English))
                    .then_some(entry)
            })
            .collect()
    }
    pub fn command_help(&self) -> String {
        let mut result = commands::help();
        result.push_str("\nPage and profile actions · F2 opens these commands\n");
        for entry in self
            .palette_entries_in(language(), PaletteMode::Commands)
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
    pub fn run_palette_entry(&mut self, entry: Entry) -> Input {
        if let Some(reason) = entry.unavailable {
            self.message = t(reason).into();
            return Input::Idle;
        }
        self.run_intent(entry.run)
    }
    pub fn run_intent(&mut self, intent: Intent) -> Input {
        match intent {
            Intent::Page(page) => {
                self.suspend_editor();
                self.navigate(page)
            }
            Intent::TestSelectedGroup => {
                self.navigate(PROXIES);
                self.test_selected_group()
            }
            Intent::CloseAllConnections => {
                self.navigate(CONNECTIONS);
                self.close_all_connections()
            }
            Intent::ExplainRoute => {
                self.navigate(RULES);
                self.rule_operation(Operation::Explain)
            }
            Intent::EditRules => {
                self.navigate(RULES);
                self.rule_operation(Operation::Edit)
            }
            Intent::NewInstance => {
                self.navigate(PROFILES);
                self.prompt(Prompt::AddName);
                Input::Idle
            }
            Intent::Op(operation) => self.active_operation(operation),
            Intent::Runtime(command) => self.runtime_action(command),
            Intent::Activate { name, kind } => {
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
            Intent::Connect => self.toggle_active(),
            Intent::ForceStart => {
                if self.page == PROFILES {
                    self.operation(Operation::ForceStart)
                } else {
                    self.active_operation(Operation::ForceStart)
                }
            }
            Intent::NodeExport(share) => self.start_node_export(share),
            Intent::NodeImport => {
                self.prompt(Prompt::ImportNode(crate::import::MergeMode::Append));
                Input::Idle
            }
            Intent::Validate => match self.active_proxy() {
                Some(row) => Input::Run(Action::Validate(row.instance.name.clone()), None),
                None => Input::Idle,
            },
            Intent::Update(command) => command.run(self),
            Intent::Registered(id) => self.run_command(id),
        }
    }
}

#[cfg(test)]
#[path = "test/palette_test.rs"]
mod tests;
