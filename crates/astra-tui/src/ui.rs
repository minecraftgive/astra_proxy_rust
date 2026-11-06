mod animation;
mod commands;
mod connections;
mod drafts;
mod event_loop;
mod focus;
mod server;
pub(crate) fn wake() {
    animation::wake();
}
mod background;
mod dashboard;
mod detached;
mod live;
mod monitor;
mod node_transfer;
mod operations;
mod pages;
mod settings;
pub(crate) fn preferred_terminal(home: &std::path::Path) -> String {
    settings::load(&home.join(settings::FILE), &Session::default())
        .0
        .terminal
}
mod shell;
mod startup;
pub(crate) use startup::{log_startup, run_silent};
#[cfg(test)]
#[path = "test/ui_stack_test.rs"]
mod stack_tests;
mod terminal_export;
#[cfg(test)]
#[path = "test/ui_test_rpc_test.rs"]
mod test_rpc;
mod update;
use crate::editor::Editor;
use crate::logs;
use crate::protocol::*;
use crate::{i18n::t, tr};
use anyhow::{Context, Result};
use astra_tui_kit::{TerminalOptions, TerminalSession};
use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
#[cfg(test)]
use ratatui::Terminal;
use ratatui::{
    layout::{Constraint, Direction, Layout},
    style::{Color, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};
use serde::{Deserialize, Serialize};
use shell::{LOGS, PROFILES};
use std::{
    collections::HashMap,
    io,
    path::PathBuf,
    time::{Duration, Instant},
};

#[derive(Clone, Serialize, Deserialize)]
enum Prompt {
    AddName,
    AddPath(String),
    Config(String),
    Clone(String),
    Remove(String),
    ForceStart(String),
    Shutdown,
    Discard,
    Import,
    UpdatePackage,
    UpdateConfirm(crate::protocol::updates::UpdateRequest),
    ImportNode(crate::import::MergeMode),
    ImportRule(crate::import::MergeMode),
    ExportConfig {
        text: String,
    },
    ExportNode {
        text: String,
        share: bool,
    },
    ExportNodeDestination {
        text: String,
        share: bool,
        clipboard: bool,
    },
    ImportNodeSelection {
        name: String,
        path: PathBuf,
        candidates: crate::import::NodeImport,
        selected: Vec<usize>,
        cursor: usize,
        mode: crate::import::MergeMode,
    },
    ExportRule {
        text: String,
    },
    ExportTerminal {
        text: String,
    },
    ImportConfirm {
        text: String,
        notices: String,
    },
    Explain(String),
    CloseAll {
        name: String,
        pid: Option<u32>,
    },
    /// Linux only: a sudo-launched core takes over the TUN listener.
    ElevateTun {
        name: String,
        pid: Option<u32>,
    },
    /// Find on a list page; Enter jumps to the next match.
    Find(usize),
}
impl Prompt {
    fn picker(&self) -> bool {
        matches!(
            self,
            Self::ExportNodeDestination { .. } | Self::ImportNodeSelection { .. }
        )
    }
    fn confirmation(&self) -> bool {
        matches!(
            self,
            Self::Remove(_)
                | Self::ForceStart(_)
                | Self::Shutdown
                | Self::Discard
                | Self::ImportConfirm { .. }
                | Self::CloseAll { .. }
                | Self::ElevateTun { .. }
                | Self::UpdateConfirm(_)
        )
    }
    fn title(&self) -> &str {
        match self {
            Self::AddName => t("Name (1–24: a-z, 0-9, -_)"),
            Self::Find(_) => t("Find (Enter jumps to the next match)"),
            Self::AddPath(_) => t("Configuration path"),
            Self::Config(_) => t("Configuration path"),
            Self::Clone(_) => t("Copy to path"),
            Self::Remove(_) => t("Remove instance? Configuration is kept."),
            Self::ForceStart(_) => t("Force start and replace previous sessions?"),
            Self::Shutdown => t("Stop all instances?"),
            Self::Discard => t("Discard changes?"),
            Self::Import => t("Import config or proxy share links"),
            Self::UpdatePackage => t("Local update package path (.zip, .tar.gz or directory)"),
            Self::UpdateConfirm(_) => t("Apply this exact update target?"),
            Self::ImportNode(_) => t("Import node files, TOML or share links"),
            Self::ImportNodeSelection { .. } => t("Select nodes to import"),
            Self::ImportRule(_) => t("Import rules TOML"),
            Self::ExportConfig { .. } => t("Export configuration to path"),
            Self::ExportNode { .. } => t("Export nodes to file or folder (missing folders are created)"),
            Self::ExportNodeDestination { .. } => t("Export selected nodes"),
            Self::ExportRule { .. } => t("Export selected rule to path"),
            Self::ExportTerminal { .. } => t("Export terminal proxy commands to path"),
            Self::ImportConfirm { .. } => t("Replace editor draft?"),
            Self::Explain(_) => t("Enter host[:port] [tcp|udp] [process name or path]"),
            Self::CloseAll { .. } => {
                t("Close all connections? Applications reconnect on their own.")
            }
            Self::ElevateTun { .. } => {
                t("Enable TUN? sudo asks for your password; the current core keeps running until the replacement is ready.")
            }
        }
    }
}

#[derive(Clone, Copy)]
enum Operation {
    UpdateSubscriptions,
    Start,
    ForceStart,
    Stop,
    Restart,
    Reload,
    Edit,
    Logs,
    Bind,
    Config,
    Copy,
    Remove,
    Diagnostics,
    NetTools,
    Explain,
    ExportConfig,
    ExportTerminal,
}
impl Operation {
    fn label(self) -> &'static str {
        t(match self {
            Self::Start => "Start",
            Self::ForceStart => "Force start",
            Self::Stop => "Stop",
            Self::Restart => "Restart",
            Self::Reload => "Reload",
            Self::UpdateSubscriptions => "Update subscriptions",
            Self::Edit => "Edit",
            Self::Logs => "Logs",
            Self::Bind => "Startup bindings",
            Self::Config => "Configuration path",
            Self::Copy => "Copy configuration",
            Self::Remove => "Remove",
            Self::Diagnostics => "Connectivity / Exit IP",
            Self::NetTools => "Network tools",
            Self::Explain => "Route explanation",
            Self::ExportConfig => "Export config",
            Self::ExportTerminal => "Copy terminal proxy commands to clipboard · F4",
        })
    }
}
struct BindingPicker {
    name: String,
    choices: Vec<(String, Kind)>,
    selected: usize,
    targets: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize)]
