//! Installation is requested from the selected management target, never inferred
//! from the frontend's own filesystem. Queries do not enter the mutation queue.
use super::*;
use astra_management::v2 as v;
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

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
        let sections = Layout::default()
            .direction(Direction::Vertical)
            .constraints([
                Constraint::Length(3),
                Constraint::Min(1),
                Constraint::Length(2),
            ])
            .split(inner);
        let scope = self.client.installation.scope_label();
        let target = self.client.installation.target_label();
        frame.render_widget(
            Paragraph::new(format!(
                "{}: {target}\n{}: {}  [Tab / ←→]\n{}",
                t("Management target"),
                t("Installation scope"),
                t(scope),
                t("↑↓ package · Enter install · r refresh · PgUp/PgDn scroll · Esc close")
            )),
            sections[0],
        );
        self.hit(
            Rect::new(sections[0].x, sections[0].y + 1, sections[0].width, 1),
            dashboard::Target::Installation(Click::Scope),
        );
        let lines: Vec<Line> = self
            .client
            .installation
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
        frame.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .scroll((self.client.installation.scroll, 0)),
            sections[1],
        );
        for (index, package) in self
            .client
            .installation
            .status
            .iter()
            .flat_map(|s| &s.packages)
            .enumerate()
        {
            let _ = package;
            if self.client.installation.scroll == 0 && index < sections[1].height as usize {
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
        let width = sections[2].width / 3;
        for (i, (label, click)) in [
            ("Install", Click::Install),
            ("Refresh", Click::Refresh),
            ("Close", Click::Close),
        ]
        .into_iter()
        .enumerate()
        {
            self.button(
                frame,
                Rect::new(
                    sections[2].x + i as u16 * width,
                    sections[2].y,
                    width.saturating_sub(1),
                    1,
                ),
                label,
                dashboard::Target::Installation(click),
            );
        }
    }
}

/// Only a launch plan returned by the local private IPC is eligible for a
/// terminal/UAC prompt. A cancellation never runs the follow-up Execute.
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
    let launch = crate::protocol::request(
        socket,
        v::RequestBody::Local(v::LocalCommand::PrepareInstallation {
            package_id: request.package_id.clone(),
            expected_release_id: request.expected_release_id.clone(),
            scope: request.scope,
        }),
    )
    .await?;
    let v::ResponseBody::InstallationLaunch(launch) = launch else {
        anyhow::bail!("{}", t("Unexpected installation response"));
    };
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
