use crate::input::{KeyCode, KeyEvent, KeyModifiers};
use crate::protocol::*;
use crate::{i18n::t, tr};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::{cell::Cell, collections::VecDeque, path::Path};
use tokio::{sync::mpsc, task::JoinHandle};

#[derive(Clone, Serialize, Deserialize, Default)]
pub enum Mode {
    #[default]
    Browse,
    Filter(String),
    Search(String),
    DisplayLevel(usize),
    CaptureLevel(usize),
    MemoryLimit(String),
    /// Picking a log session; the index is into `Viewer::sessions`.
    Session(usize),
}
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    #[serde(default)]
    pub level_order_version: u8,
    pub filter: String,
    pub search: String,
    pub min_level: Option<LogLevel>,
    pub follow: bool,
    pub mode: Mode,
    #[serde(default = "default_memory_lines")]
    pub memory_lines: usize,
    #[serde(default)]
    pub wrap: bool,
    #[serde(default)]
    pub vim_navigation: bool,
    /// A selected earlier session; never persisted, so every launch starts on
    /// the current session.
    #[serde(skip)]
    pub session: Option<String>,
}
pub fn default_memory_lines() -> usize {
    5000
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            level_order_version: 1,
            filter: String::new(),
            search: String::new(),
            min_level: None,
            follow: true,
            mode: Mode::Browse,
            memory_lines: default_memory_lines(),
            wrap: false,
            vim_navigation: false,
            session: None,
        }
    }
}
pub struct Feed {
    pub task: JoinHandle<()>,
    pub receiver: mpsc::Receiver<LogEvent>,
}
impl Drop for Feed {
    fn drop(&mut self) {
        self.task.abort();
    }
}
#[derive(Clone)]
pub struct Record {
    pub text: String,
    pub level: Option<LogLevel>,
}
pub enum Command {
    None,
    Close,
    Reconnect,
    Capture(LogLevel),
    /// Reconnect to another session; `None` is the current session.
    Session(Option<String>),
}
pub struct Viewer {
    pub waker: crate::waker::Waker,
    pub name: String,
    pub settings: Settings,
    pub capture_level: LogLevel,
    pub capture_supported: bool,
    pub cursor: Option<LogCursor>,
    pub feed: Option<Feed>,
    pub records: VecDeque<Record>,
    pub partial: Vec<u8>,
    pub skip_line: bool,
    pub bytes: usize,
    pub dropped: usize,
    pub top: usize,
    pub horizontal: u16,
    pub status: String,
    pub viewport_height: Cell<usize>,
    pub viewport_width: Cell<u16>,
    pub source: std::path::PathBuf,
    /// Sessions of this source, newest first, as listed when connecting.
    pub sessions: Vec<astra_management::v2::LogSession>,
    /// Local directory holding this source's session files (display only).
    pub source_dir: std::path::PathBuf,
    pub vim_g: bool,
    pub frozen: Option<(VecDeque<Record>, String)>,
    pub revision: u64,
}
impl Viewer {
    pub fn with_waker(mut self, waker: crate::waker::Waker) -> Self {
        self.waker = waker;
        self
    }
    pub fn new(name: String, mut settings: Settings) -> Self {
        settings.memory_lines = settings.memory_lines.clamp(1, 100_000);
        if settings.level_order_version == 0 {
            match &mut settings.mode {
                Mode::DisplayLevel(index) if *index >= 3 => *index += 1,
                Mode::CaptureLevel(index) if *index >= 2 => *index += 1,
                _ => {}
            }
            settings.level_order_version = 1;
        }
        Self {
            waker: Default::default(),
            name,
            settings,
            capture_level: LogLevel::Info,
            capture_supported: true,
            cursor: None,
            feed: None,
            records: VecDeque::new(),
            partial: vec![],
            skip_line: false,
            bytes: 0,
            dropped: 0,
            top: 0,
            horizontal: 0,
            status: t("连接中").into(),
            viewport_height: Cell::new(10),
            viewport_width: Cell::new(80),
            source: Default::default(),
            sessions: vec![],
            source_dir: Default::default(),
            vim_g: false,
            frozen: None,
            revision: 0,
        }
    }
    pub async fn connect(&mut self, socket: &Path) -> Result<()> {
        if self.source_dir.as_os_str().is_empty() {
            self.source_dir = socket
                .parent()
                .unwrap_or(Path::new("."))
                .join("logs")
                .join(crate::tui_log::source_name(&self.name).unwrap_or(&self.name));
        }
        self.update_source();
        let mut stream = crate::log_stream::connect(
            socket,
            self.name.clone(),
            self.settings.session.clone(),
            self.cursor,
        )
        .await?;
        let (sender, receiver) = mpsc::channel(16);
        let waker = self.waker.clone();
        let task = tokio::spawn(async move {
            loop {
                let event = match astra_ipc::receive_management::<LogEvent>(&mut stream).await {
                    Ok(event) => event,
                    Err(error) => {
                        let _ = sender
                            .send(LogEvent::Error(tr!("Log disconnected: {}", error)))
                            .await;
                        waker.wake();
                        break;
                    }
                };
                let failed = matches!(event, LogEvent::Error(_));
                let sent = sender.send(event).await;
                waker.wake();
                if sent.is_err() || failed {
                    break;
                }
            }
        });
        self.feed = Some(Feed { task, receiver });
        self.status = t("实时连接").into();
        Ok(())
    }
    /// Index in `sessions` of the session being shown.
    pub fn selected_session_index(&self) -> usize {
        self.sessions
            .iter()
            .position(|session| match &self.settings.session {
                Some(id) => &session.id == id,
                None => session.current,
            })
            .unwrap_or(0)
    }
    /// The command that shows `sessions[index]`; the current one follows live.
    pub fn select_session(&mut self, index: usize) -> Command {
        let Some(session) = self.sessions.get(index) else {
            return Command::None;
        };
        Command::Session((!session.current).then(|| session.id.clone()))
    }
    fn update_source(&mut self) {
        let index = self.selected_session_index();
        self.source = match self.sessions.get(index) {
            Some(session) => self.source_dir.join(format!("{}.log", session.id)),
            None => self.source_dir.clone(),
        };
    }
    /// Install the session list fetched for this source.
    pub fn set_sessions(
        &mut self,
        sessions: Vec<astra_management::v2::LogSession>,
        source_dir: std::path::PathBuf,
    ) {
        self.sessions = sessions;
        self.source_dir = source_dir;
        // A selected session that no longer exists falls back to the current one.
        if let Some(id) = &self.settings.session {
            if !self.sessions.iter().any(|session| &session.id == id) {
                self.settings.session = None;
            }
        }
        self.update_source();
    }
    /// One picker row per session: start time, size, and which is current.
    pub fn session_choices(&self) -> Vec<String> {
        self.sessions
            .iter()
            .map(|session| {
                let size = crate::installation::format_bytes(session.size.parse().unwrap_or(0));
                let start = session_time(&session.started_ms);
                if session.current {
                    tr!("本次会话 · {} · {}", start, size)
                } else {
                    format!("{start} · {size}")
                }
            })
            .collect()
    }
    /// Which session is shown, for status lines.
    pub fn session_label(&self) -> String {
        match (
            &self.settings.session,
            self.sessions.get(self.selected_session_index()),
        ) {
            (None, Some(session)) => {
                tr!("会话：本次（{}）", session_time(&session.started_ms))
            }
            (None, None) => t("会话：本次").into(),
            (Some(_), Some(session)) => {
                tr!("会话：历史（{}）", session_time(&session.started_ms))
            }
            (Some(id), None) => tr!("会话：历史（{}）", id),
        }
    }
    pub fn stop(&mut self) {
        self.feed = None;
        self.status = t("已停止订阅；r 重连").into();
        self.revision = self.revision.wrapping_add(1);
    }
    pub fn revision(&self) -> u64 {
        self.revision
    }
    pub fn set_vim_navigation(&mut self, enabled: bool) {
        self.settings.vim_navigation = enabled;
    }
    pub fn set_capture_supported(&mut self, supported: bool) {
        self.capture_supported = supported;
        if !supported && matches!(self.settings.mode, Mode::CaptureLevel(_)) {
            self.cancel_input();
        }
    }
    pub fn paused(&self) -> bool {
        self.frozen.is_some()
    }
    pub fn toggle_pause(&mut self) {
        if self.frozen.take().is_some() {
            self.settings.follow = true;
            self.top = self
                .display_len()
                .saturating_sub(self.viewport_height.get());
        } else {
            self.top = if self.settings.follow {
                self.display_len()
                    .saturating_sub(self.viewport_height.get())
            } else {
                self.top
            };
            self.frozen = Some((
                self.records.clone(),
                crate::tui_log::stamp(astra_ipc::progress::now_ms()),
            ));
            self.settings.follow = false;
        }
        self.revision = self.revision.wrapping_add(1);
    }
    pub fn cancel_input(&mut self) {
        self.settings.mode = Mode::Browse;
    }
    pub fn capture_changed(&mut self, level: LogLevel) {
        self.capture_level = level;
        self.settings.mode = Mode::Browse;
    }
    pub fn paste(&mut self, text: &str) {
        match &mut self.settings.mode {
            Mode::Filter(input) | Mode::Search(input) | Mode::MemoryLimit(input) => {
                input.extend(text.chars().filter(|c| !c.is_control()))
            }
            _ => {}
        }
    }
    pub fn poll(&mut self) -> Option<String> {
        let mut events = vec![];
        let mut disconnected = false;
        if let Some(feed) = &mut self.feed {
            for _ in 0..32 {
                match feed.receiver.try_recv() {
                    Ok(event) => events.push(event),
                    Err(mpsc::error::TryRecvError::Empty) => break,
                    Err(_) => {
                        disconnected = true;
                        break;
                    }
                }
            }
        }
        for event in events {
            if let LogEvent::Error(error) = event {
                self.stop();
                self.status = error.clone();
                return Some(error);
            }
            self.apply(event);
        }
        if disconnected && self.feed.is_some() {
            self.stop();
            return Some(t("Log subscription disconnected; continue to reconnect").into());
        }
        None
    }
    pub fn apply(&mut self, event: LogEvent) {
        self.revision = self.revision.wrapping_add(1);
        match event {
            LogEvent::Waiting => self.status = t("等待实例产生日志（订阅保持连接）").into(),
            LogEvent::Data {
                bytes,
                cursor,
                reset,
                skip_prefix,
            } => {
                if reset {
                    if self.cursor.is_some() {
                        self.push(
                            t(if self.settings.session.is_none() {
                                "── 新的日志会话开始 ──"
                            } else {
                                "── 日志已轮换或截断，继续接收 ──"
                            })
                            .into(),
                        );
                    }
                    self.partial.clear();
                    self.skip_line = skip_prefix;
                }
                self.cursor = Some(cursor);
                self.status = t("实时连接").into();
                self.partial.extend(bytes);
                while let Some(end) = self.partial.iter().position(|b| *b == b'\n') {
                    let mut line: Vec<_> = self.partial.drain(..=end).collect();
                    line.pop();
                    if self.skip_line {
                        self.skip_line = false;
                    } else {
                        if line.len() > 64 * 1024 {
                            line.truncate(64 * 1024);
                            line.extend_from_slice(t(" [超长行已截断]").as_bytes());
                        }
                        self.push(strip_ansi(&String::from_utf8_lossy(&line)));
                    }
                }
                if self.skip_line {
                    self.partial.clear();
                } else if self.partial.len() > 64 * 1024 {
                    let prefix = self.partial.drain(..64 * 1024).collect::<Vec<_>>();
                    self.push(tr!(
                        "{} [超长行已截断]",
                        strip_ansi(&String::from_utf8_lossy(&prefix))
                    ));
                    self.partial.clear();
                    self.skip_line = true;
                }
            }
            LogEvent::Error(_) => {}
        }
    }
    pub fn push(&mut self, text: String) {
        self.bytes += text.len();
        let level = detect_level(&text);
        self.records.push_back(Record { text, level });
        self.evict();
    }
    pub fn evict(&mut self) {
        // The manager streams an existing on-disk log: eviction never loses its source.
        while self.records.len() > self.settings.memory_lines || self.bytes > 2 * 1024 * 1024 {
            if let Some(line) = self.records.pop_front() {
                self.bytes -= line.text.len();
                self.dropped += 1;
                if self.frozen.is_none() && self.matches(&line) {
                    self.top = self.top.saturating_sub(self.row_count(&line));
                }
            }
        }
    }
    pub fn matches(&self, line: &Record) -> bool {
        self.settings.min_level.is_none_or(|min| {
            line.level
                .is_none_or(|level| level.severity() >= min.severity())
        }) && (self.settings.filter.is_empty()
            || line
                .text
                .to_lowercase()
                .contains(&self.settings.filter.to_lowercase()))
    }
    pub fn visible(&self) -> Vec<&Record> {
        self.frozen
            .as_ref()
            .map_or(&self.records, |(records, _)| records)
            .iter()
            .filter(|line| self.matches(line))
            .collect()
    }
    pub fn browsing(&self) -> bool {
        matches!(self.settings.mode, Mode::Browse)
    }
    pub fn accepts_text_input(&self) -> bool {
        matches!(
            self.settings.mode,
            Mode::Filter(_) | Mode::Search(_) | Mode::MemoryLimit(_)
        )
    }
    pub fn row_count(&self, record: &Record) -> usize {
        if self.settings.wrap {
            wrapped_rows(&record.text, self.viewport_width.get().max(1)).max(1)
        } else {
            1
        }
    }
    pub fn display_len(&self) -> usize {
        self.visible()
            .iter()
            .map(|record| self.row_count(record))
            .sum()
    }
    pub fn set_filter(&mut self, value: String) {
        self.settings.filter = value;
        self.top = 0;
    }
    pub fn set_search(&mut self, value: String) {
        self.settings.search = value;
        self.jump(false);
    }
    pub fn set_wrap(&mut self, value: bool) {
        self.settings.wrap = value;
        self.horizontal = 0;
        self.top = 0;
    }
    pub fn set_follow(&mut self, value: bool) {
        self.settings.follow = value;
        if value {
            self.frozen = None;
        }
    }
    pub fn set_display_level(&mut self, value: Option<LogLevel>) {
        self.settings.min_level = value;
        self.top = 0;
    }
    pub fn set_memory_limit(&mut self, value: &str) -> Result<(), String> {
        let limit = value
            .parse::<usize>()
            .ok()
            .filter(|n| (1..=100_000).contains(n))
            .ok_or_else(|| t("Enter a line limit from 1 to 100000").to_owned())?;
        self.settings.memory_lines = limit;
        self.evict();
        self.records.shrink_to_fit();
        self.status = t("Log memory limit updated").into();
        Ok(())
    }
    pub fn selected_record_index(&self) -> usize {
        let mut rows = 0;
        self.visible()
            .iter()
            .position(|record| {
                rows += self.row_count(record);
                rows > self.top
            })
            .unwrap_or(0)
    }
    pub fn begin_search(&mut self) {
        if self.browsing() {
            self.settings.mode = Mode::Search(self.settings.search.clone());
        }
    }
    pub fn find_again(&mut self, backwards: bool) {
        self.jump(backwards);
    }

