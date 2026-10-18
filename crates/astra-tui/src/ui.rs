use crate::editor::Editor;
use crate::logs;
use crate::protocol::*;
use anyhow::{Context, Result};
use crossterm::{
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEventKind,
        KeyModifiers,
    },
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Style},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
    Terminal,
};
use serde::{Deserialize, Serialize};
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
}
impl Prompt {
    fn title(&self) -> &str {
        match self {
            Self::AddName => "New instance name (ASCII; current core tab)",
            Self::AddPath(_) => "Configuration path (missing file: create default and edit)",
            Self::Config(_) => "Select configuration path (missing file: create default and edit)",
            Self::Clone(_) => "New configuration path (same directory; will not overwrite)",
            Self::Remove(_) => "Remove instance? Type yes; config file is retained",
            Self::Shutdown => "Stop ALL instances and manager? Type yes",
            Self::Discard => "Discard unsaved edits? Type yes",
        }
    }
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
#[derive(Serialize, Deserialize)]
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
    rows: Vec<Row>,
    message: String,
    viewer: Option<logs::Viewer>,
    scroll: u16,
    refresh_paused: bool,
    persistence_paused: bool,
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
}

impl Ui {
    fn new(session: Session) -> Self {
        Self {
            session,
            rows: vec![],
            message: "Connected via local IPC; operation state and drafts are remembered".into(),
            viewer: None,
            scroll: 0,
            refresh_paused: false,
            persistence_paused: false,
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
    fn prompt(&mut self, prompt: Prompt) {
        if matches!(
            prompt,
            Prompt::AddName | Prompt::Config(_) | Prompt::Clone(_)
        ) {
            self.working_dir = launch_directory();
        }
        self.prompt = Some(prompt);
        self.input.clear();
    }
    fn fail(&mut self, error: impl std::fmt::Display, resume: Resume) {
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
                    format!("Cannot remember operation state: {error:#}"),
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
                    Resume::Request(action) => return Input::Run(action, None),
                    Resume::Editor => {
                        self.message =
                            "Entry draft retained; correct the value and press Enter".into();
                    }
                    Resume::Session => {
                        self.message = "Continuing; retrying session save".into();
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
                    "Name: 1–24 ASCII letters, digits, '-' or '_'"
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
            _ => None,
        })
    }
    fn key(&mut self, key: event::KeyEvent) -> Input {
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
        if self.prompt.is_some() {
            match key.code {
                KeyCode::Esc => {
                    if let Some(Prompt::AddPath(name)) = self.prompt.take() {
                        self.prompt = Some(Prompt::AddName);
                        self.input = name;
                    }
                }
                KeyCode::Backspace => {
                    self.input.pop();
                }
                KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.input.push(c)
                }
                KeyCode::Enter => {
                    let prompt = self.prompt.take().unwrap();
                    let input = self.input.trim().to_owned();
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
        } else if let Some(editor) = &mut self.editor {
            if key.code == KeyCode::Char('s') && key.modifiers.contains(KeyModifiers::CONTROL) {
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
        } else {
            let index = self.index();
            let name = self.selected().map(|r| r.instance.name.clone());
            match key.code {
                KeyCode::Char('q') => return Input::Quit,
                KeyCode::Char('Q') => self.prompt(Prompt::Shutdown),
                KeyCode::Tab => {
                    self.kind = if self.kind == Kind::Proxy {
                        Kind::Rule
                    } else {
                        Kind::Proxy
                    }
                }
                KeyCode::Up => self.selections[index] = self.selections[index].saturating_sub(1),
                KeyCode::Down => {
                    self.selections[index] =
                        (self.selections[index] + 1).min(self.rows().len().saturating_sub(1))
                }
                KeyCode::Char('n') => self.prompt(Prompt::AddName),
                KeyCode::Char(c) if name.is_some() => {
                    let name = name.unwrap();
                    let action = match c {
                        's' => Some(Action::Start(name)),
                        'x' => Some(Action::Stop(name)),
                        'r' => Some(Action::Restart(name)),
                        'R' => Some(Action::Reload(name)),
                        'v' => Some(Action::Validate(name)),
                        'e' => Some(Action::Read(name)),
                        'l' => Some(Action::Logs(name)),
                        'd' => {
                            self.prompt(Prompt::Remove(name));
                            None
                        }
                        'c' => {
                            self.prompt(Prompt::Config(name));
                            None
                        }
                        'y' => {
                            self.prompt(Prompt::Clone(name));
                            None
                        }
                        _ => None,
                    };
                    if let Some(action) = action {
                        return Input::Run(action, None);
                    }
                }
                _ => {}
            }
        }
        Input::Idle
    }
    async fn execute(
        &mut self,
        socket: &std::path::Path,
        mut action: Action,
        mut origin: Option<(Prompt, String)>,
    ) -> bool {
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
                match call(socket, Action::PrepareConfig(name.clone())).await {
                    Ok(reply) if !reply.text.is_empty() => {
                        self.editor =
                            Some(Editor::new(name.clone(), reply.text).with_kind(self.kind));
                        self.message = "Default configuration created; review and Ctrl-S to save, then start explicitly".into();
                        return false;
                    }
                    Ok(_) => {}
                    Err(error) => {
                        self.fail(format!("{error:#}"), Resume::Request(action));
                        return false;
                    }
                }
            }
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
                    self.message = "Configuration ready; Ctrl-S validates and saves; missing files use defaults".into();
                    return false;
                }
                Action::SetLogLevel { level, .. } => {
                    if let Some(viewer) = &mut self.viewer {
                        viewer.capture_changed(level);
                    }
                    self.message = format!("记录级别已改为 {}，无需重启", level.as_str());
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
                    self.rows = reply.rows;
                    for (index, kind) in [Kind::Proxy, Kind::Rule].into_iter().enumerate() {
                        let count = self.rows.iter().filter(|r| r.instance.kind == kind).count();
                        self.selections[index] =
                            self.selections[index].min(count.saturating_sub(1));
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
            self.rows = reply.rows;
            if let Some(index) = self.rows().iter().position(|r| r.instance.name == name) {
                let kind = self.index();
                self.selections[kind] = index;
            }
        }
    }
    fn draw(&self, terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
        terminal.draw(|f| {
            let chunks = Layout::default().direction(Direction::Vertical).constraints([
                Constraint::Length(3), Constraint::Min(4), Constraint::Length(4), Constraint::Length(3),
            ]).split(f.area());
            let tabs = match self.kind { Kind::Proxy => "[ Proxy Core ]    Rule Core", Kind::Rule => "  Proxy Core    [ Rule Core ]" };
            f.render_widget(Paragraph::new(format!("Astra Dual Core  |  {tabs}  |  Tab: switch")).block(Block::default().borders(Borders::ALL)).style(Style::default().fg(Color::Cyan)), chunks[0]);
            if let Some(recovery) = &self.recovery {
                f.render_widget(Paragraph::new(format!("{}\n\nThe current step, input and editor draft are retained.\nPress c to continue / correct and retry. Press b to end this operation.", recovery.message)).scroll((self.scroll, 0)).wrap(Wrap { trim: false }).style(Style::default().fg(Color::Yellow)).block(Block::default().title("Operation paused / c continue / b end / ↑↓ scroll").borders(Borders::ALL)), chunks[1]);
            } else if let Some(editor) = &self.editor {
                editor.render(f,chunks[1]);
            } else if let Some(viewer) = &self.viewer {
                viewer.render(f,chunks[1]);
            } else {
                let items: Vec<ListItem> = self.rows().iter().map(|row| ListItem::new(format!("{:<24} {:<16} PID {:<8} {:>6}s {}", row.instance.name, row.state, row.pid.map(|p| p.to_string()).unwrap_or("—".into()), row.uptime, if row.changed { "[reload pending]" } else { "" }))).collect();
                let mut state = ListState::default(); if !items.is_empty() { state.select(Some(self.selections[self.index()])); }
                f.render_stateful_widget(List::new(items).block(Block::default().title("Instances / n add / ↑↓ select").borders(Borders::ALL)).highlight_style(Style::default().bg(Color::Blue)).highlight_symbol("› "), chunks[1], &mut state);
            }
            let detail = if let Some(prompt) = &self.prompt {
                format!("{}\n> {}▏ (Enter confirm / Esc cancel)", prompt.title(), self.input)
            } else if let Some(row) = self.selected() { format!("Config: {}\n{}", row.instance.config.display(), self.message) }
            else { self.message.clone() };
            f.render_widget(Paragraph::new(detail).wrap(Wrap { trim: false }).block(Block::default().borders(Borders::ALL)), chunks[2]);
            let help = if self.recovery.is_some() { "c continue / correct and retry    b end operation    ↑↓ scroll error\nCtrl-C detach with state retained" }
            else if self.viewer.is_some() { "/ 搜索 · n/N 跳转 · f 关键词过滤 · l 显示级别 · L 记录级别\n空格 暂停/跟随 · End 跟随最新 · r 重连 · Backspace 清筛选 · Esc 返回" }
            else if self.editor.is_some() { "↑↓ 选择 · Enter 编辑/展开节点 · Esc 收起节点/返回 · Ctrl-S 保存\nn 新增 · d 删除 · K/J 排序 · F2 显示/隐藏凭据 · Ctrl-C 记住草稿退出" }
            else { "s start  x stop  r restart  R reload  d remove  c config  e edit  v check  y copy  l logs\nq detach  Q stop all & quit  |  Ctrl-C detach and remember current step" };
            f.render_widget(Paragraph::new(help).style(Style::default().fg(Color::Gray)), chunks[3]);
        })?;
        Ok(())
    }
}

fn absolute(text: &str, directory: &std::path::Path) -> Result<PathBuf> {
    anyhow::ensure!(!text.is_empty(), "Enter a configuration file path");
    let path = PathBuf::from(text);
    Ok(if path.is_absolute() {
        path
    } else {
        directory.join(path)
    })
}

pub async fn run(socket: PathBuf) -> Result<()> {
    let session_path = socket
        .parent()
        .context("Manager socket has no directory")?
        .join("ui-session.json");
    let mut ui = match Session::load(&session_path) {
        Ok(session) => Ui::new(session),
        Err(error) => {
            let mut ui = Ui::new(Session::default());
            ui.fail(
                format!("Cannot restore operation state: {error:#}"),
                Resume::Session,
            );
            ui
        }
    };
    enable_raw_mode()?;
    let _screen = Screen;
    execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
    if let Some(name) = ui.active_log.clone() {
        ui.execute(&socket, Action::Logs(name), None).await;
    }
    let mut refreshed = Instant::now() - Duration::from_secs(3);
    loop {
        let stream_error = ui
            .viewer
            .as_mut()
            .and_then(|viewer| viewer.poll().map(|error| (viewer.name.clone(), error)));
        if let Some((name, error)) = stream_error {
            if ui.recovery.is_none() {
                ui.fail(error, Resume::Request(Action::Logs(name)));
                ui.checkpoint(&session_path);
            }
        }
        if refreshed.elapsed() > Duration::from_secs(2)
            && ui.editor.is_none()
            && ui.prompt.is_none()
            && ui.viewer.is_none()
            && ui.recovery.is_none()
            && !ui.refresh_paused
        {
            ui.execute(&socket, Action::List, None).await;
            if ui.recovery.is_some() {
                ui.checkpoint(&session_path);
            }
            refreshed = Instant::now();
        }
        ui.draw(&mut terminal)?;
        if !event::poll(Duration::from_millis(150))? {
            continue;
        }
        let input = match event::read()? {
            Event::Paste(text) if ui.recovery.is_none() => {
                if ui.prompt.is_some() {
                    ui.input.extend(text.chars().filter(|c| !c.is_control()));
                } else if let Some(editor) = &mut ui.editor {
                    editor.paste(&text);
                } else if let Some(viewer) = &mut ui.viewer {
                    viewer.paste(&text);
                }
                Input::Idle
            }
            Event::Key(key) if key.kind != KeyEventKind::Release => ui.key(key),
            _ => Input::Idle,
        };
        let quit = match input {
            Input::Quit => true,
            Input::Run(action, origin) => {
                ui.message = "Working via IPC…".into();
                ui.draw(&mut terminal)?;
                let quit = ui.execute(&socket, action, origin).await;
                refreshed = Instant::now() - Duration::from_secs(3);
                quit
            }
            Input::Idle => false,
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
        // Select IPC connections after standalone settings, socket and listeners.
        for _ in 0..5 {
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
