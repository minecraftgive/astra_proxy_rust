mod background;
mod dashboard;
mod detached;
mod live;
mod pages;
mod shell;
use crate::editor::Editor;
use crate::logs;
use crate::protocol::*;
use crate::{i18n::t, tr};
use anyhow::{Context, Result};
use crossterm::{
    event::{
        self, DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
        Event, KeyCode, KeyEventKind, KeyModifiers,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
    Frame, Terminal,
};
use serde::{Deserialize, Serialize};
use shell::{LOGS, PROFILES};
use std::{
    collections::HashMap,
    io::{self, Stdout},
    path::PathBuf,
    time::{Duration, Instant},
};

struct Screen;
impl Drop for Screen {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(
            io::stdout(),
            DisableBracketedPaste,
            DisableMouseCapture,
            crossterm::cursor::Show,
            LeaveAlternateScreen
        );
    }
}
#[derive(Clone, Serialize, Deserialize)]
enum Prompt {
    AddName,
    AddPath(String),
    Config(String),
    Clone(String),
    Remove(String),
    Shutdown,
    Discard,
    Import,
    ImportConfirm { text: String, notices: String },
    Explain(String),
    CloseAll { name: String, pid: Option<u32> },
}
impl Prompt {
    fn confirmation(&self) -> bool {
        matches!(
            self,
            Self::Remove(_)
                | Self::Shutdown
                | Self::Discard
                | Self::ImportConfirm { .. }
                | Self::CloseAll { .. }
        )
    }
    fn title(&self) -> &str {
        match self {
            Self::AddName => t("Name (1–24: a-z, 0-9, -_)"),
            Self::AddPath(_) => t("Configuration path"),
            Self::Config(_) => t("Configuration path"),
            Self::Clone(_) => t("Copy to path"),
            Self::Remove(_) => t("Remove instance? Configuration is kept."),
            Self::Shutdown => t("Stop all instances?"),
            Self::Discard => t("Discard changes?"),
            Self::Import => t("Import config or proxy share links"),
            Self::ImportConfirm { .. } => t("Replace editor draft?"),
            Self::Explain(_) => t("Enter host[:port] [tcp|udp] [process name or path]"),
            Self::CloseAll { .. } => {
                t("Close all connections? Applications reconnect on their own.")
            }
        }
    }
}