enum Resume {
    Prompt(Prompt, String),
    Request(Action),
    Session,
    Editor,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Recovery {
    message: String,
    resume: Resume,
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Session {
    #[serde(default)]
    session_version: u32,
    #[serde(default)]
    keymap_version: u32,
    #[serde(default)]
    keymap_notice_pending: bool,
    #[serde(default)]
    drafts: HashMap<String, drafts::Draft>,
    #[serde(default)]
    active_profile: Option<String>,
    #[serde(default)]
    searches: HashMap<usize, String>,
    #[serde(default)]
    input_cursor: usize,
    #[serde(default)]
    motion: animation::MotionMode,
    #[serde(default)]
    vim_keys: bool,
    kind: Kind,
    #[serde(deserialize_with = "read_selections")]
    selections: [usize; 3],
    editor: Option<Editor>,
    prompt: Option<Prompt>,
    input: String,
    recovery: Option<Recovery>,
    #[serde(default = "launch_directory")]
    working_dir: PathBuf,
    #[serde(default)]
    log_settings: HashMap<String, logs::Settings>,
    #[serde(default)]
    active_log: Option<String>,
    #[serde(default)]
    external_draft: Option<crate::external_editor::Draft>,
}
fn launch_directory() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("/"))
}
impl Default for Session {
    fn default() -> Self {
        Self {
            session_version: 1,
            keymap_version: drafts::KEYMAP_VERSION,
            keymap_notice_pending: false,
            drafts: HashMap::new(),
            active_profile: None,
            searches: HashMap::new(),
            input_cursor: 0,
            motion: Default::default(),
            vim_keys: false,
            kind: Kind::Proxy,
            selections: [0; 3],
            editor: None,
            prompt: None,
            input: String::new(),
            recovery: None,
            working_dir: launch_directory(),
            log_settings: HashMap::new(),
            active_log: None,
            external_draft: None,
        }
    }
}
const SESSION_LIMIT: u64 = 4 * 1024 * 1024;
fn read_selections<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> std::result::Result<[usize; 3], D::Error> {
    let old = Vec::<usize>::deserialize(deserializer)?;
    if !(2..=3).contains(&old.len()) {
        return Err(serde::de::Error::custom(
            "Invalid saved instance selections; original session is retained",
        ));
    }
    let mut result = [0; 3];
    result[..old.len()].copy_from_slice(&old);
    Ok(result)
}
fn kind_index(kind: Kind) -> usize {
    match kind {
        Kind::Proxy => 0,
        Kind::Rule => 1,
        Kind::Server => 2,
    }
}
fn next_kind(kind: Kind) -> Kind {
    match kind {
        Kind::Proxy => Kind::Rule,
        Kind::Rule => Kind::Server,
        Kind::Server => Kind::Proxy,
    }
}
impl Session {
    fn load(path: &std::path::Path) -> Result<Self> {
        use std::io::Read;
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(error) => return Err(error.into()),
        };
        let mut bytes = Vec::new();
        file.take(SESSION_LIMIT + 1).read_to_end(&mut bytes)?;
        anyhow::ensure!(
            bytes.len() as u64 <= SESSION_LIMIT,
            "Saved session exceeds 4 MiB"
        );
        let mut session: Self = serde_json::from_slice(&bytes)?;
        anyhow::ensure!(
            session.session_version <= 1,
            "Saved session version is newer than this TUI; original file is retained: {}",
            path.display()
        );
        anyhow::ensure!(
            session.keymap_version <= drafts::KEYMAP_VERSION,
            "Saved keymap version is newer than this TUI; original file is retained: {}",
            path.display()
        );
        let value: serde_json::Value = serde_json::from_slice(&bytes)?;
        let needs_migration = session.session_version == 0
            || session.keymap_version < drafts::KEYMAP_VERSION
            || value
                .get("selections")
                .and_then(|v| v.as_array())
                .is_some_and(|v| v.len() == 2);
        if needs_migration {
            use astra_platform::platform::OpenOptionsExt;
            use std::io::Write;
            let backup = path.with_extension("pre-v2.json");
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&backup)
            {
                Ok(mut output) => {
                    astra_platform::platform::set_private(&backup, 0o600)?;
                    output.write_all(&bytes)?;
                    output.sync_all()?;
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    anyhow::ensure!(
                        std::fs::symlink_metadata(&backup)?.is_file(),
                        "Session migration backup is not a regular file: {}",
                        backup.display()
                    );
                }
                Err(error) => return Err(error.into()),
            }
        }
        session.session_version = session.session_version.max(1);
        if let Some(editor) = &mut session.editor {
            editor.normalize();
        }
        for draft in session.drafts.values_mut() {
            draft.editor.normalize();
        }
        if session.keymap_version < drafts::KEYMAP_VERSION {
            if let Some(editor) = &session.editor {
                session.drafts.insert(
                    format!("{}\n", editor.name),
                    drafts::Draft {
                        path: PathBuf::new(),
                        kind: session.kind,
                        editor: editor.clone(),
                        conflict: false,
                        observed_disk: None,
                    },
                );
            }
            session.input_cursor = session.input.chars().count();
            session.keymap_version = drafts::KEYMAP_VERSION;
            session.keymap_notice_pending = true;
        }
        Ok(session)
    }
    fn save(&self, path: &std::path::Path) -> Result<()> {
        let data = serde_json::to_vec(self)?;
        anyhow::ensure!(data.len() as u64 <= SESSION_LIMIT, "Session exceeds 4 MiB");
        Ok(astra_platform::storage::atomic_write_default(path, &data)?)
    }
}

struct Ui {
    session: Session,
    focus: focus::Focus,
    animations: animation::Motion,
    live_state: live::LiveState,
    confirmation_accept: bool,
    update_status: Option<crate::protocol::updates::UpdateStatus>,
    update_view: Option<update::View>,
    page: usize,
    navigation_stack: Vec<usize>,
    palette: Option<shell::Palette>,
    /// Reject late view-opening results after the user has switched overlays.
    overlay_epoch: u64,
    runtime_snapshot: Option<astra_model::runtime::Snapshot>,
    runtime_instance: String,
    runtime_error: String,
    runtime_sample: Instant,
    rates: [u64; 2],
    direct_rates: [u64; 2],
    history: [std::collections::VecDeque<u64>; 2],
    group_selection: usize,
    profile_viewport: std::cell::Cell<usize>,
    connection_selection: usize,
    overview_cursor: usize,
    /// Items reachable by Overview scrolling; narrow layouts include proxy groups.
    overview_item_count: std::cell::Cell<usize>,
    traffic_expanded: bool,
    /// First time each transient notice was shown; errors remain until resolved.
    notice_since: std::cell::RefCell<HashMap<String, Instant>>,
    /// Next expiry from the last rendered notices, consumed once to wake an idle UI.
    notice_expiry: std::cell::Cell<Option<Instant>>,
    connection_filter: String,
    connection_filtering: bool,
    runtime_picker: Option<(String, usize)>,
    hits: std::cell::RefCell<astra_tui_kit::HitMap<dashboard::Target>>,
    rows: Vec<Row>,
    message: String,
    viewer: Option<logs::Viewer>,
    scroll: u16,
    refresh_paused: bool,
    persistence_paused: bool,
    details: HashMap<String, InstanceDetails>,
    details_supported: bool,
    menu: Option<usize>,
    binding_picker: Option<BindingPicker>,
    prefilled_input: bool,
    diagnostics: Option<crate::diagnostics::View>,
    /// The pending Diagnostics action was requested as network tools.
    open_nettools: bool,
    explanation: Option<crate::explanation::View>,
    vim_g: bool,
    help_scroll: Option<u16>,
    help_override: Option<String>,
    operation_steps: Vec<String>,
    operation_result: Option<String>,
    tasks: background::Tasks,
    tui_viewer: Option<logs::Viewer>,
    monitor: Option<monitor::View>,
    operation_failed: bool,
    ready_editors: HashMap<String, Editor>,
    last_find: String,
    /// Open the editor's search as soon as a requested editor arrives.
    search_on_open: bool,
    preferences: settings::Preferences,
    preferences_path: Option<PathBuf>,
    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    startup_context: Option<crate::windows::Context>,
    #[cfg(windows)]
    service_change: Option<tokio::task::JoinHandle<std::result::Result<bool, String>>>,
    settings_cursor: usize,
    settings_view: settings::View,
    terminals: settings::TerminalState,
    /// Ctrl+Home held on the accent setting: (first press, latest press, done).
    marquee_hold: Option<(Instant, Instant, bool)>,
    program_info: background::ProgramInformation,
    /// Mouse capture change for the event loop, which owns the terminal.
    mouse_request: Option<bool>,
}
impl std::ops::Deref for Ui {
    type Target = Session;
    fn deref(&self) -> &Session {
        &self.session
    }
}
impl std::ops::DerefMut for Ui {
    fn deref_mut(&mut self) -> &mut Session {
        &mut self.session
    }
}

enum Input {
    Idle,
    Quit,
    Run(Action, Option<Box<(Prompt, String)>>),
    ExternalEditor,
    OpenWindow(crate::windows::Launch),
}