    pub fn key(&mut self, mut key: KeyEvent) -> Command {
        if !self.capture_supported && matches!(self.settings.mode, Mode::CaptureLevel(_)) {
            self.cancel_input();
            return Command::None;
        }
        if self.browsing()
            && key.code == KeyCode::Char('f')
            && key.modifiers.contains(KeyModifiers::CONTROL)
        {
            self.settings.mode = Mode::Search(self.settings.search.clone());
            return Command::None;
        }
        if self.browsing() && key.code == KeyCode::F(3) {
            self.jump(key.modifiers.contains(KeyModifiers::SHIFT));
            return Command::None;
        }
        match &mut self.settings.mode {
            Mode::Filter(input) | Mode::Search(input) | Mode::MemoryLimit(input) => {
                match key.code {
                    KeyCode::Esc => self.cancel_input(),
                    KeyCode::Backspace => {
                        input.pop();
                    }
                    KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        input.clear()
                    }
                    KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        input.push(c)
                    }
                    KeyCode::Enter => {
                        if let Mode::MemoryLimit(input) = &self.settings.mode {
                            match input.parse::<usize>() {
                                Ok(value) if (1..=100_000).contains(&value) => {}
                                _ => {
                                    self.status = t("Enter a line limit from 1 to 100000").into();
                                    return Command::None;
                                }
                            }
                        }
                        let mode = std::mem::take(&mut self.settings.mode);
                        match mode {
                            Mode::Filter(text) => {
                                self.settings.filter = text;
                                self.top = 0;
                            }
                            Mode::Search(text) => {
                                self.settings.search = text;
                                self.jump(false);
                            }
                            Mode::MemoryLimit(text) => {
                                self.settings.memory_lines = text.parse().unwrap();
                                self.evict();
                                self.records.shrink_to_fit();
                                self.status = t("Log memory limit updated").into();
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
                return Command::None;
            }
            Mode::DisplayLevel(_) | Mode::CaptureLevel(_) => {
                let display = matches!(self.settings.mode, Mode::DisplayLevel(_));
                let (Mode::DisplayLevel(selected) | Mode::CaptureLevel(selected)) =
                    &mut self.settings.mode
                else {
                    unreachable!()
                };
                let max = LogLevel::ALL.len() - usize::from(!display);
                match key.code {
                    KeyCode::Esc => self.cancel_input(),
                    KeyCode::Up | KeyCode::Char('k') => *selected = selected.saturating_sub(1),
                    KeyCode::Down | KeyCode::Char('j') => *selected = (*selected + 1).min(max),
                    KeyCode::Enter => {
                        let selected = (*selected).min(max);
                        if display {
                            self.settings.min_level = if selected == 0 {
                                None
                            } else {
                                Some(LogLevel::ALL[selected - 1])
                            };
                            self.settings.mode = Mode::Browse;
                            self.top = 0;
                        } else {
                            return Command::Capture(LogLevel::ALL[selected]);
                        }
                    }
                    _ => {}
                }
                return Command::None;
            }
            Mode::Session(selected) => {
                let max = self.sessions.len().saturating_sub(1);
                match key.code {
                    KeyCode::Esc => self.cancel_input(),
                    KeyCode::Up | KeyCode::Char('k') => *selected = selected.saturating_sub(1),
                    KeyCode::Down | KeyCode::Char('j') => *selected = (*selected + 1).min(max),
                    KeyCode::Enter => {
                        let selected = (*selected).min(max);
                        self.settings.mode = Mode::Browse;
                        return self.select_session(selected);
                    }
                    _ => {}
                }
                return Command::None;
            }
            Mode::Browse => {}
        }
        if self.settings.vim_navigation
            && key.code == KeyCode::Char('g')
            && key.modifiers.is_empty()
        {
            if !self.vim_g {
                self.vim_g = true;
                return Command::None;
            }
            key.code = KeyCode::Home;
        }
        self.vim_g = false;
        if self.settings.vim_navigation {
            key.code = match key.code {
                KeyCode::Char('j') => KeyCode::Down,
                KeyCode::Char('k') => KeyCode::Up,
                KeyCode::Char('h') => KeyCode::Left,
                KeyCode::Char('l') => KeyCode::Right,
                KeyCode::Char('G') => KeyCode::End,
                KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    KeyCode::PageDown
                }
                KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    KeyCode::PageUp
                }
                code => code,
            };
        }
        match key.code {
            KeyCode::Esc => return Command::Close,
            KeyCode::Char('r') => return Command::Reconnect,
            KeyCode::Char('w') => {
                self.settings.wrap = !self.settings.wrap;
                self.horizontal = 0;
                self.top = 0;
            }
            KeyCode::Char('/') => self.settings.mode = Mode::Search(self.settings.search.clone()),
            KeyCode::Char('f') => self.settings.mode = Mode::Filter(self.settings.filter.clone()),
            KeyCode::Char('m') => self.settings.mode = Mode::MemoryLimit(String::new()),
            KeyCode::Char('s') if !self.sessions.is_empty() => {
                self.settings.mode = Mode::Session(self.selected_session_index())
            }
            KeyCode::Char('v') => {
                self.settings.mode = Mode::DisplayLevel(
                    self.settings
                        .min_level
                        .map(|l| l.severity() as usize + 1)
                        .unwrap_or(0),
                )
            }
            KeyCode::Char('L') if self.capture_supported => {
                self.settings.mode = Mode::CaptureLevel(self.capture_level.severity() as usize)
            }
            KeyCode::Char('n') => self.jump(false),
            KeyCode::Char('N') => self.jump(true),
            KeyCode::Char(' ') => self.toggle_pause(),
            KeyCode::Up | KeyCode::PageUp => {
                if self.settings.follow {
                    self.top = self
                        .display_len()
                        .saturating_sub(self.viewport_height.get());
                }
                self.settings.follow = false;
                self.top = self.top.saturating_sub(if key.code == KeyCode::Up {
                    1
                } else {
                    self.viewport_height.get()
                });
            }
            KeyCode::Down | KeyCode::PageDown => {
                if self.settings.follow {
                    self.top = self
                        .display_len()
                        .saturating_sub(self.viewport_height.get());
                }
                self.settings.follow = false;
                self.top = (self.top
                    + if key.code == KeyCode::Down {
                        1
                    } else {
                        self.viewport_height.get()
                    })
                .min(self.display_len().saturating_sub(1));
            }
            KeyCode::Home => {
                self.settings.follow = false;
                self.top = 0;
            }
            KeyCode::End => {
                self.frozen = None;
                self.settings.follow = true;
            }
            KeyCode::Left => self.horizontal = self.horizontal.saturating_sub(8),
            KeyCode::Right => self.horizontal = self.horizontal.saturating_add(8),
            KeyCode::Backspace => {
                self.settings.filter.clear();
                self.settings.search.clear();
                self.settings.min_level = None;
                self.top = 0;
            }
            _ => {}
        }
        Command::None
    }
    pub fn jump(&mut self, backward: bool) {
        if self.settings.search.is_empty() {
            return;
        }
        let lines = self.visible();
        if lines.is_empty() {
            return;
        }
        let query = self.settings.search.to_lowercase();
        let at = if self.settings.follow {
            lines.len() - 1
        } else {
            let mut offset = 0;
            lines
                .iter()
                .position(|record| {
                    offset += self.row_count(record);
                    offset > self.top
                })
                .unwrap_or(lines.len() - 1)
        };
        let found = (1..=lines.len())
            .map(|step| {
                if backward {
                    (at + lines.len() - step % lines.len()) % lines.len()
                } else {
                    (at + step) % lines.len()
                }
            })
            .find(|i| lines[*i].text.to_lowercase().contains(&query));
        if let Some(found) = found {
            self.top = lines
                .iter()
                .take(found)
                .map(|record| self.row_count(record))
                .sum();
            self.settings.follow = false;
        }
    }
}
pub fn detect_level(text: &str) -> Option<LogLevel> {
    text.split_whitespace().take(4).find_map(|word| {
        word.trim_matches(|c: char| !c.is_ascii_alphabetic())
            .parse()
            .ok()
    })
}
pub fn strip_ansi(text: &str) -> String {
    let mut out = String::new();
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            match chars.next() {
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&c) {
                            break;
                        }
                    }
                }
                Some(']') => {
                    while let Some(c) = chars.next() {
                        if c == '\x07' || c == '\x1b' && chars.peek() == Some(&'\\') {
                            if c == '\x1b' {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                _ => {}
            }
        } else if c == '\t' {
            out.push_str("    ");
        } else if !c.is_control() {
            out.push(c);
        }
    }
    out
}

