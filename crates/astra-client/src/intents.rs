use crate::navigation::*;
use crate::{editor::Editor, tr};
use crate::{i18n::t, protocol::*, state::*};
use anyhow::{Context, Result};

impl Client {
    pub fn operations(&self) -> Vec<Operation> {
        let Some(row) = self.selected() else {
            return Vec::new();
        };
        let mut operations = if row.pid.is_some() {
            vec![Operation::Stop, Operation::Restart, Operation::Reload]
        } else {
            vec![Operation::Start]
        };
        if row.pid.is_none() && row.instance.kind != Kind::Server {
            operations.push(Operation::ForceStart);
        }
        operations.extend([Operation::Edit, Operation::Logs, Operation::ExportConfig]);
        if row.instance.kind != Kind::Server {
            operations.extend([
                Operation::Bind,
                Operation::Config,
                Operation::Copy,
                Operation::ExportTerminal,
            ]);
        }
        if row.instance.kind == Kind::Proxy {
            operations.push(Operation::Diagnostics);
            operations.push(Operation::NetTools);
            operations.push(Operation::UpdateSubscriptions);
        }
        if row.instance.kind == Kind::Rule && row.pid.is_some() {
            operations.push(Operation::Explain);
        }
        if row.pid.is_none() {
            operations.push(Operation::Remove);
        }
        operations
    }
    pub fn switch_core(&mut self, kind: Kind) {
        self.kind = kind;
    }
    pub fn open_bindings(&mut self, name: String, targets: Option<Vec<String>>) {
        let Some(kind) = self
            .rows
            .iter()
            .find(|row| row.instance.name == name)
            .map(|row| row.instance.kind)
        else {
            return;
        };
        if !self.details_supported {
            self.message = "Restart manager to enable startup bindings".into();
            return;
        }
        let choices: Vec<_> = self
            .rows
            .iter()
            .filter(|row| row.instance.kind != kind)
            .map(|row| (row.instance.name.clone(), row.instance.kind))
            .collect();
        if choices.is_empty() {
            self.message = if kind == Kind::Rule {
                "Create a proxy instance first"
            } else {
                "Create a rule instance first"
            }
            .into();
            return;
        }
        let targets = targets.unwrap_or_else(|| {
            self.details
                .get(&name)
                .map(|details| details.bindings.clone())
                .unwrap_or_default()
        });
        self.binding_picker = Some(BindingPicker {
            name,
            choices,
            selected: 0,
            targets,
        });
    }
    pub fn operation(&mut self, operation: Operation) -> Input {
        let Some(row) = self.selected() else {
            return Input::Idle;
        };
        let name = row.instance.name.clone();
        let running = row.pid.is_some();
        let action = match operation {
            Operation::Start => {
                if running
                    && self
                        .details
                        .get(&name)
                        .is_none_or(|details| details.bindings.is_empty())
                {
                    self.message = "Already running".into();
                    None
                } else {
                    Some(Action::Start(name))
                }
            }
            Operation::Stop => {
                if running {
                    Some(Action::Stop(name))
                } else {
                    self.message = "Already stopped".into();
                    None
                }
            }
            Operation::ForceStart => {
                if !running && row.instance.kind != Kind::Server {
                    self.prompt(Prompt::ForceStart(name));
                }
                None
            }
            Operation::Restart => Some(if running {
                Action::Restart(name)
            } else {
                Action::Start(name)
            }),
            Operation::UpdateSubscriptions => Some(Action::UpdateSubscriptions(name)),
            Operation::Reload => {
                if running {
                    Some(Action::Reload(name))
                } else {
                    self.message = "Not running".into();
                    None
                }
            }
            Operation::Edit => {
                self.suspend_editor();
                // A cached editor's original is a baseline, not the current source file.
                // Re-read through the manager so external changes and conflict metadata survive.
                self.ready_editors.remove(&name);
                Some(Action::Read(name))
            }
            Operation::Logs => return self.open_logs(name),
            Operation::Diagnostics | Operation::NetTools => {
                if row.instance.kind != Kind::Proxy {
                    return Input::Idle;
                }
                if !self
                    .details
                    .get(&name)
                    .is_some_and(|d| d.diagnostics_supported)
                {
                    self.message = "Restart manager to enable diagnostics".into();
                    None
                } else {
                    self.open_nettools = matches!(operation, Operation::NetTools);
                    Some(Action::Diagnostics(name))
                }
            }
            Operation::Bind => {
                self.open_bindings(name, None);
                None
            }
            Operation::Explain => {
                if row.instance.kind != Kind::Rule || !running {
                    return Input::Idle;
                }
                if !self
                    .details
                    .get(&name)
                    .is_some_and(|d| d.explanation_supported)
                {
                    self.message = "Restart manager to enable route explanation".into();
                } else {
                    self.prompt(Prompt::Explain(name));
                }
                None
            }
            Operation::Config => {
                self.prompt(Prompt::Config(name));
                None
            }
            Operation::Copy => {
                self.prompt(Prompt::Clone(name));
                None
            }
            Operation::ExportConfig => {
                let text = std::fs::read_to_string(&row.instance.config).with_context(|| {
                    format!("Read configuration {}", row.instance.config.display())
                });
                match text.and_then(|text| crate::export::export_config(&text)) {
                    Ok(text) => self.prompt(Prompt::ExportConfig { text }),
                    Err(error) => self.fail(format!("{error:#}"), Resume::Editor),
                }
                None
            }
            Operation::ExportTerminal => {
                return self.copy_terminal_commands();
            }
            Operation::Remove => {
                if running {
                    self.message = "Stop instance before removing".into();
                } else {
                    self.prompt(Prompt::Remove(name));
                }
                None
            }
        };
        action
            .map(|action| Input::Run(action, None))
            .unwrap_or(Input::Idle)
    }
    pub fn active_rule(&self) -> Option<&Row> {
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
    pub fn proxies_count(&self) -> usize {
        self.rows
            .iter()
            .filter(|r| r.instance.kind == Kind::Proxy)
            .count()
    }
    pub fn close_viewer(&mut self) {
        if let Some(viewer) = self.viewer.take() {
            self.log_settings
                .insert(viewer.name.clone(), viewer.settings.clone());
        }
        self.active_log = None;
    }
    pub fn open_logs(&mut self, name: String) -> Input {
        self.remember_page(LOGS);
        if self.viewer.as_ref().is_some_and(|v| v.name == name) {
            return Input::Idle;
        }
        self.close_viewer();
        Input::Run(Action::Logs(name), None)
    }
    pub fn toggle_active(&mut self) -> Input {
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
    pub fn active_operation(&mut self, operation: Operation) -> Input {
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
    pub fn rule_operation(&mut self, operation: Operation) -> Input {
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
    pub fn rule_search(&mut self) -> Input {
        if self.active_rule().is_none() {
            self.message = t("No rule core for this profile · bind one with b in Profiles").into();
            return Input::Idle;
        }
        self.search_on_open = true;
        let input = self.rule_operation(Operation::Edit);
        self.apply_pending_search();
        input
    }
    pub fn find(&mut self, page: usize, query: &str) -> bool {
        self.find_direction(page, query, false)
    }
    pub fn find_direction(&mut self, page: usize, query: &str, backwards: bool) -> bool {
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
    pub fn log_source(&self, proxy: bool) -> Option<String> {
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
}

impl Client {
    pub fn activate_profile(&mut self, name: String) {
        self.suspend_editor();
        self.active_profile = Some(name);
        self.runtime_picker = None;
        if self.page == crate::navigation::LOGS {
            self.close_viewer();
        }
    }
}

impl Client {
    pub fn copy_terminal_commands(&mut self) -> Input {
        self.copy_terminal_commands_with(crate::clipboard::copy)
    }
    pub fn copy_terminal_commands_with(&mut self, copy: impl FnOnce(&str) -> Result<()>) -> Input {
        let result = self
            .terminal_export_profile()
            .context(t("No profile yet"))
            .and_then(|row| {
                std::fs::read_to_string(&row.instance.config).with_context(|| {
                    format!("Read configuration {}", row.instance.config.display())
                })
            })
            .and_then(|text| crate::export::current_shell_commands(&text))
            .and_then(|text| copy(&text));
        self.operation_failed = result.is_err();
        self.message = match result {
            Ok(()) => t("Terminal proxy commands copied to clipboard").into(),
            Err(error) => tr!(
                "Cannot copy terminal proxy commands: {}",
                format!("{error:#}")
            ),
        };
        self.tasks
            .notice_result(&self.message, self.operation_failed);
        Input::Idle
    }
}

#[derive(Clone, Copy)]
pub enum BindingAction {
    Previous,
    Next,
    Toggle,
    Confirm,
    Cancel,
}
impl Client {
    pub fn binding_action(&mut self, action: BindingAction) -> Input {
        let binding_is_proxy = self.kind == Kind::Proxy;
        if let Some(picker) = &mut self.binding_picker {
            match action {
                BindingAction::Cancel => self.binding_picker = None,
                BindingAction::Previous => picker.selected = picker.selected.saturating_sub(1),
                BindingAction::Next => {
                    picker.selected =
                        (picker.selected + 1).min(picker.choices.len().saturating_sub(1))
                }
                BindingAction::Toggle => {
                    if let Some((name, _)) = picker.choices.get(picker.selected) {
                        if picker.targets.contains(name) {
                            picker.targets.retain(|target| target != name);
                        } else {
                            if binding_is_proxy {
                                picker.targets.clear();
                            }
                            picker.targets.push(name.clone());
                        }
                    }
                }
                BindingAction::Confirm => {
                    let picker = self.binding_picker.take().unwrap();
                    return Input::Run(
                        Action::BindStart {
                            name: picker.name,
                            targets: picker.targets,
                        },
                        None,
                    );
                }
            }
            return Input::Idle;
        }
        Input::Idle
    }
    pub fn test_selected_group(&mut self) -> Input {
        let Some(name) = self
            .live()
            .and_then(|s| s.groups.get(self.group_selection))
            .map(|g| g.name.clone())
        else {
            return Input::Idle;
        };
        self.runtime_action(astra_model::runtime::Command::TestGroup(name))
    }
    pub fn close_all_connections(&mut self) -> Input {
        if !self.connection_rows().is_empty() {
            if let Some(row) = self.active_proxy() {
                self.prompt(Prompt::CloseAll {
                    name: row.instance.name.clone(),
                    pid: row.pid,
                });
            }
        }
        Input::Idle
    }
    pub fn server_action(&mut self, action: ServerAction) -> Input {
        match action {
            ServerAction::Quit => self.run_command(crate::commands::Id::Quit),
            ServerAction::Toggle => {
                self.operation(if self.selected().is_some_and(|r| r.pid.is_some()) {
                    Operation::Stop
                } else {
                    Operation::Start
                })
            }
            ServerAction::Reload => self.operation(Operation::Reload),
            ServerAction::Edit => self.operation(Operation::Edit),
            ServerAction::Logs => self.operation(Operation::Logs),
            ServerAction::Validate => self.selected().map_or(Input::Idle, |row| {
                Input::Run(Action::Validate(row.instance.name.clone()), None)
            }),
        }
    }
}
#[derive(Clone, Copy)]
pub enum ServerAction {
    Quit,
    Toggle,
    Reload,
    Edit,
    Logs,
    Validate,
}

impl Client {
    pub fn search_current(&mut self) -> Input {
        if let Some(editor) = &mut self.editor {
            editor.begin_search();
        } else if let Some(viewer) = &mut self.viewer {
            viewer.begin_search();
        } else if matches!(self.page, OVERVIEW | CONNECTIONS) {
            self.page = CONNECTIONS;
            self.connection_filtering = true;
        } else if self.page == RULES {
            return self.rule_search();
        } else {
            self.prompt(Prompt::Find(self.page));
        }
        Input::Idle
    }
    pub fn find_next(&mut self, backwards: bool) -> Input {
        if let Some(editor) = &mut self.editor {
            if editor.browsing() {
                let _ = editor.find_again(backwards);
            }
        } else if let Some(viewer) = &mut self.viewer {
            if viewer.browsing() {
                viewer.find_again(backwards);
            }
        } else if self.page == CONNECTIONS && !self.connection_filter.is_empty() {
            let count = self.connection_rows().len();
            if count > 0 {
                let current = self.connection_selection.min(count - 1);
                self.select_connection(if backwards {
                    (current + count - 1) % count
                } else {
                    (current + 1) % count
                });
            }
        } else {
            let query = self.searches.get(&self.page).cloned().unwrap_or_default();
            if !query.is_empty() {
                self.find_direction(self.page, &query, backwards);
            }
        }
        Input::Idle
    }
    pub fn monitor_target(&self) -> Option<String> {
        if self.page == PROFILES || self.kind == Kind::Server {
            self.selected()
        } else {
            self.active_proxy().or_else(|| self.selected())
        }
        .map(|row| row.instance.name.clone())
    }
    pub fn open_monitor(&mut self) {
        if let Some(name) = self.monitor_target() {
            let mut monitor = crate::monitor::View::new(name);
            monitor.waker = self.waker.clone();
            self.monitor = Some(monitor);
        } else {
            self.message = t("Select an instance to monitor").into();
        }
    }
}

#[cfg(test)]
#[path = "test/terminal_export_test.rs"]
mod terminal_export_tests;

#[derive(Clone, Copy)]
pub enum ProfileAction {
    Create,
    Toggle,
    Inspect,
    Validate,
    Operation(Operation),
}
#[derive(Clone, Copy)]
pub enum EditorAction {
    UpdateSubscriptions,
    Import,
    ImportItems { rules: bool },
    ExportItems { share: bool },
    Save,
    Search,
    Suspend,
}
impl Client {
    pub fn checkpoint(&mut self, path: &std::path::Path) -> bool {
        self.remember_draft();
        if let Some(viewer) = &self.viewer {
            let name = viewer.name.clone();
            let settings = viewer.settings.clone();
            self.log_settings.insert(name, settings);
        }

        if self.persistence_paused {
            return true;
        }
        if let Err(error) = self.session.save(path) {
            if self.recovery.is_none() {
                self.fail(
                    tr!("Cannot save session: {}", format!("{error:#}")),
                    Resume::Session,
                );
            }
            return false;
        }
        true
    }
    pub fn paste_content(&mut self, text: &str) {
        if self.prompt.is_some() {
            self.paste_prompt(text);
        } else if self.editor.as_ref().is_some_and(Editor::browsing) {
            if let Err(error) = self.submit(Prompt::Import, text.to_owned()) {
                self.fail(
                    format!("{error:#}"),
                    Resume::Prompt(Prompt::Import, text.to_owned()),
                );
            }
        } else if let Some(editor) = &mut self.editor {
            editor.paste(text);
        } else if let Some(viewer) = &mut self.viewer {
            viewer.paste(text);
        }
    }
    pub fn profile_action(&mut self, action: ProfileAction) -> Input {
        match action {
            ProfileAction::Create => {
                self.prompt(Prompt::AddName);
                Input::Idle
            }
            ProfileAction::Toggle => {
                self.operation(if self.selected().is_some_and(|r| r.pid.is_some()) {
                    Operation::Stop
                } else {
                    Operation::Start
                })
            }
            ProfileAction::Inspect => self.operation(if self.kind == Kind::Rule {
                Operation::Explain
            } else {
                Operation::Diagnostics
            }),
            ProfileAction::Validate => self.selected().map_or(Input::Idle, |row| {
                Input::Run(Action::Validate(row.instance.name.clone()), None)
            }),
            ProfileAction::Operation(operation) => self.operation(operation),
        }
    }
    pub fn editor_action(&mut self, action: EditorAction) -> Input {
        let kind = self.kind;
        let Some(editor) = &mut self.editor else {
            return Input::Idle;
        };
        match action {
            EditorAction::UpdateSubscriptions => {
                if editor.text() != editor.original {
                    self.message = "请先按 Ctrl-S 保存订阅配置，再手动更新".into();
                } else {
                    return Input::Run(Action::UpdateSubscriptions(editor.name.clone()), None);
                }
            }
            EditorAction::Import => {
                if editor.browsing() {
                    self.prompt(Prompt::Import);
                } else {
                    self.message = "Confirm or cancel the field before importing".into();
                }
            }
            EditorAction::ImportItems { rules } if editor.browsing() => {
                self.prompt(if rules {
                    Prompt::ImportRule(crate::import::MergeMode::Append)
                } else {
                    Prompt::ImportNode(crate::import::MergeMode::Append)
                });
            }
            EditorAction::ExportItems { share } if editor.browsing() => {
                if kind == Kind::Proxy {
                    return self.start_node_export(share);
                }
                match editor.export_selected_rule() {
                    Ok(text) => self.prompt(Prompt::ExportRule { text }),
                    Err(error) => self.fail(format!("{error:#}"), Resume::Editor),
                }
            }
            EditorAction::Save => return self.run_command(crate::commands::Id::Save),
            EditorAction::Search => editor.begin_search(),
            EditorAction::Suspend => self.suspend_editor(),
            _ => {}
        }
        Input::Idle
    }
    pub fn log_command(&mut self, command: crate::logs::Command) -> Input {
        let Some(viewer) = &self.viewer else {
            return Input::Idle;
        };
        let name = viewer.name.clone();
        match command {
            crate::logs::Command::Close => self.close_viewer(),
            crate::logs::Command::Reconnect => return Input::Run(Action::Logs(name), None),
            crate::logs::Command::Capture(level) => {
                return Input::Run(Action::SetLogLevel { name, level }, None)
            }
            crate::logs::Command::None => {}
        }
        Input::Idle
    }
}

#[cfg(test)]
mod command_tests {
    use super::*;
    #[test]
    fn document_undo_does_not_replace_an_open_search_input() {
        let mut client = Client::new(Kind::Proxy);
        let mut editor = Editor::new("proxy".into(), "standalone=true\n".into());
        editor.import("standalone=false\n".into()).unwrap();
        editor.begin_search();
        client.editor = Some(editor);
        client.run_command(crate::commands::Id::Undo);
        assert_eq!(client.editor.as_ref().unwrap().text(), "standalone=false\n");
        assert!(!client.editor.as_ref().unwrap().browsing());
        client.editor.as_mut().unwrap().cancel_input();
        client.run_command(crate::commands::Id::Undo);
        assert_eq!(client.editor.as_ref().unwrap().text(), "standalone=true\n");
    }
}