impl Ui {
    fn prepare_external(
        &mut self,
        home: &std::path::Path,
    ) -> Result<(crate::external_editor::Draft, PathBuf)> {
        let editor = self
            .editor
            .as_mut()
            .context(t("Open a configuration first"))?;
        let text = editor.prepare_save()?;
        let name = editor.name.clone();
        if let Some(previous) = &self.external_draft {
            if previous.name == name && previous.base != text && previous.path.is_file() {
                anyhow::ensure!(
                    previous.read()? == previous.base,
                    "{}: {}",
                    t("TUI draft changed; external file is kept for manual import"),
                    previous.path.display()
                );
            }
        }
        let draft = match &self.external_draft {
            Some(draft) if draft.name == name && draft.base == text && draft.path.is_file() => {
                draft.clone()
            }
            _ => crate::external_editor::Draft::create(home, name.clone(), text)?,
        };
        self.external_draft = Some(draft.clone());
        self.message = tr!(
            "External draft: {} · F2 → Read external draft · Ctrl-S save",
            draft.path.display()
        );
        let directory = self
            .rows
            .iter()
            .find(|row| row.instance.name == name)
            .and_then(|row| row.instance.config.parent())
            .map(std::path::Path::to_path_buf)
            .unwrap_or_else(|| self.working_dir.clone());
        Ok((draft, directory))
    }
    fn read_external(&mut self) -> Result<()> {
        let draft = self
            .external_draft
            .clone()
            .context(t("No external draft; use F2 → Open external editor"))?;
        let text = draft.read()?;
        let editor = self
            .editor
            .as_mut()
            .context(t("Open a configuration first"))?;
        anyhow::ensure!(
            editor.name == draft.name,
            t("External draft belongs to another instance")
        );
        anyhow::ensure!(
            editor.text() == draft.base || editor.text() == text,
            t("TUI draft changed; external file is kept for manual import")
        );
        let path = draft.path.clone();
        if editor.text() != text {
            editor.import(text.clone())?;
        }
        self.external_draft.as_mut().unwrap().base = text;
        self.message = tr!(
            "External draft: {} · F2 → Read external draft · Ctrl-S save",
            path.display()
        );
        Ok(())
    }
    fn new(session: Session) -> Self {
        let animations = animation::Motion::new(session.motion);
        let migration = session.keymap_notice_pending;
        Self {
            session,
            focus: Default::default(),
            animations,
            live_state: Default::default(),
            confirmation_accept: false,
            update_status: None,
            update_view: None,
            page: 0,
            navigation_stack: Vec::new(),
            palette: None,
            overlay_epoch: 0,
            runtime_snapshot: None,
            runtime_instance: String::new(),
            runtime_error: String::new(),
            runtime_sample: Instant::now(),
            rates: [0, 0],
            direct_rates: [0, 0],
            history: Default::default(),
            group_selection: 0,
            profile_viewport: std::cell::Cell::new(1),
            connection_selection: 0,
            overview_cursor: 0,
            overview_item_count: Default::default(),
            traffic_expanded: false,
            notice_since: Default::default(),
            notice_expiry: Default::default(),
            connection_filter: String::new(),
            connection_filtering: false,
            runtime_picker: None,
            hits: Default::default(),
            rows: vec![],
            message: String::new(),
            viewer: None,
            scroll: 0,
            refresh_paused: false,
            persistence_paused: false,
            details: HashMap::new(),
            details_supported: false,
            menu: None,
            binding_picker: None,
            prefilled_input: false,
            diagnostics: None,
            open_nettools: false,
            explanation: None,
            vim_g: false,
            help_scroll: migration.then_some(0),
            help_override: migration.then(|| t("Keyboard layout updated\n\nTab / Shift-Tab now moves focus. Enter activates the focused item.\nSpace pauses connections and logs, or toggles a checkbox.\nF2 / Ctrl-K opens every command. Q, R and 0 no longer run destructive actions.\nEsc, page changes and quitting retain configuration drafts.\n\nF1 opens the complete key map. Enter / Esc dismisses this notice.").into()),
            operation_steps: Vec::new(),
            operation_result: None,
            tasks: Default::default(),
            tui_viewer: None,
            monitor: None,
            operation_failed: false,
            ready_editors: HashMap::new(),
            last_find: String::new(),
            search_on_open: false,
            preferences: Default::default(),
            preferences_path: None,
            #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
            startup_context: None,
            #[cfg(windows)]
            service_change: None,
            settings_cursor: 0,
            settings_view: Default::default(),
            terminals: Default::default(),
            marquee_hold: None,
            program_info: Default::default(),
            mouse_request: None,
        }
    }
    fn index(&self) -> usize {
        kind_index(self.kind)
    }
    fn rows(&self) -> Vec<&Row> {
        self.rows
            .iter()
            .filter(|r| r.instance.kind == self.kind)
            .collect()
    }
    fn selected(&self) -> Option<&Row> {
        self.rows().get(self.selections[self.index()]).copied()
    }
    fn operations(&self) -> Vec<Operation> {
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
    /// Switches the Profiles list between proxy instances and rule cores.
    fn switch_core(&mut self, kind: Kind) {
        self.kind = kind;
    }
    fn open_bindings(&mut self, name: String, targets: Option<Vec<String>>) {
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
    fn operation(&mut self, operation: Operation) -> Input {
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
    fn update_rows(&mut self, reply: Reply) {
        let details = serde_json::from_str(&reply.text);
        self.details_supported = details.is_ok();
        self.details = details.unwrap_or_default();
        self.rows = reply.rows;
        if self.active_profile.is_none() {
            self.active_profile = self
                .rows
                .iter()
                .filter(|r| r.instance.kind == Kind::Proxy)
                .nth(self.selections[0])
                .or_else(|| self.rows.iter().find(|r| r.instance.kind == Kind::Proxy))
                .map(|r| r.instance.name.clone());
        }
        for (index, kind) in [Kind::Proxy, Kind::Rule, Kind::Server]
            .into_iter()
            .enumerate()
        {
            let count = self
                .rows
                .iter()
                .filter(|row| row.instance.kind == kind)
                .count();
            self.selections[index] = self.selections[index].min(count.saturating_sub(1));
        }
    }
    fn prompt(&mut self, prompt: Prompt) {
        self.confirmation_accept = false;
        if matches!(prompt, Prompt::ExportNode { .. }) {
            match astra_platform::platform::documents_dir() {
                Ok(directory) => self.working_dir = directory,
                Err(error) => {
                    self.fail(
                        format!("Locate Documents directory: {error}"),
                        Resume::Editor,
                    );
                    return;
                }
            }
        } else if matches!(
            prompt,
            Prompt::AddName
                | Prompt::Config(_)
                | Prompt::Clone(_)
                | Prompt::ImportNode(_)
                | Prompt::ImportRule(_)
                | Prompt::UpdatePackage
                | Prompt::ExportConfig { .. }
                | Prompt::ExportRule { .. }
                | Prompt::ExportTerminal { .. }
        ) {
            self.working_dir = launch_directory();
        }
        self.input = match &prompt {
            Prompt::Config(name) => self
                .rows
                .iter()
                .find(|row| &row.instance.name == name)
                .map(|row| row.instance.config.to_string_lossy().into_owned())
                .unwrap_or_default(),
            Prompt::Clone(name) => self
                .rows
                .iter()
                .find(|row| &row.instance.name == name)
                .map(|row| {
                    let source = &row.instance.config;
                    let stem = source.file_stem().unwrap_or_default().to_string_lossy();
                    let mut target = source.with_file_name(format!("{stem}-copy.toml"));
                    for index in 2..=100 {
                        if !target.exists() {
                            break;
                        }
                        target = source.with_file_name(format!("{stem}-copy-{index}.toml"));
                    }
                    target.to_string_lossy().into_owned()
                })
                .unwrap_or_default(),
            Prompt::ExportNode { share, .. } => self
                .working_dir
                .join(if *share { "nodes.txt" } else { "nodes.toml" })
                .to_string_lossy()
                .into_owned(),
            Prompt::ExportConfig { .. }
            | Prompt::ExportRule { .. }
            | Prompt::ExportTerminal { .. } => String::new(),
            Prompt::Find(page) => self.searches.get(page).cloned().unwrap_or_default(),
            _ => String::new(),
        };
        self.prefilled_input = !self.input.is_empty();
        self.input_cursor = self.input.chars().count();
        self.prompt = Some(prompt);
    }
    fn paste_prompt(&mut self, text: &str) {
        if self.prompt.as_ref().is_some_and(Prompt::picker) {
            return;
        }
        if self.prefilled_input {
            self.input.clear();
            self.input_cursor = 0;
            self.prefilled_input = false;
        }
        let multiline = matches!(self.prompt, Some(Prompt::ImportNode(_)))
            || matches!(self.prompt, Some(Prompt::Import)) && astra_convert::share::is_share(text);
        let inserted: String = text
            .chars()
            .filter(|character| {
                !character.is_control() || (multiline && matches!(*character, '\n' | '\r' | '\t'))
            })
            .collect();
        let byte = self.prompt_byte();
        self.input.insert_str(byte, &inserted);
        self.input_cursor += inserted.chars().count();
    }
    fn prompt_byte(&self) -> usize {
        self.input
            .char_indices()
            .nth(self.input_cursor)
            .map_or(self.input.len(), |(index, _)| index)
    }
    fn paste(&mut self, text: &str) {
        if self.monitor.is_some() {
            return;
        }
        if self.background_paste(text) {
            return;
        }
        if self.help_scroll.is_some() {
            return;
        }
        if let Some(palette) = &mut self.palette {
            palette.insert(text);
            return;
        }
        if self.recovery.is_some()
            || self.menu.is_some()
            || self.binding_picker.is_some()
            || self.diagnostics.is_some()
            || self.explanation.is_some()
            || self.prompt.as_ref().is_some_and(Prompt::confirmation)
        {
            return;
        }
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
    fn fail(&mut self, error: impl std::fmt::Display, resume: Resume) {
        self.operation_failed = true;
        self.recovery = Some(Recovery {
            message: error.to_string(),
            resume,
        });
        self.scroll = 0;
    }
    fn checkpoint(&mut self, path: &std::path::Path) -> bool {
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
    fn recover_key(&mut self, key: KeyCode) -> Input {
        match key {
            KeyCode::Esc | KeyCode::Char('b') => {
                let recovery = self.recovery.take().unwrap();
                if matches!(recovery.resume, Resume::Session) {
                    self.persistence_paused = true;
                }
                if matches!(recovery.resume, Resume::Request(Action::List)) {
                    self.refresh_paused = true;
                }
                if matches!(recovery.resume, Resume::Editor) {
                    if let Some(editor) = &mut self.editor {
                        editor.cancel_input();
                    }
                }
                if matches!(recovery.resume, Resume::Request(Action::Logs(_))) {
                    if let Some(viewer) = &mut self.viewer {
                        viewer.stop();
                    }
                }
                if matches!(recovery.resume, Resume::Request(Action::SetLogLevel { .. })) {
                    if let Some(viewer) = &mut self.viewer {
                        viewer.cancel_input();
                    }
                }
                self.prompt = None;
                self.message = if self.persistence_paused {
                    "Operation ended; draft retained in memory; session saving disabled until next launch"
                } else { "Operation ended; editor draft retained" }.into();
            }
            KeyCode::Char('c') => {
                let recovery = self.recovery.take().unwrap();
                self.scroll = 0;
                match recovery.resume {
                    Resume::Prompt(prompt, input) => {
                        self.prompt = Some(prompt);
                        self.input = input;
                        self.input_cursor = self.input.chars().count();
                        self.confirmation_accept = false;
                    }
                    Resume::Request(Action::Save { .. }) if self.editor.is_some() => {
                        self.message =
                            "Draft retained; correct the file or permissions, then Ctrl-S to retry"
                                .into();
                    }
                    Resume::Request(Action::BindStart { name, targets }) => {
                        self.open_bindings(name, Some(targets));
                    }
                    Resume::Request(action) => return Input::Run(action, None),
                    Resume::Editor => {
                        self.message = "Edit the value · Enter confirm".into();
                    }
                    Resume::Session => {
                        self.message = "Retrying…".into();
                    }
                }
            }
            KeyCode::Up => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::Down => self.scroll = self.scroll.saturating_add(1),
            _ => {}
        }
        Input::Idle
    }
    fn submit(&mut self, prompt: Prompt, input: String) -> Result<Option<Action>> {
        Ok(match prompt {
            Prompt::UpdatePackage => Some(update::package_action(&input, &self.working_dir)?),
            Prompt::UpdateConfirm(request) => Some(Action::UpdateTransaction { request }),
            Prompt::AddName => {
                anyhow::ensure!(
                    !input.is_empty()
                        && input.len() <= 24
                        && input
                            .bytes()
                            .all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'),
                    t("Name: 1–24 letters, digits, - or _")
                );
                Some(Action::Create {
                    name: input,
                    kind: self.kind,
                })
            }
            Prompt::AddPath(name) => Some(Action::Add(Instance {
                name,
                kind: self.kind,
                config: absolute(&input, &self.working_dir)?,
            })),
            Prompt::Config(name) => Some(Action::SetConfig(
                name,
                absolute(&input, &self.working_dir)?,
            )),
            Prompt::Clone(name) => Some(Action::CloneConfig(
                name,
                absolute(&input, &self.working_dir)?,
            )),
            Prompt::ExportConfig { text }
            | Prompt::ExportRule { text }
            | Prompt::ExportTerminal { text, .. } => {
                let path = absolute(&input, &self.working_dir)?;
                crate::export::write_new(&path, &text)?;
                self.message = tr!("Exported to {}", path.display());
                None
            }
            Prompt::ExportNode { text, share } => {
                let path = node_transfer::export_path(&input, &self.working_dir, share)?;
                crate::export::write_new(&path, &text)?;
                self.message = tr!("Exported to {}", path.display());
                None
            }
            Prompt::ExportNodeDestination { .. } | Prompt::ImportNodeSelection { .. } => None,
            Prompt::Remove(name) if input == "yes" => Some(Action::Remove(name)),
            Prompt::ForceStart(name) if input == "yes" => Some(Action::ForceStart(name)),

            Prompt::Shutdown if input == "yes" => Some(Action::Shutdown),
            Prompt::Discard if input == "yes" => {
                self.discard_current_draft();
                None
            }
            Prompt::Find(page) => {
                if !input.is_empty() {
                    self.last_find = input.clone();
                    self.searches.insert(page, input.clone());
                    if !self.find(page, &input) {
                        self.message = tr!("No match for \"{}\"", input);
                    }
                }
                None
            }
            Prompt::CloseAll { name, pid } if input == "yes" => Some(Action::Runtime {
                name,
                command: astra_model::runtime::Command::Disconnect(None),
                expected_pid: pid,
            }),
            Prompt::ElevateTun { name, pid } if input == "yes" => Some(Action::PrepareElevation {
                name,
                expected_pid: pid,
            }),
            Prompt::Explain(name) => Some(Action::ExplainRoute {
                name,
                request: crate::explanation::request(&input)?,
            }),
            Prompt::ImportNode(mode) => {
                self.prepare_node_import(&input, mode)?;
                None
            }
            Prompt::ImportRule(mode) => {
                let editor = self.editor.as_ref().context("Missing editor")?;
                let imported = crate::import::merge_at(
                    &input,
                    &self.working_dir,
                    &editor.text(),
                    Kind::Rule,
                    mode,
                )?;
                self.editor.as_mut().unwrap().import(imported.text)?;
                self.message = "Imported rules to draft · Ctrl-S save".into();
                None
            }
            Prompt::Import => {
                let share = astra_convert::share::is_share(&input);
                if share {
                    self.prepare_node_import(&input, crate::import::MergeMode::Append)?;
                    return Ok(None);
                }
                let editor = self.editor.as_ref().context("Missing editor")?;
                let path = crate::import::path(&input, &self.working_dir)?;
                let imported = crate::import::load(&path, self.kind, &editor.text())?;
                let label = path.display().to_string();
                if editor.text() != editor.original || !imported.warnings.is_empty() {
                    let mut notices = vec![label];
                    if editor.text() != editor.original {
                        notices.push(t("Unsaved draft will be replaced").into());
                    }
                    notices.extend(imported.warnings.iter().map(|s| t(s).to_owned()));
                    self.prompt(Prompt::ImportConfirm {
                        text: imported.text,
                        notices: notices.join("\n\n"),
                    });
                    self.scroll = 0;
                } else {
                    self.editor.as_mut().unwrap().import(imported.text)?;
                    self.message = "Imported to draft · Ctrl-S save".into();
                }
                None
            }
            Prompt::ImportConfirm { text, .. } if input == "yes" => {
                self.editor
                    .as_mut()
                    .context("Missing editor")?
                    .import(text)?;
                self.message = "Imported to draft · Ctrl-S save".into();
                None
            }
            _ => None,
        })
    }
    fn key(&mut self, mut key: event::KeyEvent) -> Input {
        if key.kind == KeyEventKind::Release {
            return Input::Idle;
        }
        if key.kind == KeyEventKind::Repeat {
            let navigation = matches!(
                key.code,
                KeyCode::Up
                    | KeyCode::Down
                    | KeyCode::Left
                    | KeyCode::Right
                    | KeyCode::Home
                    | KeyCode::End
                    | KeyCode::PageUp
                    | KeyCode::PageDown
            );
            let text = self.repeat_text_input_active()
                && matches!(
                    key.code,
                    KeyCode::Char(_) | KeyCode::Backspace | KeyCode::Delete
                )
                && !key.modifiers.contains(KeyModifiers::CONTROL);
            if !navigation && !text && !commands::repeatable(key) {
                return Input::Idle;
            }
        }
        if let Some(input) = self.marquee_hold_key(key) {
            return input;
        }
        if self.keymap_notice_pending
            && matches!(key.code, KeyCode::Enter | KeyCode::Esc | KeyCode::F(1))
        {
            self.keymap_notice_pending = false;
            self.help_override = None;
            self.help_scroll = if key.code == KeyCode::F(1) {
                Some(0)
            } else {
                None
            };
            return Input::Idle;
        }
        // Window shortcuts are global, including when task/log overlays own input.
        if let Some(input) = self.window_key(key) {
            return input;
        }
        if let Some(input) = self.command_key(key) {
            return input;
        }
        if let Some(view) = &mut self.monitor {
            if key.code == KeyCode::Esc {
                self.monitor = None;
            } else {
                view.key(key);
            }
            return Input::Idle;
        }
        if self.tasks.showing_details() || self.tui_viewer.is_some() {
            if let Some(input) = self.background_key(key) {
                return input;
            }
        }
        // Editable palette text must never pass through underlying editor aliases.
        if self.palette.is_some() {
            return self.palette_key(key);
        }
        if self.page == shell::SETTINGS && self.terminals.picker.is_some() && self.live_input() {
            if let Some(input) = self.settings_key(key) {
                return input;
            }
        }
        if let Some(input) = self.focus_key(key) {
            return input;
        }
        if !self.text_input_active()
            && !self.vim_keys
            && matches!(key.code, KeyCode::Char('h' | 'j' | 'k' | 'l' | 'g' | 'G'))
        {
            return Input::Idle;
        }
        if self.editor.as_ref().is_some_and(Editor::browsing)
            || (self.page == PROFILES && self.prompt.is_none() && self.menu.is_none())
        {
            if key.code == KeyCode::Insert {
                key.code = KeyCode::Char('n');
            }
            if key.code == KeyCode::Delete {
                key.code = KeyCode::Char('d');
            }
        }
        if let Some(input) = self.background_key(key) {
            return input;
        }
        // Dialogs and local input modes consume Escape before page history.
        if key.code == KeyCode::Esc
            && self.can_switch_page()
            && !self.connection_filtering
            && self.runtime_picker.is_none()
            && self.terminals.picker.is_none()
            && !self.live_detail_open()
            && self.editor.is_none()
            && !(self.page == shell::CONNECTIONS && !self.connection_filter.is_empty())
        {
            return self.navigate_back();
        }
        if let Some(input) = self.viewer_page_key(key) {
            return input;
        }
        if self.page == shell::SETTINGS && self.live_input() {
            if let Some(input) = self.settings_key(key) {
                return input;
            }
        }
        if self.live_input() {
            if let Some(input) = self.live_key(key) {
                return input;
            }
        }
        if self.overview_input() {
            if let Some(input) = self.page_key(key) {
                return input;
            }
        }
        self.key_context(key)
    }

    fn key_context(&mut self, key: event::KeyEvent) -> Input {
        if self.help_scroll.is_some()
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && key.code == KeyCode::Char('c')
        {
            return Input::Quit;
        }
        if let Some(scroll) = &mut self.help_scroll {
            match key.code {
                KeyCode::Esc | KeyCode::Char('?') => self.help_scroll = None,
                KeyCode::Down | KeyCode::Char('j') => *scroll = scroll.saturating_add(1),
                KeyCode::Up | KeyCode::Char('k') => *scroll = scroll.saturating_sub(1),
                KeyCode::PageDown => *scroll = scroll.saturating_add(10),
                KeyCode::PageUp => *scroll = scroll.saturating_sub(10),
                KeyCode::Home | KeyCode::Char('g') => *scroll = 0,
                _ => {}
            }
            return Input::Idle;
        }
        if key.code == KeyCode::Char('?')
            && self.prompt.is_none()
            && self.recovery.is_none()
            && self.editor.as_ref().is_none_or(Editor::browsing)
            && self.viewer.as_ref().is_none_or(logs::Viewer::browsing)
        {
            self.help_scroll = Some(0);
            return Input::Idle;
        }
        if self.recovery.is_some() && !key.modifiers.contains(KeyModifiers::CONTROL) {
            return self.recover_key(key.code);
        }
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            // Detach with the draft intact; the next launch restores this exact step.
            return Input::Quit;
        }
        if self.recovery.is_some() {
            return Input::Idle;
        }
        if let Some(view) = &mut self.explanation {
            match key.code {
                KeyCode::Esc => self.explanation = None,
                KeyCode::Down | KeyCode::Char('j') => view.scroll = view.scroll.saturating_add(1),
                KeyCode::Up | KeyCode::Char('k') => view.scroll = view.scroll.saturating_sub(1),
                KeyCode::PageDown => view.scroll = view.scroll.saturating_add(10),
                KeyCode::PageUp => view.scroll = view.scroll.saturating_sub(10),
                _ => {}
            }
            return Input::Idle;
        }
        if let Some(view) = &mut self.diagnostics {
            if key.code == KeyCode::Esc && !view.handles_escape() {
                self.diagnostics = None;
            } else {
                view.key(key);
                if view.closed() {
                    self.diagnostics = None;
                }
            }
            return Input::Idle;
        }
        if self.binding_picker.is_some() {
            return self.key_binding_picker(key);
        }
        if self.menu.is_some() {
            return self.key_operations_menu(key);
        }
        if let Some(input) = self.key_node_transfer(key) {
            return input;
        }
        if self.prompt.as_ref().is_some_and(Prompt::confirmation) {
            return self.key_confirm_prompt(key);
        }
        if self.prompt.is_some() {
            return self.key_text_prompt(key);
        } else if key.code == KeyCode::Char('t')
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && self.kind == Kind::Rule
            && self.editor.as_ref().is_some_and(Editor::browsing)
        {
            self.prompt(Prompt::Explain(self.editor.as_ref().unwrap().name.clone()));
        } else if self.editor.is_some() {
            return self.key_editor_focus(key);
        } else if self.viewer.is_some() {
            return self.key_log_focus(key);
        } else if self.page == PROFILES {
            return self.key_profiles_focus(key);
        }
        Input::Idle
    }

    fn key_binding_picker(&mut self, key: event::KeyEvent) -> Input {
        let binding_is_proxy = self.kind == Kind::Proxy;
        if let Some(picker) = &mut self.binding_picker {
            match key.code {
                KeyCode::Esc => self.binding_picker = None,
                KeyCode::Up | KeyCode::Char('k') => {
                    picker.selected = picker.selected.saturating_sub(1)
                }
                KeyCode::Down | KeyCode::Char('j') => {
                    picker.selected =
                        (picker.selected + 1).min(picker.choices.len().saturating_sub(1))
                }
                KeyCode::Char(' ') => {
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
                KeyCode::Enter => {
                    let picker = self.binding_picker.take().unwrap();
                    return Input::Run(
                        Action::BindStart {
                            name: picker.name,
                            targets: picker.targets,
                        },
                        None,
                    );
                }
                _ => {}
            }
            return Input::Idle;
        }
        Input::Idle
    }

    fn key_operations_menu(&mut self, key: event::KeyEvent) -> Input {
        if let Some(selected) = self.menu {
            let operations = self.operations();
            match key.code {
                KeyCode::Esc | KeyCode::Left | KeyCode::Char('h') => self.menu = None,
                KeyCode::Up | KeyCode::Char('k') => self.menu = Some(selected.saturating_sub(1)),
                KeyCode::Down | KeyCode::Char('j') => {
                    self.menu = Some((selected + 1).min(operations.len().saturating_sub(1)))
                }
                KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => {
                    self.menu = None;
                    if let Some(operation) = operations.get(selected) {
                        return self.operation(*operation);
                    }
                }
                _ => {}
            }
            return Input::Idle;
        }
        Input::Idle
    }

    fn key_confirm_prompt(&mut self, key: event::KeyEvent) -> Input {
        if self.prompt.as_ref().is_some_and(Prompt::confirmation) {
            match key.code {
                KeyCode::Esc => self.prompt = None,
                KeyCode::Tab | KeyCode::BackTab | KeyCode::Left | KeyCode::Right => {
                    self.confirmation_accept = !self.confirmation_accept
                }
                KeyCode::Enter if !self.confirmation_accept => self.prompt = None,
                KeyCode::Enter => {
                    let prompt = self.prompt.take().unwrap();
                    match self.submit(prompt.clone(), "yes".into()) {
                        Ok(Some(action)) => {
                            return Input::Run(action, Some(Box::new((prompt, "yes".into()))))
                        }
                        Ok(None) => {}
                        Err(error) => {
                            self.fail(format!("{error:#}"), Resume::Prompt(prompt, "yes".into()))
                        }
                    }
                }
                KeyCode::Up => self.scroll = self.scroll.saturating_sub(1),
                KeyCode::Down => self.scroll = self.scroll.saturating_add(1),
                _ => {}
            }
            return Input::Idle;
        }
        Input::Idle
    }

    fn key_text_prompt(&mut self, key: event::KeyEvent) -> Input {
        match key.code {
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.input.clear();
                self.input_cursor = 0;
                self.prefilled_input = false;
            }
            KeyCode::Esc => {
                if let Some(Prompt::AddPath(name)) = self.prompt.take() {
                    self.prompt = Some(Prompt::AddName);
                    self.input = name;
                    self.input_cursor = self.input.chars().count();
                }
            }
            KeyCode::Left => {
                self.prefilled_input = false;
                self.input_cursor = self.input_cursor.saturating_sub(1);
            }
            KeyCode::Right => {
                self.prefilled_input = false;
                self.input_cursor = (self.input_cursor + 1).min(self.input.chars().count());
            }
            KeyCode::Home => {
                self.prefilled_input = false;
                self.input_cursor = 0;
            }
            KeyCode::End => {
                self.prefilled_input = false;
                self.input_cursor = self.input.chars().count();
            }
            KeyCode::Backspace | KeyCode::Delete if self.prefilled_input => {
                self.input.clear();
                self.input_cursor = 0;
                self.prefilled_input = false;
            }
            KeyCode::Backspace if self.input_cursor > 0 => {
                self.input_cursor -= 1;
                let byte = self.prompt_byte();
                self.input.remove(byte);
            }
            KeyCode::Delete if self.input_cursor < self.input.chars().count() => {
                let byte = self.prompt_byte();
                self.input.remove(byte);
            }
            KeyCode::Char(c)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                if self.prefilled_input {
                    self.input.clear();
                    self.input_cursor = 0;
                    self.prefilled_input = false;
                }
                let byte = self.prompt_byte();
                self.input.insert(byte, c);
                self.input_cursor += 1;
            }
            KeyCode::Enter => {
                let prompt = self.prompt.take().unwrap();
                let input = if prompt.confirmation() && self.input.trim().is_empty() {
                    "yes".to_owned()
                } else {
                    self.input.trim().to_owned()
                };
                match self.submit(prompt.clone(), input.clone()) {
                    Ok(Some(action)) => return Input::Run(action, Some(Box::new((prompt, input)))),
                    Ok(None) => {}
                    Err(error) => self.fail(format!("{error:#}"), Resume::Prompt(prompt, input)),
                }
            }
            _ => {}
        }
        Input::Idle
    }

    fn key_editor_focus(&mut self, key: event::KeyEvent) -> Input {
        let editor_kind = self.kind;
        let editor = self.editor.as_mut().unwrap();
        if editor.requests_subscription_update(key) {
            if editor.text() != editor.original {
                self.message = "请先按 Ctrl-S 保存订阅配置，再手动更新".into();
            } else {
                return Input::Run(Action::UpdateSubscriptions(editor.name.clone()), None);
            }
        } else if key.code == KeyCode::Char('o') && key.modifiers.contains(KeyModifiers::CONTROL) {
            if editor.browsing() {
                self.prompt(Prompt::Import);
            } else {
                self.message = "Confirm or cancel the field before importing".into();
            }
        } else if matches!(key.code, KeyCode::Char('i') | KeyCode::Char('I'))
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && editor.browsing()
        {
            self.prompt(
                if key.code == KeyCode::Char('I') || key.modifiers.contains(KeyModifiers::SHIFT) {
                    Prompt::ImportRule(crate::import::MergeMode::Append)
                } else {
                    Prompt::ImportNode(crate::import::MergeMode::Append)
                },
            );
        } else if matches!(key.code, KeyCode::Char('e') | KeyCode::Char('E'))
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && editor.browsing()
        {
            let share =
                key.code == KeyCode::Char('E') || key.modifiers.contains(KeyModifiers::SHIFT);
            if editor_kind == Kind::Proxy {
                return self.start_node_export(share);
            }
            let result = editor.export_selected_rule();
            match result {
                Ok(text) => self.prompt(Prompt::ExportRule { text }),
                Err(error) => self.fail(format!("{error:#}"), Resume::Editor),
            }
        } else if key.code == KeyCode::Char('s') && key.modifiers.contains(KeyModifiers::CONTROL) {
            match editor.prepare_save() {
                Ok(text) => {
                    return Input::Run(
                        Action::Save {
                            name: editor.name.clone(),
                            original: editor.original.clone(),
                            text,
                        },
                        None,
                    )
                }
                Err(error) => self.fail(format!("{error:#}"), Resume::Editor),
            }
        } else if key.code == KeyCode::Char('f')
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && editor.browsing()
        {
            // Ctrl-F matches the GUI; it opens the same search as `/`.
            if let Err(error) =
                editor.key(event::KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE))
            {
                self.fail(format!("{error:#}"), Resume::Editor);
            }
        } else if key.code == KeyCode::Esc && editor.can_close() {
            self.suspend_editor();
        } else if let Err(error) = editor.key(key) {
            self.fail(format!("{error:#}"), Resume::Editor);
        }
        Input::Idle
    }

    fn key_log_focus(&mut self, key: event::KeyEvent) -> Input {
        let viewer = self.viewer.as_mut().unwrap();
        let name = viewer.name.clone();
        match viewer.key(key) {
            logs::Command::Close => {
                let settings = viewer.settings.clone();
                self.log_settings.insert(name, settings);
                self.viewer = None;
                self.active_log = None;
            }
            logs::Command::Reconnect => return Input::Run(Action::Logs(name), None),
            logs::Command::Capture(level) => {
                return Input::Run(Action::SetLogLevel { name, level }, None)
            }
            logs::Command::None => {}
        }
        Input::Idle
    }

    fn key_profiles_focus(&mut self, key: event::KeyEvent) -> Input {
        let index = self.index();
        let name = self.selected().map(|r| r.instance.name.clone());
        let running = self.selected().is_some_and(|r| r.pid.is_some());
        if key.code == KeyCode::Char('g') && key.modifiers.is_empty() {
            if self.vim_g {
                self.selections[index] = 0;
                self.vim_g = false;
            } else {
                self.vim_g = true;
            }
            return Input::Idle;
        }
        self.vim_g = false;
        match key.code {
            KeyCode::Char('q') => return Input::Quit,
            KeyCode::Tab => {
                self.switch_core(next_kind(self.kind));
            }
            KeyCode::Left | KeyCode::Char('h') => self.switch_core(Kind::Proxy),
            KeyCode::Right | KeyCode::Char('l') => self.switch_core(Kind::Rule),
            KeyCode::Char('3') => self.switch_core(Kind::Server),
            KeyCode::Up | KeyCode::Char('k') => {
                self.selections[index] = self.selections[index].saturating_sub(1)
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.selections[index] =
                    (self.selections[index] + 1).min(self.rows().len().saturating_sub(1))
            }
            KeyCode::Home => self.selections[index] = 0,
            KeyCode::End | KeyCode::Char('G') => {
                self.selections[index] = self.rows().len().saturating_sub(1)
            }
            KeyCode::PageDown => {
                self.selections[index] = (self.selections[index]
                    + self.profile_viewport.get().max(1))
                .min(self.rows().len().saturating_sub(1))
            }
            KeyCode::PageUp => {
                self.selections[index] =
                    self.selections[index].saturating_sub(self.profile_viewport.get().max(1))
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.selections[index] = (self.selections[index]
                    + self.profile_viewport.get().max(1))
                .min(self.rows().len().saturating_sub(1))
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.selections[index] =
                    self.selections[index].saturating_sub(self.profile_viewport.get().max(1))
            }
            KeyCode::Char('n') => self.prompt(Prompt::AddName),
            KeyCode::Enter if name.is_some() => self.menu = Some(0),
            KeyCode::Char(c)
                if name.is_some()
                    && !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                // Activation is explicit; Space is reserved for local selection/pause.
                let operation = match c {
                    'c' if running => Operation::Stop,
                    'c' => Operation::Start,
                    's' => Operation::Start,
                    'x' => Operation::Stop,
                    'r' => Operation::Reload,
                    'e' => Operation::Edit,
                    'L' => Operation::Logs,
                    't' => {
                        if self.kind == Kind::Rule {
                            Operation::Explain
                        } else {
                            Operation::Diagnostics
                        }
                    }
                    'b' => Operation::Bind,
                    'd' => Operation::Remove,
                    'y' => Operation::Copy,
                    'E' => Operation::ExportConfig,
                    'P' => Operation::ExportTerminal,
                    'v' => return Input::Run(Action::Validate(name.unwrap()), None),
                    _ => return Input::Idle,
                };
                return self.operation(operation);
            }
            _ => {}
        }
        Input::Idle
    }

    fn operation_text(&self) -> String {
        if self.operation_steps.is_empty()
            || self
                .operation_result
                .as_ref()
                .is_some_and(|result| result != &self.message)
        {
            return t(&self.message).to_owned();
        }
        self.operation_steps.last().cloned().unwrap_or_default()
    }
    #[cfg(test)]
    async fn execute(
        &mut self,
        socket: &std::path::Path,
        action: Action,
        origin: Option<(Prompt, String)>,
    ) -> bool {
        let input = operations::OperationInput::capture(self);
        let result = operations::perform(socket, input, action.clone(), origin).await;
        self.apply_operation(
            result,
            &action,
            operations::ApplyContext {
                same: true,
                log_target_current: true,
                background: false,
            },
        )
    }
    fn draw(&self, terminal: &mut astra_tui_kit::Screen) -> Result<()> {
        astra_tui_kit::frame::draw(terminal, |frame| {
            self.render(frame);
            self.animations.finish_frame(frame);
        })?;
        Ok(())
    }
    /// Details of the selected instance: labels muted, state in its status color.
    fn selection_status(&self) -> Text<'static> {
        let Some(row) = self.selected() else {
            return Text::default();
        };
        let muted = Style::default().fg(dashboard::MUTED);
        let mut text = Text::from(vec![
            Line::styled(row.instance.config.display().to_string(), muted),
            Line::from(vec![
                Span::styled(format!("{}: ", t("State")), muted),
                Span::styled(
                    t(&row.state).to_owned(),
                    Style::default().fg(crate::theme::state_color(&row.state)),
                ),
            ]),
            Line::from(vec![
                Span::styled("PID ", muted),
                Span::raw(
                    row.pid
                        .map(|pid| pid.to_string())
                        .unwrap_or_else(|| "—".into()),
                ),
                Span::styled(format!(" · {} ", t("Uptime")), muted),
                Span::raw(uptime(row.uptime)),
            ]),
        ]);
        let mut lines: Vec<String> = Vec::new();
        if let Some(details) = self.details.get(&row.instance.name) {
            if !details.modes.is_empty() {
                lines.push(
                    details
                        .modes
                        .iter()
                        .map(|mode| t(mode))
                        .collect::<Vec<_>>()
                        .join(" + "),
                );
            }
            lines.extend(details.listeners.iter().cloned());
            if !details.bindings.is_empty() {
                lines.push(format!(
                    "{}: {}",
                    t("Startup bindings"),
                    details.bindings.join(", ")
                ));
            }
            lines.push(format!(
                "{}: {}",
                t("Log level"),
                details.log_level.as_str().to_uppercase()
            ));
            lines.push(if row.instance.kind == Kind::Rule {
                tr!("{} rules", details.rules)
            } else {
                tr!(
                    "{} outbounds: {}",
                    details.outbound_count,
                    details.outbounds.join(", ")
                )
            });
        }
        text.extend(lines.into_iter().map(Line::from));
        text
    }
    fn render(&self, f: &mut Frame<'_>) {
        self.hits.borrow_mut().clear();
        if let Some(view) = &self.monitor {
            view.render(f, f.area());
            return;
        }
        if let Some(view) = &self.diagnostics {
            view.render(f, f.area());
            self.render_palette(f);
            if let Some(scroll) = self.help_scroll {
                let text = self.command_help();
                let area = f.area();
                crate::help::render(
                    f,
                    area,
                    &text,
                    scroll,
                    Block::default().borders(Borders::ALL).title(t("Help")),
                    self.help_override.is_none(),
                );
            }
            self.render_background(f);
            return;
        }
        let content_area = self.render_frame(f);
        let list_page = self.page == PROFILES
            && self.editor.is_none()
            && self.viewer.is_none()
            && self.recovery.is_none();
        let detail_height = if self.prompt.is_some() {
            3
        } else if list_page {
            (Paragraph::new(self.selection_status())
                .wrap(Wrap { trim: false })
                .line_count(content_area.width.max(1)) as u16)
                .min((content_area.height / 2).max(1))
        } else {
            0
        };
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(3), Constraint::Length(detail_height)])
            .split(content_area);
        self.render_content(f, chunks[0]);
        self.render_detail(f, chunks[1], list_page);
        self.render_import_confirmation(f);
        self.render_force_start_confirmation(f);
        self.render_node_transfer(f);
        self.render_update_confirmation(f);
        if let Some(view) = &self.diagnostics {
            view.render(f, f.area());
        }
        if let Some(view) = &self.explanation {
            view.render(f, f.area());
        }
        self.render_selection_menu(f);
        self.render_palette(f);
        self.render_page_help(f);
        self.render_background(f);
    }

    fn render_content(&self, f: &mut Frame<'_>, area: ratatui::layout::Rect) {
        if let Some(recovery) = &self.recovery {
            f.render_widget(
                Paragraph::new(&*recovery.message)
                    .scroll((self.scroll, 0))
                    .wrap(Wrap { trim: false })
                    .style(Style::default().fg(Color::Yellow))
                    .block(Block::default().title(t("Paused")).borders(Borders::ALL)),
                area,
            );
        } else if let Some(editor) = &self.editor {
            editor.render(f, area);
        } else if self.page == LOGS {
            self.render_logs_page(f, area);
        } else if let Some(viewer) = &self.viewer {
            viewer.render(f, area);
        } else if self.page == shell::SETTINGS {
            self.render_settings(f, area);
        } else if self.kind == Kind::Server && self.page != PROFILES {
            self.render_server(f, area);
        } else {
            match self.page {
                shell::OVERVIEW => self.render_overview(f, area),
                shell::PROXIES => self.render_live_groups(f, area),
                shell::CONNECTIONS => self.render_connections(f, area),
                shell::RULES => self.render_rules(f, area),
                _ => self.render_profiles(f, area),
            }
        }
    }

    fn render_detail(&self, f: &mut Frame<'_>, area: ratatui::layout::Rect, list_page: bool) {
        let detail: Text = if let Some(prompt) = &self.prompt {
            if prompt.picker() {
                prompt.title().into()
            } else if prompt.confirmation() {
                format!(
                    "{}\n{}   {} · Tab / ←→ select · Enter apply · Esc cancel",
                    prompt.title(),
                    if self.confirmation_accept {
                        "  Cancel  "
                    } else {
                        "[ Cancel ]"
                    },
                    if self.confirmation_accept {
                        "[ Confirm ]"
                    } else {
                        "  Confirm  "
                    }
                )
                .into()
            } else {
                let byte = self.prompt_byte();
                format!(
                    "{}\n> {}▏{}",
                    prompt.title(),
                    &self.input[..byte],
                    &self.input[byte..]
                )
                .into()
            }
        } else if list_page {
            self.selection_status()
        } else {
            Text::default()
        };
        f.render_widget(
            Paragraph::new(detail)
                .wrap(Wrap { trim: false })
                .style(if self.prompt.is_some() {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default()
                }),
            area,
        );
    }

    fn render_import_confirmation(&self, f: &mut Frame<'_>) {
        if let Some(Prompt::ImportConfirm { notices, .. }) = &self.prompt {
            let popup = crate::help::popup(f.area(), notices);
            f.render_widget(Clear, popup);
            f.render_widget(
                Paragraph::new(notices.as_str())
                    .wrap(Wrap { trim: false })
                    .scroll((self.scroll, 0))
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(t("Replace editor draft?"))
                            .title_bottom(if self.confirmation_accept {
                                t("Cancel   [Confirm] · Tab select · Enter apply · Esc cancel")
                            } else {
                                t("[Cancel]   Confirm · Tab select · Enter apply · Esc cancel")
                            }),
                    ),
                popup,
            );
        }
    }

    fn render_force_start_confirmation(&self, f: &mut Frame<'_>) {
        if let Some(Prompt::ForceStart(name)) = &self.prompt {
            let text = format!(
                "{}\n\n{}\n\n{}",
                name,
                t("Replace previous session records for this instance and its stopped startup bindings. An unreachable previous process may still be running."),
                t("Active or inaccessible control endpoints still prevent replacement.")
            );
            let popup = crate::help::popup(f.area(), &text);
            f.render_widget(Clear, popup);
            f.render_widget(
                Paragraph::new(text)
                    .wrap(Wrap { trim: false })
                    .scroll((self.scroll, 0))
                    .block(
                        Block::default()
                            .borders(Borders::ALL)
                            .title(t("Force start and replace previous sessions?"))
                            .title_bottom(if self.confirmation_accept {
                                t("Cancel   [Confirm] · Tab select · Enter apply · Esc cancel")
                            } else {
                                t("[Cancel]   Confirm · Tab select · Enter apply · Esc cancel")
                            }),
                    ),
                popup,
            );
        }
    }

    fn render_update_confirmation(&self, f: &mut Frame<'_>) {
        if let Some(Prompt::UpdateConfirm(request)) = &self.prompt {
            let target = match &request.intent {
                crate::protocol::updates::UpdateIntent::Stage { package } => {
                    package.display().to_string()
                }
                crate::protocol::updates::UpdateIntent::Activate { release_id } => {
                    release_id.clone()
                }
                crate::protocol::updates::UpdateIntent::Rollback { target_directory } => {
                    target_directory.display().to_string()
                }
            };
            let text = format!(
                "{}\n\n{}\n\n{}: {}",
                t("Apply this exact update target?"),
                target,
                t("Operation"),
                request.operation_id
            );
            let popup = crate::help::popup(f.area(), &text);
            f.render_widget(Clear, popup);
            f.render_widget(
                Paragraph::new(text).wrap(Wrap { trim: false }).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(t("Confirm update"))
                        .title_bottom(if self.confirmation_accept {
                            t("Cancel   [Confirm] · Tab select · Enter apply · Esc cancel")
                        } else {
                            t("[Cancel]   Confirm · Tab select · Enter apply · Esc cancel")
                        }),
                ),
                popup,
            );
        }
    }

    fn render_selection_menu(&self, f: &mut Frame<'_>) {
        if self.menu.is_some() || self.binding_picker.is_some() {
            let height = if let Some(picker) = &self.binding_picker {
                picker.choices.len() + 2
            } else {
                self.operations().len() + 2
            };
            let area = f.area();
            let width = area.width.saturating_sub(4).min(64);
            let height = (height as u16).min(18).min(area.height.saturating_sub(2));
            let popup = ratatui::layout::Rect::new(
                area.x + area.width.saturating_sub(width) / 2,
                area.y + area.height.saturating_sub(height) / 2,
                width,
                height,
            );
            f.render_widget(Clear, popup);
            self.hits.borrow_mut().clear();
            let (items, selected, title, help): (Vec<ListItem>, usize, String, &str) =
                if let Some(picker) = &self.binding_picker {
                    (
                        picker
                            .choices
                            .iter()
                            .map(|(name, kind)| {
                                ListItem::new(format!(
                                    "[{}] {}  ·  {}",
                                    if picker.targets.contains(name) {
                                        "✓"
                                    } else {
                                        " "
                                    },
                                    name,
                                    t(if *kind == Kind::Proxy {
                                        "Proxy"
                                    } else {
                                        "Rules"
                                    })
                                ))
                            })
                            .collect(),
                        picker.selected,
                        tr!("Startup bindings · {}", picker.name),
                        t("Space select · Enter save · Esc cancel"),
                    )
                } else {
                    (
                        self.operations()
                            .iter()
                            .map(|operation| ListItem::new(operation.label()))
                            .collect(),
                        self.menu.unwrap_or(0),
                        tr!(
                            "Actions · {}",
                            self.selected()
                                .map(|row| row.instance.name.as_str())
                                .unwrap_or_default()
                        ),
                        t("↑↓ select · Enter apply · Esc back"),
                    )
                };
            let mut state = ListState::default().with_selected(Some(selected));
            f.render_stateful_widget(
                List::new(items)
                    .block(
                        Block::default()
                            .title(title)
                            .title_bottom(help)
                            .borders(Borders::ALL),
                    )
                    .highlight_style(crate::theme::selected())
                    .highlight_symbol("› "),
                popup,
                &mut state,
            );
            self.menu_hits(popup, state.offset());
        }
    }

    fn render_page_help(&self, f: &mut Frame<'_>) {
        if let Some(scroll) = self.help_scroll {
            let text = self.command_help();
            let popup = crate::help::popup(f.area(), &text);
            crate::help::render(
                f,
                popup,
                &text,
                scroll,
                Block::default()
                    .title(t("Page operations"))
                    .title_bottom(Line::from(crate::theme::key_hints(t(
                        "↑↓ scroll · ? / Esc close",
                    ))))
                    .borders(Borders::ALL),
                self.help_override.is_none(),
            );
        }
    }
}