/// Count word-wrapped display rows without tying the log model to a renderer.
/// Whitespace is retained, with spaces at a wrapping boundary consumed by the
/// preceding row; zero-width graphemes and non-breaking spaces keep their width.
pub fn wrapped_rows(text: &str, width: u16) -> usize {
    use unicode_segmentation::UnicodeSegmentation;
    use unicode_width::UnicodeWidthStr;
    let limit = usize::from(width.max(1));
    text.lines()
        .map(|line| {
            let (mut rows, mut line_len, mut line_width, mut word_len, mut word_width) =
                (0, 0, 0, 0, 0);
            let mut whitespace = VecDeque::new();
            let mut whitespace_width = 0;
            let mut previous_word = false;
            for symbol in line.graphemes(true) {
                let space = symbol == "\u{200b}"
                    || (symbol.chars().all(char::is_whitespace) && symbol != "\u{a0}");
                let symbol_width = symbol.width();
                if symbol_width > limit {
                    continue;
                }
                if (previous_word && space)
                    || (line_len == 0 && word_width + whitespace_width + symbol_width > limit)
                {
                    line_len += whitespace.len() + word_len;
                    line_width += whitespace_width + word_width;
                    whitespace.clear();
                    whitespace_width = 0;
                    word_len = 0;
                    word_width = 0;
                }
                if line_width >= limit
                    || (symbol_width > 0 && line_width + whitespace_width + word_width >= limit)
                {
                    let mut remaining = limit.saturating_sub(line_width);
                    rows += 1;
                    line_len = 0;
                    line_width = 0;
                    while let Some(&w) = whitespace.front() {
                        if w > remaining {
                            break;
                        }
                        whitespace_width -= w;
                        remaining -= w;
                        whitespace.pop_front();
                    }
                    if space && whitespace.is_empty() {
                        continue;
                    }
                }
                if space {
                    whitespace_width += symbol_width;
                    whitespace.push_back(symbol_width);
                } else {
                    word_width += symbol_width;
                    word_len += 1;
                }
                previous_word = !space;
            }
            if line_len == 0 && word_len == 0 && !whitespace.is_empty() {
                rows += 1;
            }
            line_len += whitespace.len() + word_len;
            if line_len > 0 {
                rows += 1;
            }
            rows.max(1)
        })
        .sum::<usize>()
        .max(1)
}

