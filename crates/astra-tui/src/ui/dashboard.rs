//! Terminal dashboard and shared mouse hit testing. Never synthesize telemetry.
use super::*;
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::{layout::Rect, style::Modifier, widgets::BorderType};

const BACKGROUND: Color = Color::Rgb(17, 17, 20);
const PANEL: Color = Color::Rgb(31, 32, 36);
pub(super) const BLUE: Color = Color::Rgb(47, 128, 255);
pub(super) const MUTED: Color = Color::Rgb(161, 168, 183);
pub(super) const PAGES: [&str; 5] = ["Dashboard", "Proxies", "Profiles", "Connections", "Tools"];

#[derive(Clone)]
pub(super) enum Target {
    TaskDetails,
    Page(usize),
    Row(usize),
    Action(Operation),
    Menu(usize),
    Binding(usize),
    Back,
    Add,
    Core,
    Runtime(astra_ipc::runtime::Command),
    Group(usize),
    Member(usize),
    Connection(usize),
}

pub(super) fn card(title: &str) -> Block<'_> {
    Block::default()
        .title(format!(" {} ", t(title)))
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
        .title_style(Style::default().fg(MUTED))
        .border_style(Style::default().fg(Color::Rgb(64, 68, 78)))
        .style(Style::default().bg(PANEL).fg(Color::Rgb(227, 231, 239)))
}

impl Ui {
    pub(super) fn overview_input(&self) -> bool {
        self.live_input() && self.runtime_picker.is_none()
    }

    pub(super) fn navigate(&mut self, page: usize) -> Input {
        self.page = page.min(PAGES.len() - 1);
        self.connection_filtering = false;
        self.tool_selection = 0;
        if matches!(self.page, 1 | 3) {
            self.kind = Kind::Proxy;
        }
        Input::Idle
    }

    pub(super) fn hit(&self, rect: Rect, target: Target) {
        if rect.width > 0 && rect.height > 0 {
            self.hits.borrow_mut().push((rect, target));
        }
    }

    pub(super) fn render_navigation(&self, f: &mut Frame<'_>) -> Rect {
        let area = f.area();
        f.render_widget(
            Block::default().style(Style::default().bg(BACKGROUND).fg(Color::White)),
            area,
        );
        // Editors and modal views retain their full terminal width.
        if !self.overview_input() && self.menu.is_none() && self.binding_picker.is_none() {
            return area;
        }
        if area.width < 90 {
            let sections =
                Layout::vertical([Constraint::Length(2), Constraint::Min(0)]).split(area);
            let tabs = Layout::horizontal([Constraint::Ratio(1, 5); 5]).split(sections[0]);
            for (index, rect) in tabs.iter().enumerate() {
                let label = format!("{} {}", index + 1, t(PAGES[index]));
                f.render_widget(
                    Paragraph::new(label).style(if self.page == index {
                        Style::default().bg(BLUE).fg(Color::White)
                    } else {
                        Style::default().fg(MUTED)
                    }),
                    *rect,
                );
                self.hit(*rect, Target::Page(index));
            }
            return sections[1];
        }
        let columns = Layout::horizontal([Constraint::Length(19), Constraint::Min(0)]).split(area);
        f.render_widget(
            Block::default()
                .borders(Borders::RIGHT)
                .border_style(Style::default().fg(Color::DarkGray))
                .style(Style::default().bg(PANEL)),
            columns[0],
        );
        f.render_widget(
            Paragraph::new(" ASTRA\n DUAL CORE").style(
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            ),
            Rect::new(area.x + 1, area.y + 1, 16, 2),
        );
        for (index, label) in PAGES.iter().enumerate() {
            let rect = Rect::new(area.x + 1, area.y + 5 + index as u16 * 2, 17, 1)
                .intersection(columns[0]);
            f.render_widget(
                Paragraph::new(format!(" {}  {}", index + 1, t(label))).style(
                    if self.page == index {
                        Style::default()
                            .bg(BLUE)
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(MUTED)
                    },
                ),
                rect,
            );
            self.hit(rect, Target::Page(index));
        }
        columns[1]
    }

    pub(super) fn button(&self, f: &mut Frame<'_>, area: Rect, label: &str, target: Target) {
        f.render_widget(
            Paragraph::new(format!(" {} ", t(label)))
                .style(Style::default().bg(BLUE).fg(Color::White)),
            area,
        );
        self.hit(area, target);
    }

