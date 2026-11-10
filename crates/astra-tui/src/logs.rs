use crate::protocol::*;
use crate::{i18n::t, tr};
pub use astra_client::logs::*;
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Style},
    text::Line,
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};
pub trait Render {
    #[cfg(test)]
    fn test_screen(&self, width: u16, height: u16) -> String;
    fn render(&self, frame: &mut Frame<'_>, area: Rect);
}
impl Render for Viewer {
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
    fn render(&self, frame: &mut Frame<'_>, area: Rect) {
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
            format!(
                "{} · {}{}",
                self.frozen.as_ref().map_or_else(
                    || self.status.clone(),
                    |(_, stamp)| format!("{} · 冻结于 {stamp} · 后台仍更新", self.status)
                ),
                self.session_label(),
                if self.sessions.len() > 1 { " · s" } else { "" }
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
            Mode::Session(selected) => {
                let choices = self.session_choices();
                let height = (choices.len() as u16 + 2).clamp(3, 12).min(area.height);
                let popup = Rect {
                    x: area.x + 1,
                    y: area.y + area.height.saturating_sub(height) / 2,
                    width: area.width.saturating_sub(2),
                    height,
                };
                frame.render_widget(Clear, popup);
                let mut state = ListState::default().with_selected(Some(*selected));
                frame.render_stateful_widget(
                    List::new(choices.into_iter().map(ListItem::new))
                        .highlight_symbol("› ")
                        .highlight_style(crate::theme::selected())
                        .block(
                            Block::default()
                                .title(t("日志会话 · ↑↓ 选择 · Enter 打开 · Esc 取消"))
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

#[cfg(test)]
#[path = "test/logs_test.rs"]
mod tests;