pub(crate) fn action_description(action: &Action) -> String {
    let (label, name) = match action {
        Action::Logs(name) if name == "@tui" => ("TUI log", ""),
        Action::Logs(name) => ("Logs", name.as_str()),
        Action::Diagnostics(name) => ("Connectivity / Exit IP", name.as_str()),
        Action::NetTool { name, .. } => ("Network tools", name.as_str()),
        Action::Runtime { name, .. } => ("Applying runtime change", name.as_str()),
        Action::PrepareElevation { name, .. } => {
            ("Enable TUN with administrator permission", name.as_str())
        }
        Action::CancelElevation { name, .. } => ("Cancel TUN elevation", name.as_str()),
        Action::WaitElevation { name, .. } => ("Wait for the elevated core", name.as_str()),
        Action::ExplainRoute { name, .. } => ("Route explanation", name.as_str()),
        Action::Create { name, .. } => ("Create", name.as_str()),
        Action::Add(instance) => ("Add", instance.name.as_str()),
        Action::Start(name) => ("Start", name.as_str()),
        Action::ForceStart(name) => ("Force start", name.as_str()),
        Action::Stop(name) => ("Stop", name.as_str()),
        Action::Restart(name) => ("Restart", name.as_str()),
        Action::Reload(name) => ("Reload", name.as_str()),
        Action::Validate(name) => ("Validate", name.as_str()),
        Action::Read(name) => ("Edit", name.as_str()),
        Action::Save { name, .. } => ("Save", name.as_str()),
        Action::UpdateSubscriptions(name) => ("Update subscriptions", name.as_str()),
        Action::UpdatePackage(_) => ("Import local update package", ""),
        Action::UpdateStatus => ("Update status", ""),
        Action::Rollback => ("Roll back release", ""),
        Action::BindStart { name, .. } => ("Startup bindings", name.as_str()),
        Action::Remove(name) => ("Remove", name.as_str()),
        Action::SetConfig(name, _) => ("Config path", name.as_str()),
        Action::CloneConfig(name, _) => ("Copy config", name.as_str()),
        Action::Shutdown => ("Stop all", ""),
        _ => ("Processing request", ""),
    };
    format!("{} {}", t(label), name).trim().to_owned()
}

