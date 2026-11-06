//! Installation is requested from the selected management target, never inferred
//! from the frontend's own filesystem. Queries do not enter the mutation queue.
use super::*;
use astra_management::v2 as v;
use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;
use tokio::sync::watch;

pub use astra_client::installation::Request;
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

fn notice_key(executable: &std::path::Path, socket: &std::path::Path) -> String {
    use sha2::Digest;
    let kind = if executable
        .file_stem()
        .is_some_and(|name| name == "astra-server-tui")
    {
        "server"
    } else {
        "client"
    };
    let (endpoint, agent) = crate::protocol::target().unwrap_or_else(|| (socket.into(), None));
    format!(
        "{kind}:{:x}",
        sha2::Sha256::digest(format!(
            "{}\0{}",
            endpoint.display(),
            agent.unwrap_or_default()
        ))
    )
}

async fn read_status(socket: PathBuf) -> Result<v::InstallationStatus, String> {
    let result = async {
        let hello =
            crate::protocol::request(&socket, v::RequestBody::Query(v::Query::Hello)).await?;
        anyhow::ensure!(
            matches!(hello, v::ResponseBody::Hello(info) if info.capabilities.iter().any(|c| c == "installation")),
            "{}",
            t("This target does not support installation yet.")
        );
        match crate::protocol::request(&socket, v::RequestBody::Query(v::Query::InstallationStatus))
            .await?
        {
            v::ResponseBody::InstallationStatus(status) => Ok(status),
            _ => anyhow::bail!("{}", t("Unexpected installation response")),
        }
    };
    tokio::time::timeout(Duration::from_secs(60), result)
        .await
        .map_err(|_| t("Installation status timed out. Press r to retry.").to_owned())?
        .map_err(|error: anyhow::Error| format!("{error:#}"))
}

impl Ui {
    pub(super) fn installation_failed(&mut self, error: String) {
        self.client.installation.error = Some(error.clone());
        self.client.message = error;
        self.client.tasks.notice_result(&self.client.message, true);
    }
    pub(super) fn initialize_installation(
        &mut self,
        context: &crate::windows::Context,
        socket: &std::path::Path,
        launch: &crate::windows::Launch,
        resumed: bool,
    ) {
        if launch.window_id.is_some() || resumed {
            return;
        }
        let key = notice_key(&context.executable, socket);
        if !self.client.preferences.installation_notices.contains(&key) {
            self.client.installation.notice_key = Some(key);
            self.client.installation.refresh = true;
        }
    }

    pub(super) fn open_installation(&mut self) {
        if let Some(key) = self.client.installation.notice_key.take() {
            self.change_preferences(|preferences| {
                preferences.installation_notices.insert(key);
            });
        }
        self.client.installation.notice = false;
        self.client.installation.open = true;
        self.client.installation.refresh = true;
        self.client.installation.scroll = 0;
    }