    pub(super) fn header_hits(&self, f: &mut Frame<'_>, area: Rect) {
        if area.width < 14 || area.height < 3 {
            return;
        }
        let rect = Rect::new(area.right() - 13, area.y + 1, 12, 1);
        if self.editor.is_some() || self.viewer.is_some() {
            self.button(f, rect, "Esc back", Target::Back);
        } else if self.overview_input() {
            self.button(f, rect, "Tab core", Target::Core);
        }
    }

    pub(super) fn render_dashboard(&self, f: &mut Frame<'_>, area: Rect) {
        if area.height < 9 || area.width < 36 {
            f.render_widget(
                Paragraph::new(t("Enlarge terminal to view dashboard"))
                    .wrap(Wrap { trim: false })
                    .block(card("Dashboard")),
                area,
            );
            return;
        }
        if self.page == 4 {
            let ops = self.tool_operations();
            let items: Vec<_> = ops
                .iter()
                .map(|op| ListItem::new(format!("  {}", op.label())))
                .collect();
            let mut state = ListState::default().with_selected(if ops.is_empty() {
                None
            } else {
                Some(self.tool_selection.min(ops.len() - 1))
            });
            let title = format!(
                "{} · {}",
                t("Tools"),
                self.selected()
                    .map(|r| r.instance.name.as_str())
                    .unwrap_or("—")
            );
            f.render_stateful_widget(
                List::new(items)
                    .block(card(&title))
                    .highlight_style(Style::default().bg(BLUE))
                    .highlight_symbol("›"),
                area,
                &mut state,
            );
            for (index, op) in ops
                .iter()
                .skip(state.offset())
                .take(area.height.saturating_sub(2) as usize)
                .enumerate()
            {
                self.hit(
                    Rect::new(
                        area.x + 1,
                        area.y + 1 + index as u16,
                        area.width.saturating_sub(2),
                        1,
                    ),
                    Target::Action(*op),
                );
            }
            if ops.is_empty() {
                self.button(
                    f,
                    area.inner(ratatui::layout::Margin::new(2, 1)),
                    "n to add an instance",
                    Target::Add,
                );
            }
            return;
        }
        let bands = Layout::vertical([
            Constraint::Length(6),
            Constraint::Length(3),
            Constraint::Length(if self.live().is_some() { 3 } else { 0 }),
            Constraint::Min(0),
        ])
        .split(area);
        let stats = Layout::horizontal([
            Constraint::Percentage(50),
            Constraint::Percentage(25),
            Constraint::Percentage(25),
        ])
        .split(bands[0]);
        let selected = self.selected();
        let details = selected.and_then(|row| self.details.get(&row.instance.name));
        let running = self.rows.iter().filter(|row| row.pid.is_some()).count();
        let (name, state, uptime) = selected
            .map(|row| (row.instance.name.as_str(), t(&row.state), row.uptime))
            .unwrap_or(("—", t("No instance selected"), 0));
        let mut status = Text::from(vec![
            Line::styled(
                format!(" {name}   ·   {state}"),
                Style::default()
                    .fg(if selected.is_some_and(|r| r.pid.is_some()) {
                        Color::Green
                    } else {
                        MUTED
                    })
                    .add_modifier(Modifier::BOLD),
            ),
            Line::raw(format!(
                " {}  {:02}:{:02}:{:02}",
                t("Run time"),
                uptime / 3600,
                (uptime / 60) % 60,
                uptime % 60
            )),
            Line::raw(format!(
                " {}  {} / {}",
                t("Running instances"),
                running,
                self.rows.len()
            )),
        ]);
        if selected.is_some_and(|row| row.pid.is_some())
            && self.live().is_none()
            && !self.runtime_error.is_empty()
        {
            status.lines.push(Line::styled(
                &self.runtime_error,
                Style::default().fg(Color::Yellow),
            ));
        }
        f.render_widget(Paragraph::new(status).block(card("Status")), stats[0]);
        let proxies = self
            .rows
            .iter()
            .filter(|r| r.instance.kind == Kind::Proxy)
            .count();
        let rules = self.rows.len() - proxies;
        if self.live().is_some() {
            self.render_transfer(f, stats[1], 0);
            self.render_transfer(f, stats[2], 1);
        } else {
            f.render_widget(
                Paragraph::new(format!(" {proxies}\n {}", t("Proxy instances")))
                    .block(card("Proxies")),
                stats[1],
            );
            f.render_widget(
                Paragraph::new(format!(" {rules}\n {}", t("Rule instances"))).block(card("Rules")),
                stats[2],
            );
        }
        let actions = Layout::horizontal([Constraint::Ratio(1, 4); 4]).split(bands[1]);
        let power = if selected.is_some_and(|r| r.pid.is_some()) {
            Operation::Stop
        } else {
            Operation::Start
        };
        for (index, op) in [
            power,
            Operation::Edit,
            Operation::Logs,
            if self.kind == Kind::Rule {
                Operation::Explain
            } else {
                Operation::Diagnostics
            },
        ]
        .iter()
        .enumerate()
        {
            let rect = actions[index].inner(ratatui::layout::Margin::new(1, 1));
            self.button(f, rect, op.label(), Target::Action(*op));
        }
        self.render_runtime_modes(f, bands[2]);
        if bands[3].height == 0 {
            return;
        }
        let columns = if area.width >= 75 {
            Layout::horizontal([Constraint::Percentage(56), Constraint::Percentage(44)])
                .split(bands[3])
        } else {
            Layout::vertical([Constraint::Percentage(55), Constraint::Percentage(45)])
                .split(bands[3])
        };
        if self.live().is_some() {
            self.render_connections(f, columns[0]);
        } else {
            // Use a real stateful list so the mouse and keyboard share the scroll offset.
            let rows = self.rows();
            let items: Vec<_> = rows
                .iter()
                .map(|row| {
                    ListItem::new(Line::from(vec![
                        Span::styled(
                            if row.pid.is_some() { " ● " } else { " ○ " },
                            Style::default().fg(if row.pid.is_some() {
                                Color::Green
                            } else {
                                MUTED
                            }),
                        ),
                        Span::raw(format!("{}  ", row.instance.name)),
                        Span::styled(t(&row.state).to_owned(), Style::default().fg(MUTED)),
                    ]))
                })
                .collect();
            let mut list_state = ListState::default().with_selected(if rows.is_empty() {
                None
            } else {
                Some(self.selections[self.index()])
            });
            f.render_stateful_widget(
                List::new(items)
                    .block(card(if self.kind == Kind::Proxy {
                        "Proxy instances"
                    } else {
                        "Rule instances"
                    }))
                    .highlight_style(Style::default().bg(BLUE))
                    .highlight_symbol("›"),
                columns[0],
                &mut list_state,
            );
            for index in 0..columns[0].height.saturating_sub(2) as usize {
                if index + list_state.offset() >= rows.len() {
                    break;
                }
                self.hit(
                    Rect::new(
                        columns[0].x + 1,
                        columns[0].y + 1 + index as u16,
                        columns[0].width.saturating_sub(2),
                        1,
                    ),
                    Target::Row(index + list_state.offset()),
                );
            }
            if rows.is_empty() {
                self.button(
                    f,
                    columns[0].inner(ratatui::layout::Margin::new(2, 1)),
                    "n to add an instance",
                    Target::Add,
                );
            }
        }
        let right = Layout::vertical([Constraint::Percentage(40), Constraint::Percentage(60)])
            .split(columns[1]);
        let info = if let Some(row) = selected {
            format!(
                "PID {} · {}{}\n{}",
                row.pid
                    .map(|pid| pid.to_string())
                    .unwrap_or_else(|| "—".into()),
                details
                    .map(|d| d.log_level.as_str().to_uppercase())
                    .unwrap_or_else(|| "—".into()),
                details
                    .filter(|d| !d.bindings.is_empty())
                    .map(|d| format!(" → {}", d.bindings.join(" · ")))
                    .unwrap_or_default(),
                row.instance.config.display()
            )
        } else {
            t("n to add an instance").into()
        };
        f.render_widget(
            Paragraph::new(info)
                .wrap(Wrap { trim: false })
                .block(card("Profile")),
            right[0],
        );
        let summary = if let Some(d) = details {
            if d.config_unavailable {
                t("Configuration unavailable").into()
            } else {
                format!(
                    "{}\n{}\n{}: {}   {}: {}\n{}",
                    d.modes.iter().map(|m| t(m)).collect::<Vec<_>>().join(" · "),
                    d.listeners.join("\n"),
                    t("Outbounds"),
                    d.outbound_count,
                    t("Rules"),
                    d.rules,
                    d.outbounds.join(" · ")
                )
            }
        } else {
            t("No instance selected").into()
        };
        if self.live().is_some() {
            self.render_live_groups(f, right[1]);
        } else {
            f.render_widget(
                Paragraph::new(summary)
                    .wrap(Wrap { trim: false })
                    .block(card("Listeners / Outbounds")),
                right[1],
            );
        }
    }

