use crate::protocol::*;
use crate::{i18n::t, tr};
use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::Line,
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};
use serde::{Deserialize, Serialize};
use std::{cell::Cell, collections::VecDeque, path::Path};
use tokio::{sync::mpsc, task::JoinHandle};

#[derive(Clone, Serialize, Deserialize, Default)]
enum Mode {
    #[default]
    Browse,
    Filter(String),
    Search(String),
    DisplayLevel(usize),
    CaptureLevel(usize),
    MemoryLimit(String),
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    level_order_version: u8,
    filter: String,
    search: String,
    min_level: Option<LogLevel>,
    follow: bool,
    mode: Mode,
    #[serde(default = "default_memory_lines")]
    memory_lines: usize,
    #[serde(default)]
    wrap: bool,
}
fn default_memory_lines() -> usize {
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
        }
    }
}
struct Feed {
    task: JoinHandle<()>,
    receiver: mpsc::Receiver<LogEvent>,
}
impl Drop for Feed {
    fn drop(&mut self) {
        self.task.abort();
    }
}
struct Record {
    text: String,
    level: Option<LogLevel>,
}
pub enum Command {
    None,
    Close,
    Reconnect,
    Capture(LogLevel),
}
pub struct Viewer {
    pub name: String,
    pub settings: Settings,
    pub capture_level: LogLevel,
    pub cursor: Option<LogCursor>,
    feed: Option<Feed>,
    records: VecDeque<Record>,
    partial: Vec<u8>,
    skip_line: bool,
    bytes: usize,
    dropped: usize,
    top: usize,
    horizontal: u16,
    status: String,
    viewport_height: Cell<usize>,
    viewport_width: Cell<u16>,
    source: std::path::PathBuf,
    vim_g: bool,
}
impl Viewer {
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
            name,
            settings,
            capture_level: LogLevel::Info,
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
            vim_g: false,
        }
    }
    pub async fn connect(&mut self, socket: &Path) -> Result<()> {
        self.source = socket
            .parent()
            .unwrap_or(Path::new("."))
            .join(if self.name == "@tui" {
                "tui.log".to_owned()
            } else {
                format!("{}.log", self.name)
            });
        let mut stream = crate::log_stream::connect(socket, self.name.clone(), self.cursor).await?;
        let (sender, receiver) = mpsc::channel(16);
        let task = tokio::spawn(async move {
            loop {
                let event = match astra_ipc::receive_management::<LogEvent>(&mut stream).await {
                    Ok(event) => event,
                    Err(error) => {
                        let _ = sender
                            .send(LogEvent::Error(tr!("Log disconnected: {}", error)))
                            .await;
                        break;
                    }
                };
                let failed = matches!(event, LogEvent::Error(_));
                if sender.send(event).await.is_err() || failed {
                    break;
                }
            }
        });
        self.feed = Some(Feed { task, receiver });
        self.status = t("实时连接").into();
        Ok(())
    }
    pub fn stop(&mut self) {
        self.feed = None;
        self.status = t("已停止订阅；r 重连").into();
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
    fn apply(&mut self, event: LogEvent) {
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
                        self.push(t("── 日志已轮换或截断，继续接收 ──").into());
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
    fn push(&mut self, text: String) {
        self.bytes += text.len();
        let level = detect_level(&text);
        self.records.push_back(Record { text, level });
        self.evict();
    }
    fn evict(&mut self) {
        // The manager streams an existing on-disk log: eviction never loses its source.
        while self.records.len() > self.settings.memory_lines || self.bytes > 2 * 1024 * 1024 {
            if let Some(line) = self.records.pop_front() {
                self.bytes -= line.text.len();
                self.dropped += 1;
                if self.matches(&line) {
                    self.top = self.top.saturating_sub(self.row_count(&line));
                }
            }
        }
    }
    fn matches(&self, line: &Record) -> bool {
        self.settings.min_level.is_none_or(|min| {
            line.level
                .is_none_or(|level| level.severity() >= min.severity())
        }) && (self.settings.filter.is_empty()
            || line
                .text
                .to_lowercase()
                .contains(&self.settings.filter.to_lowercase()))
    }
    fn visible(&self) -> Vec<&Record> {
        self.records
            .iter()
            .filter(|line| self.matches(line))
            .collect()
    }
    #[cfg(test)]
    fn test_screen(&self, width: u16, height: u16) -> String {
        let mut terminal =
            ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height)).unwrap();
        terminal
            .draw(|frame| self.render(frame, frame.area()))
            .unwrap();
        terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>()
    }
    pub fn browsing(&self) -> bool {
        matches!(self.settings.mode, Mode::Browse)
    }
    fn row_count(&self, record: &Record) -> usize {
        if self.settings.wrap {
            Paragraph::new(record.text.as_str())
                .wrap(Wrap { trim: false })
                .line_count(self.viewport_width.get().max(1))
                .max(1)
        } else {
            1
        }
    }
    fn display_len(&self) -> usize {
        self.visible()
            .iter()
            .map(|record| self.row_count(record))
            .sum()
    }
    pub fn key(&mut self, mut key: KeyEvent) -> Command {
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
            Mode::Browse => {}
        }
        if key.code == KeyCode::Char('g') && key.modifiers.is_empty() {
            if !self.vim_g {
                self.vim_g = true;
                return Command::None;
            }
            key.code = KeyCode::Home;
        }
        self.vim_g = false;
        key.code = match key.code {
            KeyCode::Char('j') => KeyCode::Down,
            KeyCode::Char('k') => KeyCode::Up,
            KeyCode::Char('h') => KeyCode::Left,
            KeyCode::Char('l') => KeyCode::Right,
            KeyCode::Char('G') => KeyCode::End,
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                KeyCode::PageDown
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => KeyCode::PageUp,
            code => code,
        };
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
            KeyCode::Char('v') => {
                self.settings.mode = Mode::DisplayLevel(
                    self.settings
                        .min_level
                        .map(|l| l.severity() as usize + 1)
                        .unwrap_or(0),
                )
            }
            KeyCode::Char('L') => {
                self.settings.mode = Mode::CaptureLevel(self.capture_level.severity() as usize)
            }
            KeyCode::Char('n') => self.jump(false),
            KeyCode::Char('N') => self.jump(true),
            KeyCode::Char(' ') => {
                if self.settings.follow {
                    self.top = self
                        .display_len()
                        .saturating_sub(self.viewport_height.get());
                }
                self.settings.follow = !self.settings.follow;
            }
            KeyCode::Up | KeyCode::PageUp => {
                if self.settings.follow {
                    self.top = self
                        .display_len()
                        .saturating_sub(self.viewport_height.get());
                }
                self.settings.follow = false;
                self.top = self
                    .top
                    .saturating_sub(if key.code == KeyCode::Up { 1 } else { 10 });
            }
            KeyCode::Down | KeyCode::PageDown => {
                if self.settings.follow {
                    self.top = self
                        .display_len()
                        .saturating_sub(self.viewport_height.get());
                }
                self.settings.follow = false;
                self.top = (self.top + if key.code == KeyCode::Down { 1 } else { 10 })
                    .min(self.display_len().saturating_sub(1));
            }
            KeyCode::Home => {
                self.settings.follow = false;
                self.top = 0;
            }
            KeyCode::End => self.settings.follow = true,
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
    fn jump(&mut self, backward: bool) {
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
    pub fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        let parts = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(2), Constraint::Length(8)])
            .split(area);
        let visible = self.visible();
        let height = parts[0].height.saturating_sub(2) as usize;
        self.viewport_height.set(height.max(1));
        self.viewport_width
            .set(parts[0].width.saturating_sub(2).max(1));
        let top = if self.settings.follow {
            self.display_len().saturating_sub(height)
        } else {
            self.top.min(self.display_len().saturating_sub(1))
        };
        let mut skip = 0;
        let mut offset = top;
        while skip < visible.len() && offset >= self.row_count(visible[skip]) {
            offset -= self.row_count(visible[skip]);
            skip += 1;
        }
        let lines: Vec<Line> = visible
            .iter()
            .skip(skip)
            .take(height)
            .map(|line| {
                let color = match line.level {
                    Some(LogLevel::Error) => Color::Red,
                    Some(LogLevel::Warn) => Color::Yellow,
                    Some(LogLevel::Debug | LogLevel::Trace) => Color::Gray,
                    Some(LogLevel::Route) => Color::Cyan,
                    _ => Color::White,
                };
                // Only the matching text is marked; the rest keeps its level color.
                Line::from(crate::theme::mark_matches(
                    &line.text,
                    &self.settings.search,
                    Style::default().fg(color),
                ))
            })
            .collect();
        let title = tr!(
            "实时日志 · {} · {} · 显示 {} · 记录 {}",
            self.name,
            if self.settings.follow {
                t("跟随")
            } else {
                t("暂停跟随")
            },
            self.settings
                .min_level
                .map(|l| format!("{}+", l.as_str()))
                .unwrap_or(t("全部").into()),
            self.capture_level.as_str()
        );
        let paragraph =
            Paragraph::new(lines).block(Block::default().title(title).borders(Borders::ALL));
        let paragraph = if self.settings.wrap {
            paragraph
                .wrap(Wrap { trim: false })
                .scroll((offset.min(u16::MAX as usize) as u16, 0))
        } else {
            paragraph.scroll((0, self.horizontal))
        };
        frame.render_widget(paragraph, parts[0]);
        let status = tr!(
            "{}\n{}/{} lines · limit {} · released {}\n过滤: {}\n搜索: {}\nLocal log: {}",
            self.status,
            visible.len(),
            self.records.len(),
            self.settings.memory_lines,
            self.dropped,
            self.settings.filter,
            self.settings.search,
            self.source.display()
        );
        frame.render_widget(
            Paragraph::new(format!(
                "{}\n{}: {}",
                status,
                t("Log wrap"),
                t(if self.settings.wrap { "On" } else { "Off" })
            ))
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL)),
            parts[1],
        );
        match &self.settings.mode {
            Mode::Filter(input) | Mode::Search(input) | Mode::MemoryLimit(input) => {
                let label = match self.settings.mode {
                    Mode::Filter(_) => t("过滤：仅显示含关键词的行"),
                    Mode::Search(_) => t("搜索：高亮关键词，n/N 跳转"),
                    _ => t("In-memory log lines · 1–100000"),
                };
                let popup = Rect {
                    x: area.x + 1,
                    y: area.y + area.height / 3,
                    width: area.width.saturating_sub(2),
                    height: 5.min(area.height),
                };
                frame.render_widget(Clear, popup);
                frame.render_widget(
                    Paragraph::new(tr!("> {}▏\nEnter 应用 · Esc 取消 · Ctrl-U 清空", input))
                        .block(Block::default().title(label).borders(Borders::ALL)),
                    popup,
                );
            }
            Mode::DisplayLevel(selected) | Mode::CaptureLevel(selected) => {
                let display = matches!(self.settings.mode, Mode::DisplayLevel(_));
                let mut choices = Vec::new();
                if display {
                    choices.push(t("全部（包含无级别日志）").to_owned());
                }
                choices.extend(LogLevel::ALL.iter().map(|level| {
                    if display {
                        tr!("{} 及以上", level.as_str().to_uppercase())
                    } else {
                        level.as_str().to_uppercase()
                    }
                }));
                let popup = Rect {
                    x: area.x + 1,
                    y: area.y + area.height.saturating_sub(9) / 2,
                    width: area.width.saturating_sub(2),
                    height: 9.min(area.height),
                };
                frame.render_widget(Clear, popup);
                let mut state = ListState::default().with_selected(Some(*selected));
                frame.render_stateful_widget(
                    List::new(choices.into_iter().map(ListItem::new))
                        .highlight_symbol("› ")
                        .highlight_style(crate::theme::selected())
                        .block(
                            Block::default()
                                .title(if display {
                                    t("显示级别 · ↑↓ 选择 · Enter 应用")
                                } else {
                                    t("core 记录级别 · 即时生效 · Enter 应用")
                                })
                                .borders(Borders::ALL),
                        ),
                    popup,
                    &mut state,
                );
            }
            Mode::Browse => {}
        }
    }
}
fn detect_level(text: &str) -> Option<LogLevel> {
    text.split_whitespace().take(4).find_map(|word| {
        word.trim_matches(|c: char| !c.is_ascii_alphabetic())
            .parse()
            .ok()
    })
}
fn strip_ansi(text: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn wrapping_preserves_unicode_tail_navigation_search_and_settings() {
        let mut view = Viewer::new("test".into(), Settings::default());
        view.records.push_back(Record {
            text: format!("INFO {}TAIL", "中文長字串".repeat(12)),
            level: Some(LogLevel::Info),
        });
        view.records.push_back(Record {
            text: "INFO END".into(),
            level: Some(LogLevel::Info),
        });
        view.key(key('w'));
        let screen = view.test_screen(30, 24);
        assert!(
            screen.contains("TAIL") && screen.contains("INFO END"),
            "{screen}"
        );
        assert!(view.display_len() > view.visible().len());
        let total = view.display_len();
        view.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(
            view.top,
            total
                .saturating_sub(view.viewport_height.get())
                .saturating_sub(1)
        );
        view.settings.search = "END".into();
        view.jump(false);
        assert_eq!(view.top, view.row_count(&view.records[0]));
        let restored: Settings =
            serde_json::from_str(&serde_json::to_string(&view.settings).unwrap()).unwrap();
        assert!(restored.wrap);
        view.key(key('w'));
        assert!(!view.settings.wrap);
        assert_eq!(view.display_len(), 2);
        for width in [10, 30, 80] {
            view.test_screen(width, 12);
        }
    }
    #[test]
    fn vim_log_navigation_preserves_search_input_and_level_selection() {
        let mut view = Viewer::new("proxy".into(), Settings::default());
        for index in 0..30 {
            view.push(format!("INFO {index}"));
        }
        view.key(key('g'));
        view.key(key('g'));
        assert_eq!(view.top, 0);
        view.key(key('j'));
        assert_eq!(view.top, 1);
        view.key(key('k'));
        assert_eq!(view.top, 0);
        view.key(key('l'));
        assert_eq!(view.horizontal, 8);
        view.key(key('h'));
        assert_eq!(view.horizontal, 0);
        view.key(key('G'));
        assert!(view.settings.follow);
        view.key(key('/'));
        for ch in "hjklgg".chars() {
            view.key(key(ch));
        }
        assert!(matches!(&view.settings.mode,Mode::Search(text) if text=="hjklgg"));
        view.key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        view.key(key('v'));
        assert!(matches!(view.settings.mode, Mode::DisplayLevel(_)));
    }
    #[test]
    fn memory_limit_is_editable_persistent_and_releases_old_rows_immediately() {
        let mut view = Viewer::new("proxy".into(), Settings::default());
        for i in 0..100 {
            view.push(format!("INFO {i}"));
        }
        view.key(key('m'));
        view.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        view.paste("0");
        view.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(view.records.len(), 100);
        assert!(matches!(view.settings.mode, Mode::MemoryLimit(_)));
        view.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        view.paste("3");
        view.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(view.records.len(), 3);
        assert_eq!(view.dropped, 97);
        assert_eq!(view.records.front().unwrap().text, "INFO 97");
        let settings =
            serde_json::from_slice(&serde_json::to_vec(&view.settings).unwrap()).unwrap();
        let restored = Viewer::new("proxy".into(), settings);
        assert_eq!(restored.settings.memory_lines, 3);
        view.settings.memory_lines = 100_000;
        for _ in 0..100 {
            view.push("x".repeat(64 * 1024));
        }
        assert!(view.bytes <= 2 * 1024 * 1024);
        assert!(view.records.len() <= 32);
    }
    #[tokio::test]
    async fn real_log_stream_releases_rows_while_retaining_the_complete_local_file() {
        let fixture = crate::import::tests::Fixture::new();
        let text = (0..50)
            .map(|i| format!("INFO line {i}\n"))
            .collect::<String>();
        let path = fixture.write("proxy.log", &text);
        let socket = fixture.0.join("manager.sock");
        let listener = astra_ipc::platform::LocalListener::bind(&socket).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _: Request = astra_ipc::receive_management(&mut stream).await.unwrap();
            astra_ipc::send_management(&mut stream, &Reply::text("Streaming"))
                .await
                .unwrap();
            crate::log_stream::serve(stream, path, None).await;
        });
        let mut view = Viewer::new(
            "proxy".into(),
            Settings {
                memory_lines: 3,
                ..Default::default()
            },
        );
        view.connect(&socket).await.unwrap();
        for _ in 0..100 {
            view.poll();
            if view.dropped == 47 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(view.records.len(), 3);
        assert_eq!(view.dropped, 47);
        assert_eq!(std::fs::read_to_string(&view.source).unwrap(), text);
        view.stop();
        tokio::time::timeout(std::time::Duration::from_secs(2), server)
            .await
            .unwrap()
            .unwrap();
        astra_ipc::platform::remove_socket(&socket);
    }
    #[test]
    fn route_is_between_debug_and_info_in_filter_and_capture_picker() {
        let mut view = Viewer::new("proxy".into(), Settings::default());
        view.push("DEBUG debug-detail".into());
        view.push("2026-10-19T00:00:00Z ROUTE astra_route: forwarding started".into());
        view.push("INFO ready".into());
        assert_eq!(view.records[1].level, Some(LogLevel::Route));
        view.settings.min_level = Some(LogLevel::Route);
        assert_eq!(view.visible().len(), 2);
        view.settings.min_level = Some(LogLevel::Info);
        assert_eq!(view.visible().len(), 1);
        view.key(key('L'));
        view.key(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert!(matches!(
            view.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Command::Capture(LogLevel::Route)
        ));
        view.cancel_input();
        view.key(key('v'));
        for _ in 0..20 {
            view.key(KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
        }
        view.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        assert_eq!(view.settings.min_level, Some(LogLevel::Error));
    }
    #[test]
    fn pending_level_picker_migrates_without_changing_its_selection() {
        let old = r#"{"filter":"","search":"","min_level":"info","follow":true,"mode":{"CaptureLevel":2}}"#;
        let settings: Settings = serde_json::from_str(old).unwrap();
        let mut view = Viewer::new("proxy".into(), settings);
        assert!(matches!(
            view.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Command::Capture(LogLevel::Info)
        ));
    }
    fn event(bytes: &[u8], offset: u64, reset: bool) -> LogEvent {
        LogEvent::Data {
            bytes: bytes.to_vec(),
            cursor: LogCursor {
                device: 1,
                inode: 2,
                offset,
            },
            reset,
            skip_prefix: false,
        }
    }
    fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }
    #[test]
    fn fragmented_utf8_ansi_and_rotation_keep_valid_complete_lines() {
        let mut view = Viewer::new("test".into(), Settings::default());
        let text = "2026-10-18 INFO target: 中文\n".as_bytes();
        let split = text.len() - 3;
        view.apply(event(&text[..split], split as u64, true));
        assert!(view.records.is_empty());
        view.apply(event(&text[split..], text.len() as u64, false));
        assert_eq!(view.records[0].text, "2026-10-18 INFO target: 中文");
        view.apply(event(b"\x1b[31mERROR bad\x1b[0m\n", 100, false));
        assert_eq!(view.records[1].text, "ERROR bad");
        assert_eq!(view.records[1].level, Some(LogLevel::Error));
        view.apply(event(b"WARN rotated\n", 13, true));
        assert_eq!(view.records.back().unwrap().text, "WARN rotated");
        assert!(view.records.iter().any(|r| r.text.contains("轮换")));
    }
    #[test]
    fn search_filter_and_level_are_independent_and_follow_does_not_stop_ingestion() {
        let mut view = Viewer::new("test".into(), Settings::default());
        view.apply(event(
            "INFO alpha\nDEBUG beta ALPHA\nERROR 中文 Alpha\nplain stderr\n".as_bytes(),
            70,
            true,
        ));
        view.settings.filter = "aLpHa".into();
        assert_eq!(view.visible().len(), 3);
        view.settings.min_level = Some(LogLevel::Warn);
        assert_eq!(view.visible().len(), 1);
        assert!(view.visible()[0].text.contains("中文"));
        view.settings.filter.clear();
        assert_eq!(view.visible().len(), 2); // Unlevelled stderr remains visible.
        view.settings.min_level = None;
        view.settings.search = "beta".into();
        view.jump(false);
        assert_eq!(view.top, 1);
        assert!(!view.settings.follow);
        view.apply(event(b"INFO new\n", 80, false));
        assert_eq!(view.records.len(), 5);
        assert_eq!(view.top, 1);
        view.key(KeyEvent::new(KeyCode::End, KeyModifiers::NONE));
        assert!(view.settings.follow);
        view.key(key('L'));
        let Command::Capture(level) = view.key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
        else {
            panic!("capture choice")
        };
        assert_eq!(level, LogLevel::Info);
        assert!(view.settings.min_level.is_none());
    }
    #[test]
    fn bounded_memory_and_pending_search_survive_settings_restore() {
        let mut view = Viewer::new("test".into(), Settings::default());
        for i in 0..6000 {
            view.push(format!("INFO line {i}"));
        }
        assert_eq!(view.records.len(), 5000);
        assert_eq!(view.dropped, 1000);
        view.key(key('/'));
        view.paste("pending 中文 search");
        let settings: Settings =
            serde_json::from_slice(&serde_json::to_vec(&view.settings).unwrap()).unwrap();
        assert!(matches!(settings.mode,Mode::Search(input) if input=="pending 中文 search"));
        view.apply(event(&vec![b'x'; 80 * 1024], 100000, false));
        assert!(view.partial.len() <= 64 * 1024);
        assert!(view.records.back().unwrap().text.contains("已截断"));
    }
    #[test]
    fn log_view_renders_filter_status_and_level_picker() {
        use ratatui::{backend::TestBackend, Terminal};
        let mut view = Viewer::new("rules".into(), Settings::default());
        view.apply(event(b"INFO ready\nERROR failed\n", 30, true));
        view.settings.filter = "ready".into();
        view.settings.search = "INFO".into();
        let mut terminal = Terminal::new(TestBackend::new(100, 24)).unwrap();
        terminal
            .draw(|frame| view.render(frame, frame.area()))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.contains("INFO ready"));
        assert!(!text.contains("ERROR failed"));
        // Wide CJK cells include a trailing blank cell in TestBackend buffers.
        assert!(text.replace(' ', "").contains("过滤:ready"));
        assert!(text.replace(' ', "").contains("搜索:INFO"));
        view.key(key('L'));
        terminal
            .draw(|frame| view.render(frame, frame.area()))
            .unwrap();
        let text: String = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect();
        assert!(text.replace(' ', "").contains("记录级别"));
        assert!(text.contains("TRACE"));
        assert!(text.contains("ERROR"));
    }
}