fn uptime(seconds: u64) -> String {
    format!(
        "{:02}:{:02}:{:02}",
        seconds / 3600,
        seconds / 60 % 60,
        seconds % 60
    )
}

fn absolute(text: &str, directory: &std::path::Path) -> Result<PathBuf> {
    anyhow::ensure!(!text.is_empty(), t("Enter a configuration file path"));
    let path = PathBuf::from(text);
    Ok(if path.is_absolute() {
        path
    } else {
        directory.join(path)
    })
}

pub async fn run(
    socket: PathBuf,
    context: crate::windows::Context,
    launch: crate::windows::Launch,
    resume_ui: bool,
    restart_ready: Option<PathBuf>,
) -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(event_loop::run_local(
            socket,
            context,
            launch,
            resume_ui,
            restart_ready,
        ))
        .await
}
/// Ctrl-C is normally a raw-mode key event, but sudo needs a real terminal.
/// Ignore the signal in this process while sudo runs so it reaches sudo only.
#[cfg(unix)]
struct IgnoreInterrupt(libc::sighandler_t);
#[cfg(unix)]
impl IgnoreInterrupt {
    fn install() -> Self {
        Self(unsafe { libc::signal(libc::SIGINT, libc::SIG_IGN) })
    }
}
#[cfg(unix)]
impl Drop for IgnoreInterrupt {
    fn drop(&mut self) {
        unsafe {
            libc::signal(libc::SIGINT, self.0);
        }
    }
}

