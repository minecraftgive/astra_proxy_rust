use crate::protocol::*;
use anyhow::Result;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::Line,
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
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
}
#[derive(Clone, Serialize, Deserialize)]
pub struct Settings {
    filter: String,
    search: String,
    min_level: Option<LogLevel>,
    follow: bool,
    mode: Mode,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            filter: String::new(),
            search: String::new(),
            min_level: None,
            follow: true,
            mode: Mode::Browse,
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
}
impl Viewer {
    pub fn new(name: String, settings: Settings) -> Self {
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
            status: "连接中".into(),
            viewport_height: Cell::new(10),
        }
    }
    pub async fn connect(&mut self, socket: &Path) -> Result<()> {
        let mut stream = crate::log_stream::connect(socket, self.name.clone(), self.cursor).await?;
        let (sender, receiver) = mpsc::channel(16);
        let task = tokio::spawn(async move {
            loop {
                let event = match astra_ipc::receive_management::<LogEvent>(&mut stream).await {
                    Ok(event) => event,
                    Err(error) => {
                        let _ = sender
                            .send(LogEvent::Error(format!("Log connection closed: {error}")))
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
        self.status = "实时连接".into();
        Ok(())
    }
    pub fn stop(&mut self) {
        self.feed = None;
        self.status = "已停止订阅；r 重连".into();
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
            Mode::Filter(input) | Mode::Search(input) => {
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
            return Some("Log subscription disconnected; continue to reconnect".into());
        }
        None
    }
    fn apply(&mut self, event: LogEvent) {
        match event {
            LogEvent::Waiting => self.status = "等待实例产生日志（订阅保持连接）".into(),
            LogEvent::Data {
                bytes,
                cursor,
                reset,
                skip_prefix,
            } => {
                if reset {
                    if self.cursor.is_some() {
                        self.push("── 日志已轮换或截断，继续接收 ──".into());
                    }
                    self.partial.clear();
                    self.skip_line = skip_prefix;
                }
                self.cursor = Some(cursor);
                self.status = "实时连接".into();
                self.partial.extend(bytes);
                while let Some(end) = self.partial.iter().position(|b| *b == b'\n') {
                    let mut line: Vec<_> = self.partial.drain(..=end).collect();
                    line.pop();
                    if self.skip_line {
                        self.skip_line = false;
                    } else {
                        if line.len() > 64 * 1024 {
                            line.truncate(64 * 1024);
                            line.extend_from_slice(" [超长行已截断]".as_bytes());
                        }
                        self.push(strip_ansi(&String::from_utf8_lossy(&line)));
                    }
                }
                if self.skip_line {
                    self.partial.clear();
                } else if self.partial.len() > 64 * 1024 {
                    let prefix = self.partial.drain(..64 * 1024).collect::<Vec<_>>();
                    self.push(format!(
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
        while self.records.len() > 5000 || self.bytes > 2 * 1024 * 1024 {
            if let Some(line) = self.records.pop_front() {
                self.bytes -= line.text.len();
                self.dropped += 1;
                if self.matches(&line) {
                    self.top = self.top.saturating_sub(1);
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
    pub fn key(&mut self, key: KeyEvent) -> Command {
        match &mut self.settings.mode {
            Mode::Filter(input) | Mode::Search(input) => {
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
                let max = if display { 5 } else { 4 };
                match key.code {
                    KeyCode::Esc => self.cancel_input(),
                    KeyCode::Up => *selected = selected.saturating_sub(1),
                    KeyCode::Down => *selected = (*selected + 1).min(max),
                    KeyCode::Enter => {
                        let selected = *selected;
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
        match key.code {
            KeyCode::Esc => return Command::Close,
            KeyCode::Char('r') => return Command::Reconnect,
            KeyCode::Char('/') => self.settings.mode = Mode::Search(self.settings.search.clone()),
            KeyCode::Char('f') => self.settings.mode = Mode::Filter(self.settings.filter.clone()),
            KeyCode::Char('l') => {
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
                        .visible()
                        .len()
                        .saturating_sub(self.viewport_height.get());
                }
                self.settings.follow = !self.settings.follow;
            }
            KeyCode::Up | KeyCode::PageUp => {
                if self.settings.follow {
                    self.top = self
                        .visible()
                        .len()
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
                        .visible()
                        .len()
                        .saturating_sub(self.viewport_height.get());
                }
                self.settings.follow = false;
                self.top = (self.top + if key.code == KeyCode::Down { 1 } else { 10 })
                    .min(self.visible().len().saturating_sub(1));
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
            self.top.min(lines.len() - 1)
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
            self.top = found;
            self.settings.follow = false;
        }
    }
    pub fn render(&self, frame: &mut Frame<'_>, area: Rect) {
        let parts = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(2), Constraint::Length(4)])
            .split(area);
        let visible = self.visible();
        let height = parts[0].height.saturating_sub(2) as usize;
        self.viewport_height.set(height.max(1));
        let top = if self.settings.follow {
            visible.len().saturating_sub(height)
        } else {
            self.top.min(visible.len().saturating_sub(1))
        };
        let lines: Vec<Line> = visible
            .iter()
            .skip(top)
            .take(height)
            .map(|line| {
                let color = match line.level {
                    Some(LogLevel::Error) => Color::Red,
                    Some(LogLevel::Warn) => Color::Yellow,
                    Some(LogLevel::Debug | LogLevel::Trace) => Color::Gray,
                    _ => Color::White,
                };
                let highlight = !self.settings.search.is_empty()
                    && line
                        .text
                        .to_lowercase()
                        .contains(&self.settings.search.to_lowercase());
                Line::styled(
                    line.text.clone(),
                    if highlight {
                        Style::default().fg(Color::Black).bg(Color::Yellow)
                    } else {
                        Style::default().fg(color)
                    },
                )
            })
            .collect();
        let title = format!(
            "实时日志 · {} · {} · 显示 {} · 记录 {}",
            self.name,
            if self.settings.follow {
                "跟随"
            } else {
                "暂停跟随"
            },
            self.settings
                .min_level
                .map(|l| format!("{}+", l.as_str()))
                .unwrap_or("全部".into()),
            self.capture_level.as_str()
        );
        frame.render_widget(
            Paragraph::new(lines)
                .scroll((0, self.horizontal))
                .block(Block::default().title(title).borders(Borders::ALL)),
            parts[0],
        );
        let status = format!(
            "{} · 显示 {}/{} 行 · 缓冲淘汰 {} 行\n过滤: {}  搜索: {}",
            self.status,
            visible.len(),
            self.records.len(),
            self.dropped,
            self.settings.filter,
            self.settings.search
        );
        frame.render_widget(
            Paragraph::new(status).block(Block::default().borders(Borders::ALL)),
            parts[1],
        );
        match &self.settings.mode {
            Mode::Filter(input) | Mode::Search(input) => {
                let label = if matches!(self.settings.mode, Mode::Filter(_)) {
                    "过滤：仅显示含关键词的行"
                } else {
                    "搜索：高亮关键词，n/N 跳转"
                };
                let popup = Rect {
                    x: area.x + 1,
                    y: area.y + area.height / 3,
                    width: area.width.saturating_sub(2),
                    height: 5.min(area.height),
                };
                frame.render_widget(Clear, popup);
                frame.render_widget(
                    Paragraph::new(format!("> {input}▏\nEnter 应用 · Esc 取消 · Ctrl-U 清空"))
                        .block(Block::default().title(label).borders(Borders::ALL)),
                    popup,
                );
            }
            Mode::DisplayLevel(selected) | Mode::CaptureLevel(selected) => {
                let display = matches!(self.settings.mode, Mode::DisplayLevel(_));
                let mut choices = Vec::new();
                if display {
                    choices.push("全部（包含无级别日志）".to_owned());
                }
                choices.extend(LogLevel::ALL.iter().map(|level| {
                    if display {
                        format!("{} 及以上", level.as_str().to_uppercase())
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
                        .highlight_style(Style::default().bg(Color::Blue))
                        .block(
                            Block::default()
                                .title(if display {
                                    "显示级别 · ↑↓ 选择 · Enter 应用"
                                } else {
                                    "core 记录级别 · 即时生效 · Enter 应用"
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
        assert!(text.replace(' ', "").contains("过滤:ready搜索:INFO"));
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
        assert!(text.replace(' ', "").contains("core记录级别"));
        assert!(text.contains("TRACE"));
        assert!(text.contains("ERROR"));
    }
}