#[cfg(test)]
#[path = "test/logs_test.rs"]
mod tests;

impl crate::state::Client {
    pub fn log_sources(&self) -> Vec<String> {
        let mut names: Vec<_> = self.rows.iter().map(|r| r.instance.name.clone()).collect();
        names.extend(["@tui".into(), "@gui".into()]);
        names
    }
    pub fn log_viewer(&self, frontend: bool) -> Option<&Viewer> {
        if frontend {
            self.tui_viewer.as_ref()
        } else {
            self.viewer.as_ref()
        }
    }
    pub fn log_viewer_mut(&mut self, frontend: bool) -> Option<&mut Viewer> {
        if frontend {
            self.tui_viewer.as_mut()
        } else {
            self.viewer.as_mut()
        }
    }
    pub fn frontend_log_command(&mut self, command: Command) -> crate::state::Input {
        let Some(viewer) = &self.tui_viewer else {
            return crate::state::Input::Idle;
        };
        let name = viewer.name.clone();
        match command {
            Command::Close => {
                let viewer = self.tui_viewer.take().unwrap();
                self.log_settings.insert(name, viewer.settings);
                self.active_log = None;
                self.overlay_epoch = self.overlay_epoch.wrapping_add(1);
            }
            Command::Reconnect => return crate::state::Input::Run(Action::Logs(name), None),
            Command::Session(session) => {
                let mut settings = viewer.settings.clone();
                settings.session = session;
                self.log_settings.insert(name.clone(), settings);
                return crate::state::Input::Run(Action::Logs(name), None);
            }
            Command::Capture(level) if viewer.capture_supported => {
                return crate::state::Input::Run(Action::SetLogLevel { name, level }, None)
            }
            _ => {}
        }
        crate::state::Input::Idle
    }
}

#[cfg(test)]
#[path = "test/observability_test.rs"]
mod observability_tests;

/// Local start time of a session, `MM-DD HH:MM:SS`.
pub fn session_time(started_ms: &str) -> String {
    let ms = started_ms.parse::<i64>().unwrap_or(0);
    chrono::DateTime::from_timestamp_millis(ms)
        .map(|time| {
            time.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| started_ms.to_owned())
}
