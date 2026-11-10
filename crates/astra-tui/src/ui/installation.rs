//! Local frontend packages and manager-discovered packages share one installation view.
use super::*;
#[cfg(test)]
use astra_management::v2 as v;
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::{layout::Rect, style::Modifier};

pub use astra_client::installation::{InstallationAction, Request};
#[derive(Clone)]
pub(super) enum Click {
    Open,
    Later,
    Close,
    Row(usize),
    Scope,
    Refresh,
    Install,
    LaunchNow,
    ToggleAutoLaunch,
}

impl Ui {
    pub(super) async fn poll_installation(&mut self, socket: &std::path::Path) -> bool {
        self.client
            .poll_installation(socket, self.palette.is_none())
            .await
    }
    pub(super) fn installation_key(&mut self, key: event::KeyEvent) -> Input {
        if key.kind == KeyEventKind::Repeat
            && !matches!(
                key.code,
                KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown
            )
        {
            return Input::Idle;
        }
        let action = match key.code {
            KeyCode::Tab | KeyCode::Left | KeyCode::Right => InstallationAction::ToggleChoice,
            KeyCode::Enter => InstallationAction::Confirm,
            KeyCode::Esc => InstallationAction::Dismiss,
            KeyCode::Up => InstallationAction::Previous,
            KeyCode::Down => InstallationAction::Next,
            KeyCode::PageUp => InstallationAction::PageUp,
            KeyCode::PageDown => InstallationAction::PageDown,
            KeyCode::Char('r') => InstallationAction::Refresh,
            KeyCode::Char('m') => InstallationAction::RetryMaintenance,
            KeyCode::Char('b') => InstallationAction::RestorePrevious,
            KeyCode::Char('v') => InstallationAction::MaintenanceLog,
            KeyCode::Char('c') => InstallationAction::CancelMaintenance,
            KeyCode::Char('l') => InstallationAction::LaunchNow,
            KeyCode::Char('a') => InstallationAction::ToggleAutoLaunch,
            _ => return Input::Idle,
        };
        self.client.installation_action(action)
    }
    pub(super) fn installation_mouse(&mut self, mouse: MouseEvent) -> Input {
        let action = match mouse.kind {
            MouseEventKind::ScrollUp => InstallationAction::PageUp,
            MouseEventKind::ScrollDown => InstallationAction::PageDown,
            MouseEventKind::Down(MouseButton::Left) => {
                let hit = self.hits.borrow().hit(mouse.column, mouse.row).cloned();
                match hit {
                    Some(dashboard::Target::Installation(Click::Open)) => InstallationAction::Open,
                    Some(dashboard::Target::Installation(Click::Later | Click::Close)) => {
                        InstallationAction::Dismiss
                    }
                    Some(dashboard::Target::Installation(Click::Row(index))) => {
                        InstallationAction::Select(index)
                    }
                    Some(dashboard::Target::Installation(Click::Scope)) => {
                        InstallationAction::ToggleChoice
                    }
                    Some(dashboard::Target::Installation(Click::Refresh)) => {
                        InstallationAction::Refresh
                    }
                    Some(dashboard::Target::Installation(Click::Install)) => {
                        InstallationAction::Confirm
                    }
                    Some(dashboard::Target::Installation(Click::LaunchNow)) => {
                        InstallationAction::LaunchNow
                    }
                    Some(dashboard::Target::Installation(Click::ToggleAutoLaunch)) => {
                        InstallationAction::ToggleAutoLaunch
                    }
                    _ => return Input::Idle,
                }
            }
            _ => return Input::Idle,
        };
        self.client.installation_action(action)
    }
    pub(super) fn render_installation(&self, frame: &mut Frame<'_>) {
        if !self.client.installation.visible() {
            return;
        }
        self.hits.borrow_mut().clear();
        let area = frame.area();
        frame.render_widget(Clear, area);
        let block = dashboard::card(if self.client.installation.notice {
            "Install Astra"
        } else {
            "Installation"
        });
        let inner = block.inner(area);
        frame.render_widget(block, area);
        if inner.width < 8 || inner.height < 4 {
            return;
        }
        if self.client.installation.notice {
            let text = self.client.installation.notice_text();
            frame.render_widget(
                Paragraph::new(text).wrap(Wrap { trim: false }),
                Rect {
                    height: inner.height.saturating_sub(3),
                    ..inner
                },
            );
            let y = inner.y + inner.height - 2;
            for (i, (label, click)) in
                [("Go to installation", Click::Open), ("Later", Click::Later)]
                    .into_iter()
                    .enumerate()
            {
                let width = inner.width / 2;
                let rect = Rect::new(inner.x + i as u16 * width, y, width, 1);
                let chosen = self.client.installation.notice_accept == (i == 0);
                frame.render_widget(
                    Paragraph::new(format!("{} {}", if chosen { "›" } else { " " }, t(label)))
                        .style(Style::default().fg(if chosen {
                            Color::Cyan
                        } else {
                            Color::White
                        })),
                    rect,
                );
                self.hit(rect, dashboard::Target::Installation(click));
            }
            return;
        }
        let installation = &self.client.installation;
        let maintenance = &self.client.maintenance;
        let maintenance_label = maintenance.label();
        let show_maintenance = maintenance.active()
            || maintenance.failed()
            || !maintenance_label.is_empty()
            || maintenance.log.is_some();
        let progress_height = if installation.progress_visible() {
            4
        } else {
            0
        };
        let extra_height = 1 + u16::from(show_maintenance);
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(2),
                Constraint::Min(1),
                Constraint::Length(progress_height),
                Constraint::Length(extra_height),
                Constraint::Length(2),
            ])
            .split(inner);
        let muted = Style::default().fg(dashboard::MUTED);
        frame.render_widget(
            Paragraph::new(vec![
                Line::from(vec![
                    Span::styled(format!("{}: ", t("Management target")), muted),
                    Span::raw(installation.target_label()),
                    Span::raw("   "),
                    Span::styled(format!("{}: ", t("Installation scope")), muted),
                    Span::styled(installation.scope_label(), Style::default().fg(Color::Cyan)),
                    Span::styled("  [Tab]", muted),
                ]),
                Line::styled(
                    t("↑↓ package · Enter install · r refresh · PgUp/PgDn scroll · Esc close"),
                    muted,
                ),
            ]),
            sections[0],
        );
        self.hit(
            Rect::new(sections[0].x, sections[0].y, sections[0].width, 1),
            dashboard::Target::Installation(Click::Scope),
        );
        let mut lines: Vec<Line> = installation
            .details()
            .into_iter()
            .map(|detail| {
                use astra_client::installation::DetailKind;
                match detail.kind {
                    DetailKind::Package { selected, .. } => Line::styled(
                        astra_tui_kit::text::truncate_cells(
                            &format!("{} {}", if selected { "›" } else { " " }, detail.text),
                            sections[1].width as usize,
                            "…",
                        ),
                        Style::default().fg(if selected { Color::Cyan } else { Color::White }),
                    ),
                    DetailKind::Plain => Line::from(detail.text),
                    DetailKind::Success => {
                        Line::styled(detail.text, Style::default().fg(Color::Green))
                    }
                    DetailKind::Error => Line::styled(detail.text, Style::default().fg(Color::Red)),
                }
            })
            .collect();
        if let Some(log) = &maintenance.log {
            lines.extend(log.lines().map(|line| Line::styled(line.to_owned(), muted)));
        }
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((installation.scroll, 0)),
            sections[1],
        );
        for (index, _) in installation
            .status
            .iter()
            .flat_map(|s| &s.packages)
            .enumerate()
        {
            if installation.scroll == 0 && index < sections[1].height as usize {
                self.hit(
                    Rect::new(
                        sections[1].x,
                        sections[1].y + index as u16,
                        sections[1].width,
                        1,
                    ),
                    dashboard::Target::Installation(Click::Row(index)),
                );
            }
        }
        if progress_height > 0 {
            self.render_installation_progress(frame, sections[2]);
        }
        let mut extra = vec![Line::from(vec![
            Span::raw(self.client.update_launch.label()),
            Span::styled("  [l / a]", muted),
        ])];
        if show_maintenance {
            extra.push(Line::from(vec![
                Span::styled(maintenance_label, Style::default().fg(Color::Yellow)),
                Span::styled(
                    format!(
                        "  {}",
                        t("Maintenance: m retry · b previous · v log · c cancel")
                    ),
                    muted,
                ),
            ]));
        }
        frame.render_widget(Paragraph::new(extra), sections[3]);
        self.hit(
            Rect::new(sections[3].x, sections[3].y, sections[3].width, 1),
            dashboard::Target::Installation(Click::ToggleAutoLaunch),
        );
        let busy = installation.busy();
        let mut buttons = vec![(
            if busy { "Installing…" } else { "Install" },
            Click::Install,
            installation.can_install(),
        )];
        if self.client.update_launch.pending() {
            buttons.push(("Launch now", Click::LaunchNow, true));
        }
        buttons.push(("Refresh", Click::Refresh, !busy));
        buttons.push(("Close", Click::Close, true));
        let width = sections[4].width / buttons.len() as u16;
        for (i, (label, click, enabled)) in buttons.into_iter().enumerate() {
            self.button_enabled(
                frame,
                Rect::new(
                    sections[4].x + i as u16 * width,
                    sections[4].y,
                    width.saturating_sub(1),
                    1,
                ),
                label,
                dashboard::Target::Installation(click),
                enabled,
            );
        }
    }

    /// Gauge plus a step checklist; kept outside the scrolled details so the
    /// current step is always visible.
    fn render_installation_progress(&self, frame: &mut Frame<'_>, area: Rect) {
        use astra_client::installation::StepState;
        let installation = &self.client.installation;
        if area.height == 0 || area.width == 0 {
            return;
        }
        let failed = installation.error.is_some() && !installation.busy();
        let title = Line::from(vec![
            Span::styled(
                t("Installation progress"),
                Style::default().add_modifier(Modifier::BOLD),
            ),
            Span::raw(" · "),
            Span::raw(installation.progress_label()),
        ]);
        frame.render_widget(Paragraph::new(title), Rect { height: 1, ..area });
        if area.height < 2 {
            return;
        }
        let percent = installation.percent();
        let color = if failed {
            Color::Red
        } else if percent == Some(100) {
            Color::Green
        } else {
            Color::Cyan
        };
        frame.render_widget(
            ratatui::widgets::Gauge::default()
                .gauge_style(Style::default().fg(color).bg(Color::DarkGray))
                .ratio(f64::from(percent.unwrap_or(0)) / 100.0)
                .label(percent.map_or_else(|| "…".to_owned(), |p| format!("{p}%"))),
            Rect {
                y: area.y + 1,
                height: 1,
                ..area
            },
        );
        if area.height < 3 {
            return;
        }
        // Pack whole steps per line so a mark never wraps away from its label.
        let width = area.width as usize;
        let mut lines = vec![Vec::new()];
        let mut used = 0;
        for row in installation.step_rows() {
            let (mark, style) = match row.state {
                StepState::Done => ("✓", Style::default().fg(Color::Green)),
                StepState::Active => (
                    "▶",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                StepState::Failed => ("✗", Style::default().fg(Color::Red)),
                StepState::Pending => ("·", Style::default().fg(dashboard::MUTED)),
            };
            let item = format!("{mark} {}", row.label);
            let cells = astra_tui_kit::text::width(&item);
            let gap = if used == 0 { 0 } else { 3 };
            if used > 0 && used + gap + cells > width {
                lines.push(Vec::new());
                used = 0;
            } else if gap > 0 {
                lines.last_mut().unwrap().push(Span::raw("   "));
                used += gap;
            }
            used += cells;
            lines.last_mut().unwrap().push(Span::styled(
                astra_tui_kit::text::truncate_cells(&item, width, "…"),
                style,
            ));
        }
        frame.render_widget(
            Paragraph::new(lines.into_iter().map(Line::from).collect::<Vec<_>>()),
            Rect {
                y: area.y + 2,
                height: area.height - 2,
                ..area
            },
        );
    }
}