    pub(super) fn list_hits(&self, area: Rect, offset: usize) {
        for visible in 0..area.height.saturating_sub(2) as usize / 2 {
            if offset + visible >= self.rows().len() {
                break;
            }
            self.hit(
                Rect::new(
                    area.x + 1,
                    area.y + 1 + visible as u16 * 2,
                    area.width.saturating_sub(2),
                    2,
                ),
                Target::Row(offset + visible),
            );
        }
    }

    pub(super) fn menu_hits(&self, area: Rect, offset: usize) {
        let count = self
            .binding_picker
            .as_ref()
            .map_or_else(|| self.operations().len(), |p| p.choices.len());
        for visible in 0..area.height.saturating_sub(2) as usize {
            let index = visible + offset;
            if index >= count {
                break;
            }
            self.hit(
                Rect::new(
                    area.x + 1,
                    area.y + 1 + visible as u16,
                    area.width.saturating_sub(2),
                    1,
                ),
                if self.binding_picker.is_some() {
                    Target::Binding(index)
                } else {
                    Target::Menu(index)
                },
            );
        }
    }

    pub(super) fn mouse(&mut self, mouse: MouseEvent) -> Input {
        if matches!(
            mouse.kind,
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
        ) {
            return self.key(event::KeyEvent::new(
                if mouse.kind == MouseEventKind::ScrollUp {
                    KeyCode::Up
                } else {
                    KeyCode::Down
                },
                KeyModifiers::NONE,
            ));
        }
        if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
            return Input::Idle;
        }
        if self.help_scroll.is_some()
            || self.prompt.is_some()
            || self.recovery.is_some()
            || self.diagnostics.is_some()
            || self.explanation.is_some()
        {
            return Input::Idle;
        }
        let target = self
            .hits
            .borrow()
            .iter()
            .rev()
            .find(|(rect, _)| rect.contains((mouse.column, mouse.row).into()))
            .map(|(_, target)| target.clone());
        match target {
            Some(Target::TaskDetails) => {
                self.key(event::KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE))
            }
            Some(Target::Runtime(command)) => self.runtime_action(command),
            Some(Target::Group(index)) => {
                self.group_selection = index;
                self.open_runtime_group();
                Input::Idle
            }
            Some(Target::Member(index)) => self.choose_runtime_member(index),
            Some(Target::Connection(index)) => {
                self.connection_selection = index;
                Input::Idle
            }
            Some(Target::Page(page)) if self.overview_input() => self.navigate(page),
            Some(Target::Row(index)) => {
                let slot = self.index();
                self.selections[slot] = index;
                self.menu = Some(0);
                Input::Idle
            }
            Some(Target::Action(op)) => self.operation(op),
            Some(Target::Menu(index)) => {
                self.menu = None;
                if let Some(op) = self.operations().get(index).copied() {
                    self.operation(op)
                } else {
                    Input::Idle
                }
            }
            Some(Target::Binding(index)) => {
                if let Some(picker) = &mut self.binding_picker {
                    picker.selected = index;
                }
                self.key(event::KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE))
            }
            Some(Target::Back) => self.key(event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Some(Target::Add) => {
                self.prompt(Prompt::AddName);
                Input::Idle
            }
            Some(Target::Core) => self.key(event::KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE)),
            _ => Input::Idle,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    fn sample() -> Ui {
        let mut ui = Ui::new(Session::default());
        ui.rows = (0..24)
            .map(|index| Row {
                instance: Instance {
                    name: format!("proxy-{index:02}"),
                    kind: Kind::Proxy,
                    config: format!("/profiles/proxy-{index:02}.toml").into(),
                },
                pid: Some(100 + index),
                state: "Running".into(),
                uptime: 3723,
                changed: false,
            })
            .collect();
        ui.details.insert(
            "proxy-00".into(),
            InstanceDetails {
                modes: vec!["Rules".into(), "System proxy".into()],
                listeners: vec!["SOCKS5 127.0.0.1:7890".into()],
                outbounds: vec!["Tokyo [select]".into(), "Singapore [url-test]".into()],
                outbound_count: 12,
                rules: 114,
                diagnostics_supported: true,
                ..InstanceDetails::default()
            },
        );
        ui
    }
    fn click(rect: Rect) -> MouseEvent {
        MouseEvent {
            kind: MouseEventKind::Down(MouseButton::Left),
            column: rect.x,
            row: rect.y,
            modifiers: KeyModifiers::NONE,
        }
    }
    fn render(ui: &Ui, width: u16, height: u16) -> Terminal<TestBackend> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|f| ui.render(f)).unwrap();
        terminal
    }
    #[test]
    fn mouse_navigation_actions_scrolling_and_modal_capture() {
        let mut ui = sample();
        render(&ui, 120, 36);
        let rect = ui
            .hits
            .borrow()
            .iter()
            .find(|(_, hit)| matches!(hit, Target::Page(1)))
            .unwrap()
            .0;
        ui.mouse(click(rect));
        assert_eq!(ui.page, 1);
        render(&ui, 120, 36);
        let rect = ui
            .hits
            .borrow()
            .iter()
            .find(|(_, hit)| matches!(hit, Target::Row(1)))
            .unwrap()
            .0;
        ui.mouse(click(rect));
        assert_eq!(ui.selections[0], 1);
        assert_eq!(ui.menu, Some(0));
        render(&ui, 120, 36);
        assert!(ui
            .hits
            .borrow()
            .iter()
            .all(|(_, hit)| matches!(hit, Target::Menu(_))));
        let rect = ui.hits.borrow()[0].0;
        assert!(
            matches!(ui.mouse(click(rect)), Input::Run(Action::Stop(name), _) if name == "proxy-01")
        );
        ui.menu = None;
        for _ in 0..20 {
            ui.mouse(MouseEvent {
                kind: MouseEventKind::ScrollDown,
                column: 50,
                row: 15,
                modifiers: KeyModifiers::NONE,
            });
        }
        assert_eq!(ui.selections[0], 21);
        render(&ui, 120, 36);
        assert!(ui
            .hits
            .borrow()
            .iter()
            .any(|(_, hit)| matches!(hit, Target::Row(21))));
        ui.prompt(Prompt::Shutdown);
        let before = ui.page;
        ui.mouse(click(Rect::new(1, 5, 1, 1)));
        assert_eq!(ui.page, before);
        assert!(ui.prompt.is_some());
    }
    #[test]
    fn dashboard_resizes_and_tools_have_keyboard_focus() {
        crate::i18n::with_language(crate::i18n::Language::English, || {
            let mut ui = sample();
            let terminal = render(&ui, 140, 40);
            let text: String = terminal
                .backend()
                .buffer()
                .content
                .iter()
                .map(|cell| cell.symbol())
                .collect();
            for value in [
                "Dashboard",
                "Status",
                "proxy-00",
                "SOCKS5 127.0.0.1:7890",
                "01:02:03",
            ] {
                assert!(text.contains(value), "{value}");
            }
            for (width, height) in [(1, 1), (20, 6), (60, 20), (90, 24)] {
                render(&ui, width, height);
            }
            ui.key(event::KeyEvent::new(KeyCode::Char('5'), KeyModifiers::NONE));
            ui.key(event::KeyEvent::new(KeyCode::Down, KeyModifiers::NONE));
            assert_eq!(ui.tool_selection, 1);
            assert!(matches!(
                ui.key(event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
                Input::Run(Action::Diagnostics(_), _)
            ));
            if std::env::var_os("ASTRA_TUI_PREVIEW").is_some() {
                ui.page = 0;
                let terminal = render(&ui, 140, 40);
                let cells: Vec<_> = terminal.backend().buffer().content.iter().map(|cell| serde_json::json!({"text":cell.symbol(),"fg":format!("{:?}",cell.fg),"bg":format!("{:?}",cell.bg)})).collect();
                std::fs::write(
                    concat!(env!("CARGO_MANIFEST_DIR"), "/../../target/tui-preview.json"),
                    serde_json::to_vec(&cells).unwrap(),
                )
                .unwrap();
            }
        });
    }
}