    pub(super) async fn poll_installation(&mut self, socket: &std::path::Path) -> bool {
        let mut changed = false;
        if let Some(progress) = &mut self.client.installation.progress {
            if progress.has_changed().unwrap_or(false) {
                if let Some(event) = progress.borrow_and_update().clone() {
                    self.client.installation.events.push(format!(
                        "{} · {}",
                        t(&event.phase),
                        event.detail
                    ));
                    if self.client.installation.events.len() > 64 {
                        self.client.installation.events.remove(0);
                    }
                    changed = true;
                }
            }
        }
        if self
            .client
            .installation
            .operation
            .as_ref()
            .is_some_and(|task| task.is_finished())
        {
            let result = self
                .client
                .installation
                .operation
                .take()
                .unwrap()
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result);
            self.client.installation.progress = None;
            match result {
                Ok(result) => {
                    self.client.message = result.message.clone();
                    self.client.installation.result = Some(result);
                    self.client.installation.error = None;
                    self.client.tasks.notice_result(&self.client.message, false);
                }
                Err(error) => {
                    self.client.message = error.clone();
                    self.client.installation.error = Some(error);
                    self.client.tasks.notice_result(&self.client.message, true);
                }
            }
            self.client.installation.refresh = true;
            changed = true;
        }
        if self
            .client
            .installation
            .query
            .as_ref()
            .is_some_and(|task| task.is_finished())
        {
            match self
                .client
                .installation
                .query
                .take()
                .unwrap()
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result)
            {
                Ok(mut status) => {
                    self.client.installation.query_error = None;
                    status
                        .packages
                        .sort_by(|a, b| (&a.role, &a.source).cmp(&(&b.role, &b.source)));
                    self.client.installation.selected = self
                        .client
                        .installation
                        .selected
                        .min(status.packages.len().saturating_sub(1));
                    self.client.installation.status = Some(status);
                }
                Err(error) => self.client.installation.query_error = Some(error),
            }
            changed = true;
        }
        if self.client.installation.refresh
            && self.client.installation.query.is_none()
            && !self.client.installation.busy()
        {
            self.client.installation.refresh = false;
            let socket = socket.to_owned();
            self.client.installation.query = Some(tokio::spawn(async move {
                let result = read_status(socket).await;
                super::wake();
                result
            }));
            changed = true;
        }
        if self.client.installation.notice_key.is_some()
            && self.client.installation.status.is_some()
            && self.client.installation.query_error.is_none()
            && !self.client.installation.visible()
            && self.prompt.is_none()
            && self.recovery.is_none()
            && self.editor.is_none()
            && self.client.help_scroll.is_none()
            && self.palette.is_none()
        {
            self.client.installation.notice = true;
            self.client.installation.notice_accept = true;
            changed = true;
        }
        changed
    }

    fn dismiss_installation_notice(&mut self, open: bool) {
        if let Some(key) = self.client.installation.notice_key.take() {
            self.change_preferences(|preferences| {
                preferences.installation_notices.insert(key);
            });
        }
        self.client.installation.notice = false;
        if open {
            self.client.page = shell::SETTINGS;
            self.open_installation();
        }
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
        if self.client.installation.notice {
            match key.code {
                KeyCode::Tab | KeyCode::Left | KeyCode::Right => {
                    self.client.installation.notice_accept = !self.client.installation.notice_accept
                }
                KeyCode::Enter => {
                    self.dismiss_installation_notice(self.client.installation.notice_accept)
                }
                KeyCode::Esc => self.dismiss_installation_notice(false),
                _ => {}
            }
            return Input::Idle;
        }
        match key.code {
            KeyCode::Esc => self.client.installation.open = false,
            KeyCode::Up => {
                self.client.installation.selected =
                    self.client.installation.selected.saturating_sub(1);
                self.client.installation.scroll = 0;
            }
            KeyCode::Down => {
                let count = self
                    .client
                    .installation
                    .status
                    .as_ref()
                    .map_or(0, |s| s.packages.len());
                self.client.installation.selected =
                    (self.client.installation.selected + 1).min(count.saturating_sub(1));
                self.client.installation.scroll = 0;
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Tab if !self.client.installation.busy() => {
                self.client.installation.scope = match self.client.installation.scope {
                    v::InstallationScope::User => v::InstallationScope::System,
                    v::InstallationScope::System => v::InstallationScope::User,
                };
                self.client.installation.scroll = 0;
            }
            KeyCode::Char('r') if !self.client.installation.busy() => {
                self.client.installation.query_error = None;
                self.client.installation.refresh = true;
            }
            KeyCode::PageDown => {
                self.client.installation.scroll = self.client.installation.scroll.saturating_add(5)
            }
            KeyCode::PageUp => {
                self.client.installation.scroll = self.client.installation.scroll.saturating_sub(5)
            }
            KeyCode::Enter
                if !self.client.installation.busy()
                    && !self.client.installation.refresh
                    && self.client.installation.query.is_none()
                    && self.client.installation.query_error.is_none() =>
            {
                return self.installation_request()
            }
            _ => {}
        }
        Input::Idle
    }

    fn installation_request(&mut self) -> Input {
        let Some(destination) = self.client.installation.destination() else {
            return Input::Idle;
        };
        if let Some(reason) = &destination.unavailable_reason {
            self.client.installation.error = Some(reason.clone());
            return Input::Idle;
        }
        if destination.requires_elevation
            && crate::protocol::target().is_some_and(|(_, agent)| agent.is_some())
        {
            self.client.installation.error = Some(t("Administrator permission is required. Run installation on the target machine; remote requests cannot open a permission prompt.").into());
            return Input::Idle;
        }
        let package = self.client.installation.selected().unwrap();
        Input::Install(Request {
            package_id: package.id.clone(),
            expected_release_id: package.release_id.clone(),
            scope: self.client.installation.scope,
            elevate: destination.requires_elevation,
        })
    }

    pub(super) fn start_installation(&mut self, socket: &std::path::Path, request: Request) {
        if self.client.installation.busy() {
            return;
        }
        self.client.installation.error = None;
        self.client.installation.result = None;
        self.client.installation.events.clear();
        let socket = socket.to_owned();
        let (progress, receiver) = watch::channel(None);
        self.client.installation.progress = Some(receiver);
        self.client.installation.operation = Some(tokio::spawn(async move {
            let result = astra_ipc::progress::scope(
                progress,
                crate::protocol::execute(
                    &socket,
                    v::Command::InstallCurrentPackage {
                        package_id: request.package_id,
                        expected_release_id: request.expected_release_id,
                        scope: request.scope,
                    },
                ),
            )
            .await;
            let result = result
                .and_then(|response| match response {
                    v::ResponseBody::InstallationResult(result) => Ok(result),
                    _ => anyhow::bail!("{}", t("Unexpected installation response")),
                })
                .map_err(|error| format!("{error:#}"));
            super::wake();
            result
        }));
    }

    pub(super) fn installation_mouse(&mut self, mouse: MouseEvent) -> Input {
        if matches!(
            mouse.kind,
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
        ) {
            return self.installation_key(event::KeyEvent::new(
                if mouse.kind == MouseEventKind::ScrollUp {
                    KeyCode::PageUp
                } else {
                    KeyCode::PageDown
                },
                KeyModifiers::NONE,
            ));
        }
        if mouse.kind != MouseEventKind::Down(MouseButton::Left) {
            return Input::Idle;
        }
        let hit = self.hits.borrow().hit(mouse.column, mouse.row).cloned();
        match hit {
            Some(dashboard::Target::Installation(Click::Open)) => {
                self.dismiss_installation_notice(true)
            }
            Some(dashboard::Target::Installation(Click::Later)) => {
                self.dismiss_installation_notice(false)
            }
            Some(dashboard::Target::Installation(Click::Close)) => {
                self.client.installation.open = false
            }
            Some(dashboard::Target::Installation(Click::Row(index))) => {
                self.client.installation.selected = index;
                self.client.installation.scroll = 0;
            }
            Some(dashboard::Target::Installation(Click::Scope)) => {
                return self
                    .installation_key(event::KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE))
            }
            Some(dashboard::Target::Installation(Click::Refresh)) => {
                return self
                    .installation_key(event::KeyEvent::new(KeyCode::Char('r'), KeyModifiers::NONE))
            }
            Some(dashboard::Target::Installation(Click::Install)) => {
                return self
                    .installation_key(event::KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            }
            _ => {}
        }
        Input::Idle
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
            let target = self
                .client
                .installation
                .status
                .as_ref()
                .map(|s| s.target.as_str())
                .unwrap_or("");
            let text = format!("{}\n\n{}: {target}\n\n{}", t("Install the current package to a stable location and create a launch entry."), t("Management target"), t("You can do this later from Settings → Installation. Existing configurations and running instances are retained."));
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
        let scope = match self.client.installation.scope {
            v::InstallationScope::User => "Current user",
            v::InstallationScope::System => "All users",
        };
        let target = self
            .client
            .installation
            .status
            .as_ref()
            .map(|s| format!("{} · {}", s.target, s.account))
            .unwrap_or_else(|| t("Loading installation status…").into());
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
        let mut lines = Vec::<Line>::new();
        if let Some(status) = &self.client.installation.status {
            for (index, package) in status.packages.iter().enumerate() {
                lines.push(Line::styled(
                    astra_tui_kit::text::truncate_cells(
                        &format!(
                            "{} {} · {}",
                            if index == self.client.installation.selected {
                                "›"
                            } else {
                                " "
                            },
                            t(&package.role),
                            package.release_id
                        ),
                        sections[1].width as usize,
                        "…",
                    ),
                    Style::default().fg(if index == self.client.installation.selected {
                        Color::Cyan
                    } else {
                        Color::White
                    }),
                ));
            }
            for role in ["client", "server", "controller", "web"] {
                if !status.packages.iter().any(|package| package.role == role) {
                    let reason = status
                        .unavailable_roles
                        .get(role)
                        .map(String::as_str)
                        .unwrap_or("No complete package was found on this target.");
                    lines.push(Line::from(format!(
                        "  {} · {}: {}",
                        t(role),
                        t("Unavailable"),
                        t(reason)
                    )));
                }
            }
            if let Some(package) = self.client.installation.selected() {
                let destination = self.client.installation.destination().unwrap();
                lines.extend([
                    Line::from(""),
                    Line::from(format!(
                        "{}: {}",
                        t("Package source"),
                        package.source.display()
                    )),
                    Line::from(format!(
                        "{}: {}",
                        t("Install location"),
                        destination
                            .directory
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "—".into())
                    )),
                    Line::from(format!(
                        "{}: {}",
                        t("Launch entry"),
                        destination
                            .entry
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_else(|| "—".into())
                    )),
                ]);
                if destination.installed {
                    lines.push(Line::from(t("This package is already installed.")));
                }
                if destination.requires_elevation {
                    lines.push(Line::from(t(
                        "Administrator permission is required for this location.",
                    )));
                }
                if let Some(reason) = &destination.unavailable_reason {
                    lines.push(Line::from(reason.clone()));
                }
            }
            for warning in &status.warnings {
                lines.push(Line::from(warning.clone()));
            }
        }
        if self.client.installation.query.is_some() {
            lines.push(Line::from(t("Checking the current package…")));
        }
        if self.client.installation.busy() {
            lines.push(Line::from(t(
                "Installing… You may close this panel while the operation continues.",
            )));
        }
        for event in &self.client.installation.events {
            lines.push(Line::from(event.clone()));
        }
        if let Some(result) = &self.client.installation.result {
            lines.extend([Line::from(""), Line::styled(result.message.clone(), Style::default().fg(Color::Green)), Line::from(format!("{}: {}", t("Launch command"), result.launch_command)), Line::from(t("Running programs keep their current version. Use the launch command after a maintenance restart."))]);
        }
        if let Some(error) = &self.client.installation.error {
            lines.push(Line::styled(error.clone(), Style::default().fg(Color::Red)));
        }
        if let Some(error) = &self.client.installation.query_error {
            lines.push(Line::styled(error.clone(), Style::default().fg(Color::Red)));
        }
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