/// Launch only the verified frontend package helper or a plan from local private IPC.
/// A cancellation never runs the follow-up installation.
pub(super) async fn elevate(
    socket: &std::path::Path,
    request: &Request,
    screen: &mut TerminalSession,
) -> Result<()> {
    anyhow::ensure!(
        crate::protocol::target().is_none_or(|(_, agent)| agent.is_none()),
        "{}",
        t("Run installation on the target machine.")
    );
    let launch = astra_client::installation::prepare_elevation(socket, request).await?;
    #[cfg(unix)]
    {
        let sudo = ["/usr/bin/sudo", "/bin/sudo", "/usr/local/bin/sudo"].into_iter()
            .find_map(|path| astra_platform::unix_handoff::ensure_root_controlled(std::path::Path::new(path)).ok())
            .context(t("sudo is unavailable. Run installation on the target machine with administrator permission."))?;
        screen.suspend()?;
        let _interrupt = IgnoreInterrupt::install();
        let result = std::process::Command::new(sudo)
            .arg(&launch.program)
            .args(&launch.arguments)
            .status();
        screen.resume()?;
        anyhow::ensure!(
            result?.success(),
            "{}",
            t("Installation was cancelled or administrator permission was not granted.")
        );
    }
    #[cfg(windows)]
    {
        let _ = screen;
        tokio::task::spawn_blocking(move || {
            crate::windows::installation::elevate(&launch.program, &launch.arguments)
        })
        .await??;
    }
    Ok(())
}

#[cfg(test)]
#[path = "../test/ui_installation_test.rs"]
mod tests;
