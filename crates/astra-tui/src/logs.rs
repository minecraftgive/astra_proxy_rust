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
#[serde(deny_unknown_fields)]
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
    #[serde(default)]
    vim_navigation: bool,
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
            vim_navigation: false,
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
#[derive(Clone)]
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
    capture_supported: bool,
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
    frozen: Option<(VecDeque<Record>, String)>,
    revision: u64,
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
            vim_g: false,
            frozen: None,
            revision: 0,
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
                        crate::ui::wake();
                        break;
                    }
                };
                let failed = matches!(event, LogEvent::Error(_));
                let sent = sender.send(event).await;
                crate::ui::wake();
                if sent.is_err() || failed {
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
    fn apply(&mut self, event: LogEvent) {
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
                if self.frozen.is_none() && self.matches(&line) {
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
        self.frozen
            .as_ref()
            .map_or(&self.records, |(records, _)| records)
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
    pub fn accepts_text_input(&self) -> bool {
        matches!(
            self.settings.mode,
            Mode::Filter(_) | Mode::Search(_) | Mode::MemoryLimit(_)
        )
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
        let height = parts[0]
            .height
            .saturating_sub(2 + u16::from(self.settings.follow)) as usize;
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
        let follow = if self.paused() {
            t("显示已暂停 · 后台仍更新 · Space 恢复")
        } else if self.settings.follow {
            t("跟随")
        } else {
            t("暂停跟随")
        };
        let display = self
            .settings
            .min_level
            .map(|l| format!("{}+", l.as_str()))
            .unwrap_or(t("全部").into());
        let title = if self.capture_supported {
            tr!(
                "实时日志 · {} · {} · 显示 {} · 记录 {}",
                self.name,
                follow,
                display,
                self.capture_level.as_str()
            )
        } else {
            format!(
                "{} · {} · {} · {} {}",
                t("Logs"),
                self.name,
                follow,
                t("Filter"),
                display
            )
        };
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
        if self.settings.follow && parts[0].height > 2 && parts[0].width > 2 {
            let marker = Rect::new(parts[0].x + 1, parts[0].bottom() - 2, parts[0].width - 2, 1);
            frame.render_widget(
                Block::default().style(Style::default().bg(Color::DarkGray)),
                marker,
            );
        }
        let status = tr!(
            "{}\n{}/{} lines · limit {} · released {}\n过滤: {}\n搜索: {}\nLocal log: {}",
            self.frozen.as_ref().map_or_else(
                || self.status.clone(),
                |(_, stamp)| format!("{} · 冻结于 {stamp} · 后台仍更新", self.status)
            ),
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
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title_bottom(Line::from(crate::theme::key_hints(t(
                        "End bottom / follow",
                    )))),
            ),
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
#[path = "test/logs_test.rs"]
mod tests;