#[derive(Clone, Copy)]
enum Operation {
    UpdateSubscriptions,
    Start,
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
    Explain,
}
impl Operation {
    fn label(self) -> &'static str {
        t(match self {
            Self::Start => "Start",
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
            Self::Explain => "Route explanation",
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
struct Recovery {
    message: String,
    resume: Resume,
}
#[derive(Clone, Serialize, Deserialize)]
struct Session {
    kind: Kind,
    selections: [usize; 2],
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
            kind: Kind::Proxy,
            selections: [0; 2],
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
        if let Some(editor) = &mut session.editor {
            editor.normalize();
        }
        Ok(session)
    }
    fn save(&self, path: &std::path::Path) -> Result<()> {
        let data = serde_json::to_vec(self)?;
        anyhow::ensure!(data.len() as u64 <= SESSION_LIMIT, "Session exceeds 4 MiB");
        crate::manager::atomic_write(path, &data)
    }
}

struct Ui {
    session: Session,
    page: usize,
    palette: Option<shell::Palette>,
    runtime_snapshot: Option<astra_ipc::runtime::Snapshot>,
    runtime_instance: String,
    runtime_error: String,
    runtime_sample: Instant,
    rates: [u64; 2],
    history: [std::collections::VecDeque<u64>; 2],
    group_selection: usize,
    connection_selection: usize,
    connection_filter: String,
    connection_filtering: bool,
    runtime_picker: Option<(String, usize)>,
    hits: std::cell::RefCell<Vec<(ratatui::layout::Rect, dashboard::Target)>>,
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
    explanation: Option<crate::explanation::View>,
    vim_g: bool,
    help_scroll: Option<u16>,
    operation_steps: Vec<String>,
    operation_result: Option<String>,
    tasks: background::Tasks,
    tui_viewer: Option<logs::Viewer>,
    operation_failed: bool,
    ready_editors: HashMap<String, Editor>,
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
    Run(Action, Option<(Prompt, String)>),
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
            "External draft: {} · F5 read again · Ctrl-S save",
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
            .context(t("No external draft; press F4 to open an editor"))?;
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
            "External draft: {} · F5 read again · Ctrl-S save",
            path.display()
        );
        Ok(())
    }
    fn new(session: Session) -> Self {
        Self {
            session,
            page: 0,
            palette: None,
            runtime_snapshot: None,
            runtime_instance: String::new(),
            runtime_error: String::new(),
            runtime_sample: Instant::now(),
            rates: [0, 0],
            history: Default::default(),
            group_selection: 0,
            connection_selection: 0,
            connection_filter: String::new(),
            connection_filtering: false,
            runtime_picker: None,
            hits: std::cell::RefCell::new(Vec::new()),
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
            explanation: None,
            vim_g: false,
            help_scroll: None,
            operation_steps: Vec::new(),
            operation_result: None,
            tasks: Default::default(),
            tui_viewer: None,
            operation_failed: false,
            ready_editors: HashMap::new(),
        }
    }
    fn index(&self) -> usize {
        if self.kind == Kind::Proxy {
            0
        } else {
            1
        }
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
        operations.extend([
            Operation::Edit,
            Operation::Logs,
            Operation::Bind,
            Operation::Config,
            Operation::Copy,
        ]);
        if row.instance.kind == Kind::Proxy {
            operations.push(Operation::Diagnostics);
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
                if let Some(editor) = self.ready_editors.remove(&name) {
                    self.editor = Some(editor);
                    return Input::Idle;
                }
                Some(Action::Read(name))
            }
            Operation::Logs => return self.open_logs(name),
            Operation::Diagnostics => {
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
        for (index, kind) in [Kind::Proxy, Kind::Rule].into_iter().enumerate() {
            let count = self
                .rows
                .iter()
                .filter(|row| row.instance.kind == kind)
                .count();
            self.selections[index] = self.selections[index].min(count.saturating_sub(1));
        }
    }
    fn prompt(&mut self, prompt: Prompt) {
        if matches!(
            prompt,
            Prompt::AddName | Prompt::Config(_) | Prompt::Clone(_)
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
            _ => String::new(),
        };
        self.prefilled_input = !self.input.is_empty();
        self.prompt = Some(prompt);
    }
    fn paste_prompt(&mut self, text: &str) {
        if self.prefilled_input {
            self.input.clear();
            self.prefilled_input = false;
        }
        let links =
            matches!(self.prompt, Some(Prompt::Import)) && astra_convert::share::is_share(text);
        self.input.extend(
            text.chars()
                .filter(|character| !character.is_control() || (links && *character == '\n')),
        );
    }
    fn paste(&mut self, text: &str) {
        if self.background_paste(text) {
            return;
        }
        if self.help_scroll.is_some() {
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
            KeyCode::Char('b') => {
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
            Prompt::Remove(name) if input == "yes" => Some(Action::Remove(name)),
            Prompt::Shutdown if input == "yes" => Some(Action::Shutdown),
            Prompt::Discard if input == "yes" => {
                self.editor = None;
                None
            }
            Prompt::CloseAll { name, pid } if input == "yes" => Some(Action::Runtime {
                name,
                command: astra_ipc::runtime::Command::Disconnect(None),
                expected_pid: pid,
            }),
            Prompt::Explain(name) => Some(Action::ExplainRoute {
                name,
                request: crate::explanation::request(&input)?,
            }),
            Prompt::Import => {
                let share = astra_convert::share::is_share(&input);
                let editor = self.editor.as_ref().context("Missing editor")?;
                let (imported, label) = if share {
                    (
                        crate::import::links(&input, self.kind, &editor.text())?,
                        t("Import node links").to_owned(),
                    )
                } else {
                    let path = crate::import::path(&input, &self.working_dir)?;
                    (
                        crate::import::load(&path, self.kind, &editor.text())?,
                        path.display().to_string(),
                    )
                };
                if editor.text() != editor.original || !imported.warnings.is_empty() {
                    let mut notices = vec![label];
                    if editor.text() != editor.original {
                        notices.push(t("Unsaved draft will be replaced").into());
                    }
                    notices.extend(imported.warnings.iter().map(|s| t(s).to_owned()));
                    self.prompt = Some(Prompt::ImportConfirm {
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
    fn key(&mut self, key: event::KeyEvent) -> Input {
        if let Some(input) = self.window_key(key) {
            return input;
        }
        if let Some(input) = self.background_key(key) {
            return input;
        }
        if self.palette.is_some() {
            return self.palette_key(key);
        }
        if let Some(input) = self.viewer_page_key(key) {
            return input;
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
            if key.code == KeyCode::Esc {
                self.diagnostics = None;
            } else {
                view.key(key);
            }
            return Input::Idle;
        }
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
        if self.prompt.as_ref().is_some_and(Prompt::confirmation) {
            match key.code {
                KeyCode::Esc | KeyCode::Char('n') => self.prompt = None,
                KeyCode::Enter | KeyCode::Char('y') => {
                    let prompt = self.prompt.take().unwrap();
                    match self.submit(prompt.clone(), "yes".into()) {
                        Ok(Some(action)) => {
                            return Input::Run(action, Some((prompt, "yes".into())))
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
        if self.prompt.is_some() {
            match key.code {
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.input.clear();
                    self.prefilled_input = false;
                }
                KeyCode::Esc => {
                    if let Some(Prompt::AddPath(name)) = self.prompt.take() {
                        self.prompt = Some(Prompt::AddName);
                        self.input = name;
                    }
                }
                KeyCode::Backspace => {
                    if self.prefilled_input {
                        self.input.clear();
                        self.prefilled_input = false;
                    } else {
                        self.input.pop();
                    }
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    if self.prefilled_input {
                        self.input.clear();
                        self.prefilled_input = false;
                    }
                    self.input.push(c)
                }
                KeyCode::Enter => {
                    let prompt = self.prompt.take().unwrap();
                    let input = if prompt.confirmation() && self.input.trim().is_empty() {
                        "yes".to_owned()
                    } else {
                        self.input.trim().to_owned()
                    };
                    match self.submit(prompt.clone(), input.clone()) {
                        Ok(Some(action)) => return Input::Run(action, Some((prompt, input))),
                        Ok(None) => {}
                        Err(error) => {
                            self.fail(format!("{error:#}"), Resume::Prompt(prompt, input))
                        }
                    }
                }
                _ => {}
            }
        } else if key.code == KeyCode::Char('t')
            && key.modifiers.contains(KeyModifiers::CONTROL)
            && self.kind == Kind::Rule
            && self.editor.as_ref().is_some_and(Editor::browsing)
        {
            self.prompt(Prompt::Explain(self.editor.as_ref().unwrap().name.clone()));
        } else if let Some(editor) = &mut self.editor {
            if editor.requests_subscription_update(key) {
                if editor.text() != editor.original {
                    self.message = "请先按 Ctrl-S 保存订阅配置，再手动更新".into();
                } else {
                    return Input::Run(Action::UpdateSubscriptions(editor.name.clone()), None);
                }
            } else if key.code == KeyCode::F(4) {
                return Input::ExternalEditor;
            } else if key.code == KeyCode::F(5) {
                if !editor.browsing() {
                    self.message =
                        "Confirm or cancel the field before reading external changes".into();
                } else if let Err(error) = self.read_external() {
                    self.fail(format!("{error:#}"), Resume::Editor);
                }
            } else if key.code == KeyCode::Char('o')
                && key.modifiers.contains(KeyModifiers::CONTROL)
            {
                if editor.browsing() {
                    self.prompt(Prompt::Import);
                } else {
                    self.message = "Confirm or cancel the field before importing".into();
                }
            } else if key.code == KeyCode::Char('s')
                && key.modifiers.contains(KeyModifiers::CONTROL)
            {
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
            } else if key.code == KeyCode::Esc && editor.can_close() {
                if editor.text() != editor.original {
                    self.prompt(Prompt::Discard);
                } else {
                    self.editor = None;
                }
            } else if let Err(error) = editor.key(key) {
                self.fail(format!("{error:#}"), Resume::Editor);
            }
        } else if let Some(viewer) = &mut self.viewer {
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
        } else if self.page == PROFILES {
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
                KeyCode::Char('Q') => self.prompt(Prompt::Shutdown),
                KeyCode::Tab => {
                    self.switch_core(if self.kind == Kind::Proxy {
                        Kind::Rule
                    } else {
                        Kind::Proxy
                    });
                }
                KeyCode::Left | KeyCode::Char('h') => self.switch_core(Kind::Proxy),
                KeyCode::Right | KeyCode::Char('l') => self.switch_core(Kind::Rule),
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
                    self.selections[index] =
                        (self.selections[index] + 10).min(self.rows().len().saturating_sub(1))
                }
                KeyCode::PageUp => {
                    self.selections[index] = self.selections[index].saturating_sub(10)
                }
                KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.selections[index] =
                        (self.selections[index] + 10).min(self.rows().len().saturating_sub(1))
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.selections[index] = self.selections[index].saturating_sub(10)
                }
                KeyCode::Char('n') => self.prompt(Prompt::AddName),
                KeyCode::Enter if name.is_some() => self.menu = Some(0),
                KeyCode::Char(c)
                    if name.is_some()
                        && !key
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    // Same verbs as every other page: c/Space connect, r apply, R restart.
                    let operation = match c {
                        'c' | ' ' if running => Operation::Stop,
                        'c' | ' ' => Operation::Start,
                        's' => Operation::Start,
                        'x' => Operation::Stop,
                        'r' => Operation::Reload,
                        'R' => Operation::Restart,
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
                        'v' => return Input::Run(Action::Validate(name.unwrap()), None),
                        _ => return Input::Idle,
                    };
                    return self.operation(operation);
                }
                _ => {}
            }
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
    fn progress(
        &mut self,
        text: String,
        terminal: &mut Option<&mut Terminal<CrosstermBackend<Stdout>>>,
    ) {
        astra_ipc::progress::report("UI", &text);
        if terminal.is_some() {
            self.operation_steps
                .push(format!("{}. {}", self.operation_steps.len() + 1, text));
            if let Some(terminal) = terminal.as_deref_mut() {
                let _ = self.draw(terminal);
            }
        }
    }
    async fn execute(
        &mut self,
        socket: &std::path::Path,
        action: Action,
        origin: Option<(Prompt, String)>,
    ) -> bool {
        self.execute_inner(socket, action, origin, None).await
    }
    async fn execute_inner(
        &mut self,
        socket: &std::path::Path,
        mut action: Action,
        mut origin: Option<(Prompt, String)>,
        mut terminal: Option<&mut Terminal<CrosstermBackend<Stdout>>>,
    ) -> bool {
        if let Action::Runtime { name, command, .. } = &action {
            self.progress(t("Applying runtime change").into(), &mut terminal);
            match call(socket, action.clone()).await.and_then(|reply| {
                Ok(serde_json::from_str::<astra_ipc::runtime::Snapshot>(
                    &reply.text,
                )?)
            }) {
                Ok(snapshot) => {
                    self.accept_runtime(name.clone(), snapshot);
                    self.message = t(
                        if matches!(command, astra_ipc::runtime::Command::TestGroup(_)) {
                            "Group test started"
                        } else {
                            "Runtime updated; profile unchanged"
                        },
                    )
                    .into();
                }
                Err(error) => {
                    self.operation_failed = true;
                    self.message = format!("{error:#}");
                }
            }
            return false;
        }
        if let Action::Diagnostics(name) = &action {
            self.diagnostics = Some(crate::diagnostics::View::start(socket, name.clone()));
            self.message.clear();
            return false;
        }
        if let Action::Logs(name) = &action {
            self.active_log = Some(name.clone());
            if self
                .viewer
                .as_ref()
                .is_none_or(|viewer| viewer.name != *name)
            {
                let settings = self.log_settings.get(name).cloned().unwrap_or_default();
                self.viewer = Some(logs::Viewer::new(name.clone(), settings));
            }
            let result = async {
                let level = call(socket, Action::GetLogLevel(name.clone()))
                    .await?
                    .text
                    .parse::<LogLevel>()?;
                let viewer = self.viewer.as_mut().unwrap();
                viewer.capture_level = level;
                viewer.connect(socket).await
            }
            .await;
            if let Err(error) = result {
                self.fail(format!("{error:#}"), Resume::Request(action));
            }
            return false;
        }
        loop {
            // A missing configuration must be edited before a start/validation proceeds.
            if let Action::Start(name) | Action::Restart(name) | Action::Validate(name) = &action {
                self.progress(t("Checking saved configuration").to_owned(), &mut terminal);
                match call(socket, Action::PrepareConfig(name.clone())).await {
                    Ok(reply) if !reply.text.is_empty() => {
                        self.editor =
                            Some(Editor::new(name.clone(), reply.text).with_kind(self.kind));
                        self.message = "Review configuration · Ctrl-S save".into();
                        self.refresh_node_tags(socket).await;
                        return false;
                    }
                    Ok(_) => {}
                    Err(error) => {
                        self.fail(format!("{error:#}"), Resume::Request(action));
                        return false;
                    }
                }
            }
            self.progress(
                format!(
                    "{} · {}",
                    action_description(&action),
                    t("Waiting for manager confirmation")
                ),
                &mut terminal,
            );
            let reply = match call(socket, action.clone()).await {
                Ok(reply) => reply,
                Err(error) => {
                    let resume = origin
                        .map(|(p, text)| Resume::Prompt(p, text))
                        .unwrap_or(Resume::Request(action));
                    self.fail(format!("{error:#}"), resume);
                    return false;
                }
            };
            self.refresh_paused = false;
            match action {
                Action::Create { name, kind } => {
                    self.kind = kind;
                    self.refresh_binding(socket, &name).await;
                    action = Action::Read(name);
                    origin = None;
                }
                Action::Add(i) => {
                    self.kind = i.kind;
                    self.refresh_binding(socket, &i.name).await;
                    action = Action::Read(i.name);
                    // Registration succeeded: retries now resume reading, never add twice.
                    origin = None;
                }
                Action::SetConfig(name, _) => {
                    self.refresh_binding(socket, &name).await;
                    action = Action::Read(name);
                    origin = None;
                }
                Action::Read(name) => {
                    self.editor = Some(Editor::new(name, reply.text).with_kind(self.kind));
                    self.message.clear();
                    self.refresh_node_tags(socket).await;
                    return false;
                }
                Action::ExplainRoute { .. } => {
                    match serde_json::from_str(&reply.text) {
                        Ok(report) => {
                            self.explanation = Some(crate::explanation::View { report, scroll: 0 });
                            self.message.clear();
                        }
                        Err(error) => self.fail(error.to_string(), Resume::Request(action)),
                    }
                    return false;
                }
                Action::SetLogLevel { level, .. } => {
                    if let Some(viewer) = &mut self.viewer {
                        viewer.capture_changed(level);
                    }
                    self.message = tr!("记录级别已改为 {}，无需重启", level.as_str());
                    return false;
                }
                Action::UpdateSubscriptions(_) => {
                    self.refresh_node_tags(socket).await;
                    self.message = reply.text;
                    return false;
                }
                Action::Save { .. } => {
                    if let Some(editor) = &mut self.editor {
                        editor.original = editor.text();
                    }
                    self.message = reply.text;
                    return false;
                }
                Action::List => {
                    self.update_rows(reply);
                    return false;
                }
                Action::BindStart { .. } => {
                    self.message = reply.text;
                    if let Ok(reply) = call(socket, Action::List).await {
                        self.update_rows(reply);
                    }
                    return false;
                }
                Action::Shutdown => {
                    self.message = reply.text;
                    return true;
                }
                _ => {
                    self.message = reply.text;
                    return false;
                }
            }
        }
    }
    async fn refresh_binding(&mut self, socket: &std::path::Path, name: &str) {
        if let Ok(reply) = call(socket, Action::List).await {
            self.update_rows(reply);
            if let Some(index) = self.rows().iter().position(|r| r.instance.name == name) {
                let kind = self.index();
                self.selections[kind] = index;
            }
        }
    }
    async fn refresh_subscription_nodes(&mut self, socket: &std::path::Path) {
        if self.kind != Kind::Proxy {
            return;
        }
        let Some(name) = self.editor.as_ref().map(|e| e.name.clone()) else {
            return;
        };
        if !self
            .details
            .get(&name)
            .is_some_and(|d| d.subscription_nodes_supported)
        {
            if let Some(path) = self
                .rows
                .iter()
                .find(|row| row.instance.name == name)
                .map(|row| row.instance.config.clone())
            {
                if let Ok(Ok(nodes)) =
                    tokio::task::spawn_blocking(move || crate::subscription_view::read(&path)).await
                {
                    if let Some(editor) = &mut self.editor {
                        editor.set_subscription_nodes(nodes);
                    }
                }
            }
            return;
        }
        if let Ok(nodes) = call(socket, Action::SubscriptionNodes(name))
            .await
            .and_then(|r| Ok(serde_json::from_str(&r.text)?))
        {
            if let Some(editor) = &mut self.editor {
                editor.set_subscription_nodes(nodes);
            }
        }
    }
    async fn refresh_node_tags(&mut self, socket: &std::path::Path) {
        self.refresh_subscription_nodes(socket).await;
        if self.kind != Kind::Rule {
            return;
        }
        let Some(name) = self.editor.as_ref().map(|editor| editor.name.clone()) else {
            return;
        };
        if !self
            .details
            .get(&name)
            .is_some_and(|details| details.node_tags_supported)
        {
            return;
        }
        match call(socket, Action::NodeTags(name))
            .await
            .and_then(|reply| Ok(serde_json::from_str::<Vec<String>>(&reply.text)?))
        {
            Ok(tags) => self.editor.as_mut().unwrap().set_node_tags(tags),
            Err(_) => {
                self.message = "Node suggestions unavailable; manual entry is supported".into()
            }
        }
    }
    fn draw(&self, terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
        terminal.draw(|frame| self.render(frame))?;
        Ok(())
    }
    fn selection_status(&self) -> String {
        let Some(row) = self.selected() else {
            return String::new();
        };
        let mut lines = vec![
            row.instance.config.display().to_string(),
            format!("{}: {}", t("State"), t(&row.state)),
            format!(
                "PID {} · {} {}",
                row.pid
                    .map(|pid| pid.to_string())
                    .unwrap_or_else(|| "—".into()),
                t("Uptime"),
                uptime(row.uptime)
            ),
        ];
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
        lines.join("\n")
    }
    fn help_page(&self) -> crate::help::Page {
        use crate::help::Page;
        if self.recovery.is_some() {
            Page::Recovery
        } else if self.menu.is_some() {
            Page::Menu
        } else if self.binding_picker.is_some() {
            Page::Bindings
        } else if self.explanation.is_some() {
            Page::Route
        } else if self.diagnostics.is_some() {
            Page::Diagnostics
        } else if self.viewer.is_some() {
            Page::Logs
        } else if let Some(editor) = &self.editor {
            if editor.subscription_page() {
                Page::Subscriptions
            } else if self.kind == Kind::Rule {
                Page::RuleEditor
            } else {
                Page::ProxyEditor
            }
        } else if self.page == PROFILES {
            Page::Instances
        } else {
            Page::General
        }
    }
    fn render(&self, f: &mut Frame<'_>) {
        self.hits.borrow_mut().clear();
        if let Some(view) = &self.diagnostics {
            view.render(f, f.area());
            if let Some(scroll) = self.help_scroll {
                let text = crate::help::text(self.help_page());
                f.render_widget(
                    Paragraph::new(text)
                        .scroll((scroll, 0))
                        .block(Block::default().borders(Borders::ALL).title(t("Help"))),
                    f.area(),
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
        if let Some(recovery) = &self.recovery {
            f.render_widget(
                Paragraph::new(&*recovery.message)
                    .scroll((self.scroll, 0))
                    .wrap(Wrap { trim: false })
                    .style(Style::default().fg(Color::Yellow))
                    .block(Block::default().title(t("Paused")).borders(Borders::ALL)),
                chunks[0],
            );
        } else if let Some(editor) = &self.editor {
            editor.render(f, chunks[0]);
        } else if self.page == LOGS {
            self.render_logs_page(f, chunks[0]);
        } else if let Some(viewer) = &self.viewer {
            viewer.render(f, chunks[0]);
        } else {
            match self.page {
                shell::OVERVIEW => self.render_overview(f, chunks[0]),
                shell::PROXIES => self.render_live_groups(f, chunks[0]),
                shell::CONNECTIONS => self.render_connections(f, chunks[0]),
                shell::RULES => self.render_rules(f, chunks[0]),
                _ => self.render_profiles(f, chunks[0]),
            }
        }
        let detail = if let Some(prompt) = &self.prompt {
            if prompt.confirmation() {
                prompt.title().to_owned()
            } else {
                format!("{}\n> {}▏", prompt.title(), self.input)
            }
        } else if list_page {
            self.selection_status()
        } else {
            String::new()
        };
        f.render_widget(
            Paragraph::new(detail)
                .wrap(Wrap { trim: false })
                .style(if self.prompt.is_some() {
                    Style::default().fg(Color::Yellow)
                } else {
                    Style::default()
                }),
            chunks[1],
        );
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
                            .title_bottom(t("Enter confirm · Esc cancel · ↑↓ scroll")),
                    ),
                popup,
            );
        }
        if let Some(view) = &self.diagnostics {
            view.render(f, f.area());
        }
        if let Some(view) = &self.explanation {
            view.render(f, f.area());
        }
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
                    .highlight_style(Style::default().bg(Color::Blue))
                    .highlight_symbol("› "),
                popup,
                &mut state,
            );
            self.menu_hits(popup, state.offset());
        }
        self.render_palette(f);
        if let Some(scroll) = self.help_scroll {
            let text = crate::help::text(self.help_page());
            let popup = crate::help::popup(f.area(), &text);
            f.render_widget(Clear, popup);
            f.render_widget(
                Paragraph::new(text)
                    .wrap(Wrap { trim: false })
                    .scroll((scroll, 0))
                    .block(
                        Block::default()
                            .title(t("Page operations"))
                            .title_bottom(t("↑↓ scroll · ? / Esc close"))
                            .borders(Borders::ALL),
                    ),
                popup,
            );
        }
        self.render_background(f);
    }
}

pub(crate) fn action_description(action: &Action) -> String {
    let (label, name) = match action {
        Action::Logs(name) if name == "@tui" => ("TUI log", ""),
        Action::Logs(name) => ("Logs", name.as_str()),
        Action::Diagnostics(name) => ("Connectivity / Exit IP", name.as_str()),
        Action::Runtime { name, .. } => ("Applying runtime change", name.as_str()),
        Action::ExplainRoute { name, .. } => ("Route explanation", name.as_str()),
        Action::Create { name, .. } => ("Create", name.as_str()),
        Action::Add(instance) => ("Add", instance.name.as_str()),
        Action::Start(name) => ("Start", name.as_str()),
        Action::Stop(name) => ("Stop", name.as_str()),
        Action::Restart(name) => ("Restart", name.as_str()),
        Action::Reload(name) => ("Reload", name.as_str()),
        Action::Validate(name) => ("Validate", name.as_str()),
        Action::Read(name) => ("Edit", name.as_str()),
        Action::Save { name, .. } => ("Save", name.as_str()),
        Action::UpdateSubscriptions(name) => ("Update subscriptions", name.as_str()),
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
) -> Result<()> {
    tokio::task::LocalSet::new()
        .run_until(run_local(socket, context, launch))
        .await
}
async fn run_local(
    socket: PathBuf,
    context: crate::windows::Context,
    launch: crate::windows::Launch,
) -> Result<()> {
    let session_path =
        launch.session_path(socket.parent().context("Manager socket has no directory")?)?;
    let mut ui = match Session::load(&session_path) {
        Ok(session) => Ui::new(session),
        Err(error) => {
            let mut ui = Ui::new(Session::default());
            ui.fail(
                tr!("Cannot restore session: {}", format!("{error:#}")),
                Resume::Session,
            );
            ui
        }
    };
    if let Some(view) = launch.view {
        if let Some(page) = view.page() {
            ui.page = page;
        }
    }
    if let Some(kind) = launch.kind {
        ui.kind = kind;
    }
    let mut initial = Some(launch);
    let mut opening: Vec<tokio::task::JoinHandle<Result<()>>> = Vec::new();
    enable_raw_mode()?;
    let _screen = Screen;
    execute!(
        io::stdout(),
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture
    )?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    let mut jobs = background::Jobs::new(&socket);
    let mut feeds = background::Feeds::new(&socket);
    if let Some(name) = ui.active_log.clone() {
        jobs.start(&mut ui, Action::Logs(name), None);
    }
    let mut refreshed = Instant::now() - crate::UI_REFRESH_INTERVAL;
    let mut redraw = true;
    loop {
        tokio::task::yield_now().await;
        if refreshed.elapsed() >= crate::UI_REFRESH_INTERVAL {
            feeds.poll(&mut ui).await;
            if feeds.ready {
                if let Some(launch) = initial.take() {
                    if let Some(action) = ui.apply_launch(&launch) {
                        jobs.start(&mut ui, action, None);
                    }
                }
            }
            let mut index = 0;
            while index < opening.len() {
                if opening[index].is_finished() {
                    ui.message = match opening.remove(index).await {
                        Ok(Ok(())) => t("Opened in a new window").into(),
                        Ok(Err(error)) => format!("{}: {error:#}", t("Cannot open a new window")),
                        Err(error) => error.to_string(),
                    };
                    ui.tasks.notice_message(&ui.message);
                } else {
                    index += 1;
                }
            }
            let quit = jobs.poll(&mut ui).await;
            if jobs.take_changed() || quit {
                let remembered = ui.checkpoint(&session_path);
                if quit && remembered {
                    break;
                }
            }
            if let Some(view) = &mut ui.diagnostics {
                view.poll();
            }
            if let Some(viewer) = &mut ui.viewer {
                viewer.poll();
            }
            if let Some(viewer) = &mut ui.tui_viewer {
                viewer.poll();
            }
            // Start the next period after processing; do not catch up missed ticks.
            refreshed = Instant::now();
            redraw = true;
        }
        if redraw {
            ui.draw(&mut terminal)?;
            redraw = false;
        }
        if !event::poll(Duration::from_millis(35))? {
            continue;
        }
        redraw = true;
        let input = match event::read()? {
            Event::Paste(text) if ui.recovery.is_none() => {
                ui.paste(&text);
                Input::Idle
            }
            Event::Key(key) if key.kind != KeyEventKind::Release => ui.key(key),
            Event::Mouse(mouse) => ui.mouse(mouse),
            _ => Input::Idle,
        };
        let quit = match input {
            Input::Quit => true,
            Input::Run(action, origin) => {
                jobs.start(&mut ui, action, origin);
                false
            }
            Input::Idle => false,
            Input::OpenWindow(launch) => {
                if opening.len() < 4 {
                    let context = context.clone();
                    opening.push(tokio::task::spawn_blocking(move || context.open(&launch)));
                }
                false
            }
            Input::ExternalEditor => {
                let result = (|| -> Result<()> {
                    let command = crate::external_editor::command()?;
                    let (draft, directory) = ui.prepare_external(
                        socket.parent().context("Manager socket has no directory")?,
                    )?;
                    anyhow::ensure!(
                        ui.checkpoint(&session_path),
                        t("Cannot open editor until the draft is checkpointed")
                    );
                    let launched = (|| -> Result<()> {
                        disable_raw_mode()?;
                        execute!(
                            io::stdout(),
                            DisableBracketedPaste,
                            DisableMouseCapture,
                            crossterm::cursor::Show,
                            LeaveAlternateScreen
                        )?;
                        crate::external_editor::launch(&command, &draft, &directory)
                    })();
                    // Always restore the TUI, including when the executable cannot start.
                    enable_raw_mode()?;
                    execute!(
                        io::stdout(),
                        EnterAlternateScreen,
                        EnableBracketedPaste,
                        EnableMouseCapture
                    )?;
                    terminal.clear()?;
                    launched?;
                    ui.read_external()
                })();
                if let Err(error) = result {
                    ui.fail(format!("{error:#}"), Resume::Editor);
                }
                false
            }
        };
        let remembered = ui.checkpoint(&session_path);
        // A failed checkpoint is recoverable too: show c/b instead of dropping the draft.
        if quit && remembered {
            break;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn key(c: char) -> event::KeyEvent {
        event::KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }
    fn sample_ui(running: bool) -> Ui {
        let mut ui = Ui::new(Session::default());
        // Instance-level shortcuts live on the Profiles page.
        ui.page = PROFILES;
        ui.rows = vec![
            Row {
                instance: Instance {
                    name: "proxy".into(),
                    kind: Kind::Proxy,
                    config: "/configs/proxy.toml".into(),
                },
                pid: running.then_some(42),
                state: if running { "Running" } else { "Stopped" }.into(),
                uptime: 610,
                changed: false,
            },
            Row {
                instance: Instance {
                    name: "rules".into(),
                    kind: Kind::Rule,
                    config: "/configs/rules.toml".into(),
                },
                pid: None,
                state: "Stopped".into(),
                uptime: 0,
                changed: false,
            },
        ];
        ui.details_supported = true;
        ui.details.insert(
            "proxy".into(),
            InstanceDetails {
                modes: vec!["Rules".into(), "TUN".into()],
                listeners: vec!["SOCKS5 127.0.0.1:1080".into()],
                bindings: vec!["rules".into()],
                outbounds: vec!["edge".into()],
                log_level: LogLevel::Route,
                ..InstanceDetails::default()
            },
        );
        ui
    }
    fn press(ui: &mut Ui, code: KeyCode) -> Input {
        ui.key(event::KeyEvent::new(code, KeyModifiers::NONE))
    }
    fn import_ui() -> Ui {
        let mut ui = sample_ui(false);
        ui.editor =
            Some(Editor::new("proxy".into(), "standalone=true\n".into()).with_kind(Kind::Proxy));
        ui
    }
    #[test]
    fn operation_status_defaults_to_latest_line() {
        let mut ui = sample_ui(true);
        ui.operation_steps = vec!["first".into(), "latest".into()];
        assert_eq!(ui.operation_text(), "latest");
        assert_eq!(ui.operation_text().lines().count(), 1);
        ui.message = "new result".into();
        ui.operation_result = Some("old result".into());
        assert_eq!(ui.operation_text(), "new result");
    }
    #[test]
    fn help_overlay_preserves_state_and_does_not_capture_question_marks_in_inputs() {
        let mut ui = sample_ui(true);
        let selected = ui.selections;
        assert!(matches!(ui.key(key('?')), Input::Idle));
        assert_eq!(ui.help_scroll, Some(0));
        assert!(matches!(ui.key(key('j')), Input::Idle));
        assert_eq!(ui.help_scroll, Some(1));
        assert_eq!(ui.selections, selected);
        ui.key(key('?'));
        assert!(ui.help_scroll.is_none());
        ui.prompt(Prompt::AddName);
        ui.key(key('?'));
        assert!(ui.help_scroll.is_none());
        assert_eq!(ui.input, "?");
        ui.prompt = None;
        ui.key(key('?'));
        use ratatui::{backend::TestBackend, Terminal};
        for language in [
            crate::i18n::Language::English,
            crate::i18n::Language::Simplified,
            crate::i18n::Language::Traditional,
        ] {
            crate::i18n::with_language(language, || {
                for (width, height) in [(100, 30), (36, 12), (12, 7)] {
                    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                    terminal.draw(|frame| ui.render(frame)).unwrap();
                }
            });
        }
        ui.key(event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert!(ui.help_scroll.is_none());
    }
    #[test]
    fn vim_list_navigation_and_log_shortcut_leave_prompt_text_intact() {
        let mut ui = sample_ui(false);
        let mut second = ui.rows[0].clone();
        second.instance.name = "second".into();
        ui.rows.insert(1, second);
        ui.key(key('j'));
        assert_eq!(ui.selections[0], 1);
        ui.key(key('k'));
        assert_eq!(ui.selections[0], 0);
        ui.key(key('G'));
        assert_eq!(ui.selections[0], 1);
        ui.key(key('g'));
        ui.key(key('g'));
        assert_eq!(ui.selections[0], 0);
        press(&mut ui, KeyCode::Enter);
        ui.key(key('j'));
        assert_eq!(ui.menu, Some(1));
        ui.key(key('h'));
        assert!(ui.menu.is_none());
        ui.key(key('l'));
        assert_eq!(ui.kind, Kind::Rule);
        assert!(matches!(ui.key(key('L')),Input::Run(Action::Logs(name),_) if name=="rules"));
        ui.prompt(Prompt::AddName);
        for ch in "hjklgg".chars() {
            ui.key(key(ch));
        }
        assert_eq!(ui.input, "hjklgg");
    }
    #[test]
    fn diagnostics_are_only_offered_for_proxy_instances_and_supported_managers() {
        let mut ui = sample_ui(false);
        ui.details.get_mut("proxy").unwrap().diagnostics_supported = true;
        assert!(ui
            .operations()
            .iter()
            .any(|op| matches!(op, Operation::Diagnostics)));
        assert!(
            matches!(ui.key(key('t')), Input::Run(Action::Diagnostics(name), None) if name == "proxy")
        );
        ui.details.get_mut("proxy").unwrap().diagnostics_supported = false;
        assert!(matches!(ui.key(key('t')), Input::Idle));
        assert_eq!(ui.message, "Restart manager to enable diagnostics");
        ui.kind = Kind::Rule;
        assert!(!ui
            .operations()
            .iter()
            .any(|op| matches!(op, Operation::Diagnostics)));
        assert!(matches!(ui.key(key('t')), Input::Idle));
    }
    #[tokio::test]
    #[ignore = "requires freshly built core binaries"]
    async fn live_rule_query_keeps_draft_and_explains_rule_set_toggle() {
        let fixture = crate::import::tests::Fixture::new();
        let home = fixture.0.join("query-manager");
        let socket = home.join("manager.sock");
        let bin = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let manager = tokio::spawn(crate::manager::serve(home, bin));
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let text = format!("socket={:?}\ndefault_action='direct'\n[[rules]]\nkind='rule_set'\nvalue='sites'\naction='block'\n[rule_sets.sites]\nbehavior='domain'\npayload=['+.example.test']\n", fixture.0.join("query-route.sock").to_string_lossy());
        let path = fixture.write("query-rules.toml", &text);
        call(
            &socket,
            Action::Add(Instance {
                name: "rules".into(),
                kind: Kind::Rule,
                config: path,
            }),
        )
        .await
        .unwrap();
        call(&socket, Action::Start("rules".into())).await.unwrap();
        let mut ui = Ui::new(Session::default());
        ui.kind = Kind::Rule;
        ui.page = PROFILES;
        ui.execute(&socket, Action::List, None).await;
        ui.key(key('t'));
        assert!(matches!(ui.prompt, Some(Prompt::Explain(_))));
        ui.paste("a.example.test:443 tcp");
        let Input::Run(action, origin) = press(&mut ui, KeyCode::Enter) else {
            panic!("query action");
        };
        ui.execute(&socket, action, origin).await;
        let report = &ui.explanation.as_ref().unwrap().report;
        assert_eq!(report.action, astra_ipc::RouteAction::Block);
        assert!(report.path.starts_with("rules[1] -> rule_set(\"sites\")"));
        assert_eq!(
            report.condition,
            Some(("domain_suffix".into(), "example.test".into()))
        );
        let previous_revision = report.revision;
        press(&mut ui, KeyCode::Esc);
        let draft = text.replace("default_action='direct'", "default_action='proxy:unsaved'");
        ui.editor = Some(Editor::new("rules".into(), draft.clone()).with_kind(Kind::Rule));
        ui.key(event::KeyEvent::new(
            KeyCode::Char('t'),
            KeyModifiers::CONTROL,
        ));
        ui.paste("unmatched.test");
        let Input::Run(action, origin) = press(&mut ui, KeyCode::Enter) else {
            panic!("draft query");
        };
        ui.execute(&socket, action, origin).await;
        assert_eq!(
            ui.explanation.as_ref().unwrap().report.default_action,
            "direct"
        );
        assert_eq!(ui.editor.as_ref().unwrap().text(), draft);
        press(&mut ui, KeyCode::Esc);
        call(
            &socket,
            Action::Save {
                name: "rules".into(),
                original: text.clone(),
                text: text.replace("behavior='domain'", "enabled=false\nbehavior='domain'"),
            },
        )
        .await
        .unwrap();
        let reply = call(
            &socket,
            Action::ExplainRoute {
                name: "rules".into(),
                request: crate::explanation::request("a.example.test").unwrap(),
            },
        )
        .await
        .unwrap();
        let report: astra_ipc::control::RouteExplanation =
            serde_json::from_str(&reply.text).unwrap();
        assert_eq!(report.action, astra_ipc::RouteAction::Direct);
        assert_eq!(report.path, "default_action");
        assert!(report.revision > previous_revision);
        call(&socket, Action::Shutdown).await.unwrap();
        manager.await.unwrap().unwrap();
    }
    #[tokio::test]
    #[ignore = "requires freshly built core binaries"]
    async fn live_rule_editor_completes_a_bound_proxy_node_and_saves_valid_action() {
        let fixture = crate::import::tests::Fixture::new();
        let home = fixture.0.join("manager");
        let socket = home.join("manager.sock");
        let bin = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let manager = tokio::spawn(crate::manager::serve(home, bin));
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        call(
            &socket,
            Action::Create {
                name: "rules".into(),
                kind: Kind::Rule,
            },
        )
        .await
        .unwrap();
        let path = fixture.write("proxy.toml", "standalone=true\nsocks5_listen='127.0.0.1:1080'\n[[outbounds]]\nkind='socks5'\ntag='east-node'\nserver='127.0.0.1:1081'\nusername='test-user'\npassword='do-not-show'\n");
        call(
            &socket,
            Action::Add(Instance {
                name: "proxy".into(),
                kind: Kind::Proxy,
                config: path,
            }),
        )
        .await
        .unwrap();
        call(
            &socket,
            Action::BindStart {
                name: "proxy".into(),
                targets: vec!["rules".into()],
            },
        )
        .await
        .unwrap();
        let mut ui = Ui::new(Session::default());
        ui.kind = Kind::Rule;
        ui.execute(&socket, Action::List, None).await;
        ui.execute(&socket, Action::Read("rules".into()), None)
            .await;
        assert!(ui.recovery.is_none());
        let editor = ui.editor.as_mut().unwrap();
        editor
            .key(event::KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
            .unwrap();
        editor
            .key(event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        editor.paste("eas");
        editor
            .key(event::KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            .unwrap();
        editor
            .key(event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        let text = editor.text();
        assert!(!text.contains("do-not-show"));
        assert_eq!(
            toml::from_str::<toml::Value>(&text).unwrap()["default_action"].as_str(),
            Some("proxy:east-node")
        );
        let Input::Run(save, _) = ui.key(event::KeyEvent::new(
            KeyCode::Char('s'),
            KeyModifiers::CONTROL,
        )) else {
            panic!("save");
        };
        ui.execute(&socket, save, None).await;
        assert!(ui.recovery.is_none());
        let saved = call(&socket, Action::Read("rules".into()))
            .await
            .unwrap()
            .text;
        assert_eq!(
            toml::from_str::<toml::Value>(&saved).unwrap()["default_action"].as_str(),
            Some("proxy:east-node")
        );
        call(&socket, Action::Shutdown).await.unwrap();
        manager.await.unwrap().unwrap();
    }
    #[test]
    fn dropped_config_and_import_shortcut_replace_only_draft_then_use_normal_save() {
        let fixture = crate::import::tests::Fixture::new();
        let path = fixture.write(
            "中文 file.toml",
            "standalone=true\nsocks5_listen='127.0.0.1:1234'\n",
        );
        for shortcut in [false, true] {
            let mut ui = import_ui();
            if shortcut {
                ui.key(event::KeyEvent::new(
                    KeyCode::Char('o'),
                    KeyModifiers::CONTROL,
                ));
                assert!(matches!(ui.prompt, Some(Prompt::Import)));
                ui.paste(&format!("\"{}\"", path.display()));
                press(&mut ui, KeyCode::Enter);
            } else {
                ui.paste(&format!("\"{}\"", path.display()));
            }
            assert!(ui.prompt.is_none());
            assert!(ui.editor.as_ref().unwrap().text().contains("1234"));
            assert_eq!(ui.editor.as_ref().unwrap().original, "standalone=true\n");
            assert_eq!(
                std::fs::read_to_string(&path).unwrap(),
                "standalone=true\nsocks5_listen='127.0.0.1:1234'\n"
            );
            assert!(
                matches!(ui.key(event::KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)), Input::Run(Action::Save {name, original, text}, None) if name == "proxy" && original == "standalone=true\n" && text.contains("1234"))
            );
        }
    }
    #[test]
    fn import_failure_cancel_and_confirmation_preserve_draft_and_survive_restore() {
        let fixture = crate::import::tests::Fixture::new();
        let path = fixture.write(
            "new.toml",
            "standalone=true\nhttp_listen='127.0.0.1:9876'\n",
        );
        let mut ui = import_ui();
        ui.editor
            .as_mut()
            .unwrap()
            .import("standalone=true\n# my draft\n".into())
            .unwrap();
        let old = ui.editor.as_ref().unwrap().text();
        ui.paste("missing-file.toml");
        assert!(ui.recovery.is_some());
        assert_eq!(ui.editor.as_ref().unwrap().text(), old);
        press(&mut ui, KeyCode::Char('c'));
        assert!(matches!(ui.prompt, Some(Prompt::Import)));
        press(&mut ui, KeyCode::Esc);
        ui.paste(path.to_str().unwrap());
        assert!(matches!(ui.prompt, Some(Prompt::ImportConfirm { .. })));
        ui.paste("ignored.toml");
        assert_eq!(ui.editor.as_ref().unwrap().text(), old);
        press(&mut ui, KeyCode::Esc);
        assert_eq!(ui.editor.as_ref().unwrap().text(), old);
        ui.paste(path.to_str().unwrap());
        let session_path = fixture.0.join("ui-session.json");
        ui.session.save(&session_path).unwrap();
        let mut restored = Ui::new(Session::load(&session_path).unwrap());
        assert!(matches!(
            restored.prompt,
            Some(Prompt::ImportConfirm { .. })
        ));
        press(&mut restored, KeyCode::Enter);
        assert!(restored.prompt.is_none());
        assert!(restored.editor.as_ref().unwrap().text().contains("9876"));
        assert_eq!(
            restored.editor.as_ref().unwrap().original,
            "standalone=true\n"
        );
    }
    #[test]
    fn pasting_inside_a_field_keeps_field_editing_and_ctrl_o_does_not_discard_input() {
        let mut ui = import_ui();
        // The first row is a boolean that toggles in place; open the next, text-like field.
        press(&mut ui, KeyCode::Down);
        press(&mut ui, KeyCode::Enter);
        assert!(!ui.editor.as_ref().unwrap().browsing());
        ui.key(event::KeyEvent::new(
            KeyCode::Char('o'),
            KeyModifiers::CONTROL,
        ));
        assert!(ui.prompt.is_none());
        ui.paste("true");
        assert!(ui.recovery.is_none());
        assert!(!ui.editor.as_ref().unwrap().browsing());
    }
    #[test]
    fn yaml_import_review_renders_in_three_languages_and_small_windows() {
        use ratatui::backend::TestBackend;
        let fixture = crate::import::tests::Fixture::new();
        let path = fixture.write("source.yaml", "socks-port: 1088\nrules: ['MATCH,DIRECT']\n");
        let mut ui = import_ui();
        ui.paste(path.to_str().unwrap());
        assert!(matches!(ui.prompt, Some(Prompt::ImportConfirm { .. })));
        for lang in [
            crate::i18n::Language::English,
            crate::i18n::Language::Simplified,
            crate::i18n::Language::Traditional,
        ] {
            crate::i18n::with_language(lang, || {
                for (width, height) in [(110, 32), (60, 16), (20, 8)] {
                    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                    terminal.draw(|frame| ui.render(frame)).unwrap();
                }
            });
        }
        press(&mut ui, KeyCode::Enter);
        assert_eq!(ui.editor.as_ref().unwrap().original, "standalone=true\n");
        assert!(ui.editor.as_ref().unwrap().text().contains("1088"));
    }
    #[tokio::test]
    #[ignore = "requires freshly built core binaries"]
    async fn live_import_saves_each_core_without_changing_source_or_binding() {
        let fixture = crate::import::tests::Fixture::new();
        let home = fixture.0.join("manager");
        let socket = home.join("manager.sock");
        let bin = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let manager = tokio::spawn(crate::manager::serve(home, bin));
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let source = "socks-port: 11088\nrules: ['MATCH,DIRECT']\n";
        let path = fixture.write("导入 file.yaml", source);
        let mut ui = Ui::new(Session::default());
        for (name, kind) in [("rules", Kind::Rule), ("proxy", Kind::Proxy)] {
            ui.kind = kind;
            ui.execute(
                &socket,
                Action::Create {
                    name: name.into(),
                    kind,
                },
                None,
            )
            .await;
            assert!(ui.recovery.is_none());
            let original = ui.editor.as_ref().unwrap().original.clone();
            ui.execute(&socket, Action::List, None).await;
            let bound = ui
                .rows
                .iter()
                .find(|r| r.instance.name == name)
                .unwrap()
                .instance
                .config
                .clone();
            ui.paste(path.to_str().unwrap());
            assert!(matches!(ui.prompt, Some(Prompt::ImportConfirm { .. })));
            press(&mut ui, KeyCode::Enter);
            assert_eq!(std::fs::read_to_string(&bound).unwrap(), original);
            let Input::Run(action, _) = ui.key(event::KeyEvent::new(
                KeyCode::Char('s'),
                KeyModifiers::CONTROL,
            )) else {
                panic!("save");
            };
            ui.execute(&socket, action, None).await;
            assert!(ui.recovery.is_none());
            let saved = std::fs::read_to_string(&bound).unwrap();
            assert_ne!(saved, original);
            ui.execute(&socket, Action::List, None).await;
            assert_eq!(
                ui.rows
                    .iter()
                    .find(|r| r.instance.name == name)
                    .unwrap()
                    .instance
                    .config,
                bound
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), source);
        }
        call(&socket, Action::Shutdown).await.unwrap();
        manager.await.unwrap().unwrap();
    }
    #[test]
    fn context_menu_only_offers_applicable_actions_and_keeps_shortcuts() {
        let mut ui = sample_ui(true);
        assert!(!ui
            .operations()
            .iter()
            .any(|operation| matches!(operation, Operation::Start | Operation::Remove)));
        press(&mut ui, KeyCode::Enter);
        assert_eq!(ui.menu, Some(0));
        for _ in 0..3 {
            press(&mut ui, KeyCode::Down);
        }
        assert!(
            matches!(press(&mut ui, KeyCode::Enter), Input::Run(Action::Read(name), None) if name == "proxy")
        );
        assert!(matches!(ui.key(key('d')), Input::Idle));
        assert!(ui.prompt.is_none());
        let mut ui = sample_ui(false);
        assert!(ui
            .operations()
            .iter()
            .any(|operation| matches!(operation, Operation::UpdateSubscriptions)));
        assert!(!ui.operations().iter().any(|operation| matches!(
            operation,
            Operation::Stop | Operation::Restart | Operation::Reload
        )));
        // r applies (hot reload) and needs a running core; R restarts or starts.
        assert!(matches!(ui.key(key('r')), Input::Idle));
        assert!(
            matches!(ui.key(key('R')), Input::Run(Action::Start(name), None) if name == "proxy")
        );
        assert!(
            matches!(ui.key(key('c')), Input::Run(Action::Start(name), None) if name == "proxy")
        );
    }
    #[test]
    fn bindings_are_selected_without_typing_and_cancel_does_not_save() {
        let mut ui = sample_ui(false);
        ui.key(key('b'));
        assert_eq!(ui.binding_picker.as_ref().unwrap().targets, ["rules"]);
        press(&mut ui, KeyCode::Char(' '));
        assert!(ui.binding_picker.as_ref().unwrap().targets.is_empty());
        assert!(
            matches!(press(&mut ui, KeyCode::Enter), Input::Run(Action::BindStart { name, targets }, None) if name == "proxy" && targets.is_empty())
        );
        ui.key(key('b'));
        press(&mut ui, KeyCode::Char(' '));
        press(&mut ui, KeyCode::Esc);
        assert!(ui.binding_picker.is_none());
        assert_eq!(ui.details["proxy"].bindings, ["rules"]);
    }
    #[test]
    fn binding_picker_selects_one_rule_or_multiple_proxies() {
        let mut ui = sample_ui(false);
        let mut other_proxy = ui.rows[0].clone();
        other_proxy.instance.name = "other-proxy".into();
        ui.rows.push(other_proxy);
        let mut other_rule = ui
            .rows
            .iter()
            .find(|row| row.instance.kind == Kind::Rule)
            .unwrap()
            .clone();
        other_rule.instance.name = "other-rule".into();
        ui.rows.push(other_rule);
        ui.key(key('b'));
        assert_eq!(ui.binding_picker.as_ref().unwrap().choices.len(), 2);
        assert!(ui
            .binding_picker
            .as_ref()
            .unwrap()
            .choices
            .iter()
            .all(|(_, kind)| *kind == Kind::Rule));
        press(&mut ui, KeyCode::Down);
        press(&mut ui, KeyCode::Char(' '));
        assert_eq!(ui.binding_picker.as_ref().unwrap().targets, ["other-rule"]);
        press(&mut ui, KeyCode::Esc);
        ui.kind = Kind::Rule;
        assert!(ui
            .operations()
            .iter()
            .any(|operation| matches!(operation, Operation::Bind)));
        ui.key(key('b'));
        assert!(ui
            .binding_picker
            .as_ref()
            .unwrap()
            .choices
            .iter()
            .all(|(_, kind)| *kind == Kind::Proxy));
        press(&mut ui, KeyCode::Char(' '));
        press(&mut ui, KeyCode::Down);
        press(&mut ui, KeyCode::Char(' '));
        assert_eq!(
            ui.binding_picker.as_ref().unwrap().targets,
            ["proxy", "other-proxy"]
        );
    }
    #[test]
    fn confirmations_use_enter_and_escape_without_a_typed_phrase() {
        let mut ui = sample_ui(false);
        ui.key(key('d'));
        assert!(
            matches!(press(&mut ui, KeyCode::Enter), Input::Run(Action::Remove(name), _) if name == "proxy")
        );
        ui.key(key('Q'));
        press(&mut ui, KeyCode::Esc);
        assert!(ui.prompt.is_none());
        ui.key(key('Q'));
        assert!(matches!(ui.key(key('y')), Input::Run(Action::Shutdown, _)));
    }
    #[test]
    fn configuration_paths_are_prefilled_and_paste_replaces_the_default() {
        let mut ui = sample_ui(false);
        // The path dialog moved to the action menu and command palette; c connects.
        ui.operation(Operation::Config);
        assert!(ui.input.ends_with("proxy.toml"));
        ui.paste_prompt("new path.toml");
        assert_eq!(ui.input, "new path.toml");
        press(&mut ui, KeyCode::Esc);
        ui.key(key('y'));
        assert!(ui.input.ends_with("proxy-copy.toml"));
        ui.key(event::KeyEvent::new(
            KeyCode::Char('u'),
            KeyModifiers::CONTROL,
        ));
        assert!(ui.input.is_empty());
    }
    #[test]
    fn instance_overview_and_overlays_render_in_all_languages_and_small_windows() {
        use crate::i18n::{with_language, Language};
        use ratatui::backend::TestBackend;
        for language in [
            Language::English,
            Language::Simplified,
            Language::Traditional,
        ] {
            with_language(language, || {
                let mut ui = sample_ui(true);
                let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
                terminal.draw(|frame| ui.render(frame)).unwrap();
                let text: String = terminal
                    .backend()
                    .buffer()
                    .content
                    .iter()
                    .map(|cell| cell.symbol())
                    .collect();
                for value in [
                    "proxy",
                    "TUN",
                    "127.0.0.1:1080",
                    "rules",
                    "PID 42",
                    "00:10:10",
                    "ROUTE",
                    "edge",
                ] {
                    assert!(text.contains(value), "Missing {value}: {text}");
                }
                ui.key(key('b'));
                for (width, height) in [(100, 24), (20, 8), (8, 4)] {
                    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
                    terminal.draw(|frame| ui.render(frame)).unwrap();
                }
                press(&mut ui, KeyCode::Esc);
                press(&mut ui, KeyCode::Enter);
                terminal.draw(|frame| ui.render(frame)).unwrap();
            });
        }
    }
    #[test]
    fn older_manager_rows_work_without_sending_an_unsupported_binding_action() {
        let mut ui = sample_ui(false);
        let rows = ui.rows.clone();
        let mut reply = Reply::text("");
        reply.rows = rows;
        ui.update_rows(reply);
        assert!(!ui.details_supported);
        assert!(matches!(ui.key(key('b')), Input::Idle));
        assert!(ui.binding_picker.is_none());
        assert!(!ui.message.is_empty());
    }
    #[test]
    fn new_instance_name_creates_bound_config_without_a_second_path_prompt() {
        let mut ui = Ui::new(Session::default());
        ui.kind = Kind::Rule;
        ui.prompt(Prompt::AddName);
        ui.input = "rules".into();
        let Input::Run(Action::Create { name, kind }, Some((Prompt::AddName, input))) =
            ui.key(event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        else {
            panic!("create action")
        };
        assert_eq!(name, "rules");
        assert_eq!(input, "rules");
        assert_eq!(kind, Kind::Rule);
        assert!(ui.prompt.is_none());
    }
    #[test]
    fn retry_restores_path_step_name_input_and_core() {
        let mut ui = Ui::new(Session::default());
        ui.kind = Kind::Rule;
        ui.fail(
            "permission denied",
            Resume::Prompt(
                Prompt::AddPath("rules".into()),
                "/private/rules.toml".into(),
            ),
        );
        assert!(matches!(ui.key(key('c')), Input::Idle));
        assert!(matches!(&ui.prompt, Some(Prompt::AddPath(name)) if name == "rules"));
        assert_eq!(ui.input, "/private/rules.toml");
        assert_eq!(ui.kind, Kind::Rule);
        assert!(ui.recovery.is_none());
    }
    #[test]
    fn retry_save_preserves_form_draft_and_b_ends_only_operation() {
        let mut ui = Ui::new(Session::default());
        let mut editor = Editor::new(
            "rules".into(),
            r#"socket = "/tmp/original.sock"
default_action = "direct"
"#
            .into(),
        );
        editor
            .key(event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        editor.paste("/tmp/new.sock");
        editor
            .key(event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        let draft = editor.text();
        let snapshot = serde_json::to_value(&editor).unwrap();
        let action = Action::Save {
            name: "rules".into(),
            original: editor.original.clone(),
            text: draft.clone(),
        };
        ui.editor = Some(editor);
        ui.fail("read only", Resume::Request(action.clone()));
        ui.key(key('c'));
        assert_eq!(
            serde_json::to_value(ui.editor.as_ref().unwrap()).unwrap(),
            snapshot
        );
        ui.fail("still read only", Resume::Request(action));
        ui.key(key('b'));
        assert!(ui.recovery.is_none());
        assert_eq!(ui.editor.as_ref().unwrap().text(), draft);
    }
    #[test]
    fn invalid_entry_uses_c_to_resume_and_b_to_cancel_the_field_only() {
        let mut ui = Ui::new(Session::default());
        let mut editor = Editor::new(
            "proxy".into(),
            include_str!("../../../configs/proxy.toml").into(),
        );
        // Select IPC connections after standalone settings, socket, listeners and authentication.
        for _ in 0..7 {
            editor
                .key(event::KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
                .unwrap();
        }
        editor
            .key(event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        editor.paste("0");
        let snapshot = serde_json::to_value(&editor).unwrap();
        let original = editor.text();
        ui.editor = Some(editor);
        ui.key(event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(matches!(
            ui.recovery.as_ref().unwrap().resume,
            Resume::Editor
        ));
        ui.key(key('c'));
        assert_eq!(
            serde_json::to_value(ui.editor.as_ref().unwrap()).unwrap(),
            snapshot
        );
        ui.key(event::KeyEvent::new(
            KeyCode::Char('s'),
            KeyModifiers::CONTROL,
        ));
        assert!(ui.recovery.is_some());
        ui.key(key('b'));
        assert!(ui.recovery.is_none());
        assert_eq!(ui.editor.as_ref().unwrap().text(), original);
        // A subsequent Enter reopens the unchanged field with its original value.
        ui.key(event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        ui.editor.as_mut().unwrap().paste("16");
        ui.key(event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(ui
            .editor
            .as_ref()
            .unwrap()
            .text()
            .contains("ipc_connections = 16"));
    }
    #[test]
    fn external_editor_roundtrip_keeps_original_and_recovers_invalid_or_conflicting_edits() {
        let home = std::env::temp_dir().join(format!("astra-external-ui-{}", std::process::id()));
        crate::manager::private_dir(&home).unwrap();
        let mut ui = sample_ui(false);
        let original = "standalone=true\nsocks5_listen='127.0.0.1:1080'\n";
        ui.editor = Some(Editor::new("proxy".into(), original.into()));
        assert!(matches!(
            press(&mut ui, KeyCode::F(4)),
            Input::ExternalEditor
        ));
        let (draft, _) = ui.prepare_external(&home).unwrap();
        let changed = original.replace("1080", "1088");
        std::fs::write(&draft.path, &changed).unwrap();
        let session = serde_json::to_string(&ui.session).unwrap();
        let mut ui = Ui::new(serde_json::from_str(&session).unwrap());
        press(&mut ui, KeyCode::F(5));
        assert_eq!(ui.editor.as_ref().unwrap().text(), changed);
        assert_eq!(ui.editor.as_ref().unwrap().original, original);
        assert!(
            matches!(ui.key(event::KeyEvent::new(KeyCode::Char('s'),KeyModifiers::CONTROL)),Input::Run(Action::Save{original:base,text,..},_) if base==original && text==changed)
        );
        std::fs::write(&draft.path, "bad = [").unwrap();
        assert!(ui.read_external().is_err());
        assert_eq!(ui.editor.as_ref().unwrap().text(), changed);
        // Local edits made after the last external import cannot be silently replaced.
        ui.editor
            .as_mut()
            .unwrap()
            .import(changed.replace("1088", "1099"))
            .unwrap();
        std::fs::write(&draft.path, original).unwrap();
        assert!(ui.read_external().is_err());
        assert!(ui.editor.as_ref().unwrap().text().contains("1099"));
        assert!(ui.prepare_external(&home).is_err());
        assert_eq!(ui.external_draft.as_ref().unwrap().path, draft.path);

        std::fs::remove_file(draft.path).unwrap();
        std::fs::remove_dir(home.join("external-drafts")).unwrap();
        std::fs::remove_dir(home).unwrap();
    }
    #[test]
    fn occupied_start_retries_same_action_and_cancel_does_not_execute() {
        let mut ui = Ui::new(Session::default());
        ui.fail(
            "address in use",
            Resume::Request(Action::Start("proxy".into())),
        );
        assert!(
            matches!(ui.key(key('c')), Input::Run(Action::Start(name), None) if name == "proxy")
        );
        ui.fail(
            "address in use",
            Resume::Request(Action::Start("proxy".into())),
        );
        assert!(matches!(ui.key(key('b')), Input::Idle));
        assert!(ui.recovery.is_none());
    }
    #[test]
    fn bad_local_input_is_recoverable() {
        let mut ui = Ui::new(Session::default());
        ui.prompt(Prompt::Config("proxy".into()));
        ui.key(event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert!(ui.recovery.is_some());
        ui.key(key('c'));
        assert!(matches!(&ui.prompt, Some(Prompt::Config(name)) if name == "proxy"));
    }
    #[test]
    fn session_roundtrip_retains_draft_recovery_and_selection() {
        let mut session = Session {
            kind: Kind::Rule,
            selections: [2, 3],
            editor: Some(Editor::new(
                "rules".into(),
                r#"socket = "/tmp/a.sock"
"#
                .into(),
            )),
            prompt: Some(Prompt::AddPath("other".into())),
            input: "/tmp/a b.toml".into(),
            recovery: Some(Recovery {
                message: "permission denied".into(),
                resume: Resume::Request(Action::Read("rules".into())),
            }),
            working_dir: launch_directory(),
            log_settings: HashMap::new(),
            active_log: None,
            external_draft: None,
        };
        session
            .editor
            .as_mut()
            .unwrap()
            .key(event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        session.editor.as_mut().unwrap().paste("/tmp/pending.sock");
        let form = serde_json::to_value(session.editor.as_ref().unwrap()).unwrap();
        let restored: Session =
            serde_json::from_slice(&serde_json::to_vec(&session).unwrap()).unwrap();
        assert_eq!(restored.kind, Kind::Rule);
        assert_eq!(restored.selections, [2, 3]);
        assert_eq!(restored.input, "/tmp/a b.toml");
        assert_eq!(
            serde_json::to_value(restored.editor.unwrap()).unwrap(),
            form
        );
        assert!(restored.recovery.is_some());
    }
    #[tokio::test]
    #[ignore = "run after cargo build --workspace"]
    async fn live_recovery_and_defaults() {
        #[cfg(unix)]
        use std::os::unix::fs::PermissionsExt;
        let directory =
            std::env::temp_dir().join(format!("astra-ui-recover-{}", std::process::id()));
        std::fs::create_dir_all(&directory).unwrap();
        let home = directory.join("state");
        let socket = home.join("manager.sock");
        let bin = std::env::current_exe()
            .unwrap()
            .parent()
            .unwrap()
            .parent()
            .unwrap()
            .to_path_buf();
        let manager = tokio::spawn(crate::manager::serve(home.clone(), bin));
        for _ in 0..100 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let mut ui = Ui::new(Session::default());
        ui.kind = Kind::Rule;
        let path = directory.join("new/rule.toml");
        let instance = Instance {
            name: "rules".into(),
            kind: Kind::Rule,
            config: path.clone(),
        };
        ui.execute(
            &socket,
            Action::Add(instance),
            Some((Prompt::AddPath("rules".into()), path.display().to_string())),
        )
        .await;
        assert!(path.is_file());
        assert!(ui.recovery.is_none());
        assert_eq!(
            ui.editor.as_ref().unwrap().text(),
            std::fs::read_to_string(&path).unwrap()
        );
        // An invalid route action is rejected by the core; all form state survives c.
        let editor = ui.editor.as_mut().unwrap();
        editor
            .key(event::KeyEvent::new(KeyCode::Down, KeyModifiers::NONE))
            .unwrap();
        editor
            .key(event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        editor.paste("invalid-route");
        editor
            .key(event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        let snapshot = serde_json::to_value(ui.editor.as_ref().unwrap()).unwrap();
        let Input::Run(save, _) = ui.key(event::KeyEvent::new(
            KeyCode::Char('s'),
            KeyModifiers::CONTROL,
        )) else {
            panic!("save action");
        };
        ui.execute(&socket, save, None).await;
        assert!(ui.recovery.is_some());
        ui.key(key('c'));
        assert_eq!(
            serde_json::to_value(ui.editor.as_ref().unwrap()).unwrap(),
            snapshot
        );
        assert!(!std::fs::read_to_string(&path)
            .unwrap()
            .contains("invalid-route"));
        ui.editor = None;
        // A removed config during start is recreated and opened, never started silently.
        std::fs::remove_file(&path).unwrap();
        ui.execute(&socket, Action::Start("rules".into()), None)
            .await;
        assert!(ui.editor.is_some());
        assert!(ui.recovery.is_none());
        assert!(call(&socket, Action::List).await.unwrap().rows[0]
            .pid
            .is_none());
        // Path occupied by a directory: c restores the path prompt and instance name.
        let blocked = directory.join("occupied.toml");
        std::fs::create_dir(&blocked).unwrap();
        ui.editor = None;
        ui.execute(
            &socket,
            Action::SetConfig("rules".into(), blocked.clone()),
            Some((
                Prompt::Config("rules".into()),
                blocked.display().to_string(),
            )),
        )
        .await;
        assert!(ui.recovery.is_some());
        ui.key(key('c'));
        assert_eq!(ui.input, blocked.display().to_string());
        assert!(matches!(&ui.prompt, Some(Prompt::Config(name)) if name == "rules"));
        ui.prompt = None;
        // Repair a real filesystem error and retry without restarting the UI.
        let locked = directory.join("locked");
        #[cfg(unix)]
        {
            std::fs::create_dir(&locked).unwrap();
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).unwrap();
        }
        #[cfg(windows)]
        std::fs::write(&locked, "parent occupied by file").unwrap();
        let locked_path = locked.join("rule.toml");
        ui.execute(
            &socket,
            Action::SetConfig("rules".into(), locked_path.clone()),
            Some((
                Prompt::Config("rules".into()),
                locked_path.display().to_string(),
            )),
        )
        .await;
        #[cfg(unix)]
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
        #[cfg(windows)]
        {
            std::fs::remove_file(&locked).unwrap();
            std::fs::create_dir(&locked).unwrap();
        }
        assert!(ui.recovery.is_some());
        ui.key(key('c'));
        let Input::Run(retry, origin) =
            ui.key(event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        else {
            panic!("retry action");
        };
        ui.execute(&socket, retry, origin).await;
        assert!(ui.recovery.is_none());
        assert!(ui.editor.is_some());
        assert!(locked_path.is_file());
        // Draft + cursor + recovery survive a real file roundtrip with private permissions.
        ui.editor
            .as_mut()
            .unwrap()
            .key(event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        ui.editor
            .as_mut()
            .unwrap()
            .paste("/tmp/remembered-field.sock");
        ui.fail(
            "recoverable error",
            Resume::Request(Action::Validate("rules".into())),
        );
        let session_path = home.join("ui-session.json");
        assert!(ui.checkpoint(&session_path));
        let restored = Session::load(&session_path).unwrap();
        assert_eq!(
            restored.editor.as_ref().unwrap().text(),
            ui.editor.as_ref().unwrap().text()
        );
        assert!(restored.recovery.is_some());
        #[cfg(unix)]
        assert_eq!(
            std::fs::metadata(&session_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        call(&socket, Action::Shutdown).await.unwrap();
        manager.await.unwrap().unwrap();
        std::fs::remove_dir_all(directory).unwrap();
    }
}