/// Ask the manager for a hand-off, run sudo on the real terminal, and wait for
/// the elevated core as a background job so the UI stays responsive.
#[cfg(unix)]
async fn elevate(
    ui: &mut Ui,
    jobs: &mut background::Jobs,
    socket: &std::path::Path,
    name: String,
    expected_pid: Option<u32>,
    terminal: &mut astra_tui_kit::Screen,
    screen: &mut TerminalSession,
) {
    let reply = match call(
        socket,
        Action::PrepareElevation {
            name: name.clone(),
            expected_pid,
        },
    )
    .await
    {
        Ok(reply) => reply,
        Err(error) => {
            ui.message = format!("{error:#}");
            return;
        }
    };
    let offer: astra_platform::unix_handoff::Offer = match serde_json::from_str(&reply.text) {
        Ok(offer) => offer,
        Err(error) => {
            ui.message = format!("{error:#}");
            return;
        }
    };
    // Re-check right before sudo runs it: the reply only names a path.
    if let Err(error) = astra_platform::unix_handoff::ensure_root_controlled(&offer.binary)
        .and_then(|_| astra_platform::unix_handoff::ensure_root_controlled(&offer.sudo))
    {
        ui.message = format!("{error:#}");
        return;
    }
    let launched = (|| -> Result<std::process::ExitStatus> {
        screen.suspend()?;
        #[cfg(unix)]
        let _interrupt = IgnoreInterrupt::install();
        std::process::Command::new(&offer.sudo)
            .arg(&offer.binary)
            .args(["--unix-elevated"])
            .arg(&offer.socket)
            .arg(&offer.ticket)
            .status()
            .map_err(Into::into)
    })();
    // Always restore the TUI, including when sudo or the terminal setup fails.
    let restored = (|| -> Result<()> {
        screen.resume()?;
        terminal.backend_mut().writer_mut().invalidate();
        Ok(())
    })();
    if let Err(error) = restored {
        ui.message = format!("{error:#}");
        return;
    }
    match launched {
        Ok(status) if status.success() => {
            ui.message =
                t("Administrator permission granted; waiting for the elevated core").into();
            jobs.start(
                ui,
                Action::WaitElevation {
                    name,
                    ticket: offer.ticket,
                },
                None,
            );
        }
        Ok(_) => {
            let _ = call(
                socket,
                Action::CancelElevation {
                    name: name.clone(),
                    ticket: offer.ticket,
                },
            )
            .await;
            ui.message = t("Administrator permission was not granted").into();
        }
        Err(error) => {
            let _ = call(
                socket,
                Action::CancelElevation {
                    name,
                    ticket: offer.ticket,
                },
            )
            .await;
            ui.message = format!("{error:#}");
        }
    }
}

#[cfg(not(unix))]
async fn elevate(
    ui: &mut Ui,
    _jobs: &mut background::Jobs,
    _socket: &std::path::Path,
    _name: String,
    _expected_pid: Option<u32>,
    _terminal: &mut astra_tui_kit::Screen,
    _screen: &mut TerminalSession,
) {
    ui.message = t("TUN elevation is only available on Linux").into();
}

#[cfg(test)]
#[path = "test/ui_test.rs"]
mod tests;
