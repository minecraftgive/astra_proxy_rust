//! Background operations and bounded, reconnecting state subscriptions.
use super::*;
use astra_ipc::progress;
use astra_ipc::progress::Progress;

use astra_client::tasks::Record;
#[derive(Default)]
pub(super) struct TaskView {
    viewport_height: std::cell::Cell<u16>,
    content_height: std::cell::Cell<u16>,
}
pub(super) struct Jobs(astra_client::jobs::Jobs);
impl std::ops::Deref for Jobs {
    type Target = astra_client::jobs::Jobs;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl std::ops::DerefMut for Jobs {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}
impl Jobs {
    pub(super) fn new(socket: &std::path::Path) -> Self {
        Self(astra_client::jobs::Jobs::new(
            socket,
            astra_client::waker::Waker::new(super::wake),
        ))
    }
    pub(super) fn start(&mut self, ui: &mut Ui, action: Action, origin: Option<(Prompt, String)>) {
        if let Err(error) = self.0.check(&action) {
            ui.message = error.to_string();
            return;
        }
        if matches!(&action,Action::Logs(name) if name=="@tui") && ui.tui_viewer.is_none() {
            let mut viewer = logs::Viewer::new(
                "@tui".into(),
                ui.log_settings.get("@tui").cloned().unwrap_or_default(),
            );
            viewer.set_vim_navigation(ui.vim_keys);
            ui.tui_viewer = Some(viewer);
        }
        let id = self.next + 1;
        let title = action_description(&action);
        if let Some(log) = &self.log {
            log.write("UI task", &format!("#{id} started {title}"));
        }
        if ui.client.tasks.records.len() >= 32 {
            if let Some(index) = ui
                .client
                .tasks
                .records
                .iter()
                .position(|r| r.finished.is_some())
            {
                ui.client.tasks.records.remove(index);
            }
        }
        ui.client.tasks.records.push_back(Record {
            id,
            title,
            started: Instant::now(),
            finished: None,
            events: Default::default(),
        });
        ui.client
            .tasks
            .event(id, Progress::new("Queued", "等待执行；界面可继续操作"));
        let origin_context = astra_client::jobs::Origin {
            selected: ui.selected().map(|r| r.instance.name.clone()),
            view_context: (
                ui.page,
                ui.kind,
                ui.active_proxy().map(|row| row.instance.name.clone()),
            ),
            overlay_epoch: ui.overlay_epoch,
        };
        let input = operations::OperationInput::capture(ui);
        if let Err(error) = self.0.start(input, action, origin, origin_context) {
            ui.message = error.to_string();
        }
    }

    pub(super) async fn poll(&mut self, ui: &mut Ui) -> bool {
        let queue = &mut self.0;
        if ui
            .tasks
            .notice
            .as_ref()
            .is_none_or(|(message, _)| message != &ui.message)
        {
            ui.client.tasks.notice = Some((ui.message.clone(), progress::now_ms()));
            queue.changed = true;
        }
        let mut quit = false;
        let mut index = 0;
        while index < queue.jobs.len() {
            let job = &mut queue.jobs[index];
            if job.progress.has_changed().unwrap_or(false) {
                if let Some(event) = job.progress.borrow_and_update().clone() {
                    queue.changed = true;
                    if let Some(log) = &queue.log {
                        log.write("UI progress", &format!("#{} {}", job.id, event.phase));
                    }
                    ui.client.tasks.event(job.id, event);
                }
            }
            if !job.handle.is_finished() {
                index += 1;
                continue;
            }
            let job = queue.jobs.remove(index);
            job.progress_wake.abort();
            queue.changed = true;
            match job.handle.await {
                Ok(result) => {
                    let failed = result.failed();
                    let message = result.message();
                    if matches!(&result, operations::JobResult::Update(_)) {
                        ui.client.tasks.details = Some(0);
                    }
                    ui.client.tasks.event(
                        job.id,
                        Progress::new(if failed { "Failed" } else { "Completed" }, message.clone()),
                    );
                    if let Some(log) = &queue.log {
                        log.write(
                            "UI task",
                            &format!(
                                "#{} {}",
                                job.id,
                                if failed {
                                    "failed (F6 details)"
                                } else {
                                    "completed"
                                }
                            ),
                        );
                    }
                    if let Some(record) =
                        ui.client.tasks.records.iter_mut().find(|r| r.id == job.id)
                    {
                        record.finished = Some(Instant::now());
                    }
                    ui.client.tasks.verdict = Some((message, failed));
                    let selected = ui.selected().map(|r| r.instance.name.clone());
                    let context = (
                        ui.page,
                        ui.kind,
                        ui.active_proxy().map(|row| row.instance.name.clone()),
                    );
                    let origin = astra_client::jobs::Origin {
                        selected: job.selected,
                        view_context: job.view_context,
                        overlay_epoch: job.overlay_epoch,
                    };
                    let current = astra_client::jobs::Origin {
                        selected,
                        view_context: context,
                        overlay_epoch: ui.overlay_epoch,
                    };
                    let apply =
                        origin.apply_context(&job.action, &current, ui.tui_viewer.is_some());
                    quit |= ui.apply_operation(result, &job.action, apply);
                }
                Err(error) => {
                    if matches!(&job.action,Action::Logs(name) if name=="@tui")
                        && job.overlay_epoch == ui.overlay_epoch
                        && ui.tui_viewer.is_some()
                    {
                        ui.tui_viewer = None;
                        ui.message = error.to_string();
                    }
                    ui.client.tasks.verdict = Some((error.to_string(), true));
                    ui.client
                        .tasks
                        .event(job.id, Progress::new("Failed", error.to_string()));
                    if let Some(r) = ui.client.tasks.records.iter_mut().find(|r| r.id == job.id) {
                        r.finished = Some(Instant::now());
                    }
                }
            }
        }
        quit
    }
}
impl Ui {
    pub(super) fn refresh_program_info(&mut self) {
        self.program_info.refresh = true;
    }

    pub(super) fn notice_host_restart(&mut self) {
        self.message =
            t("Host restarted; previous connection targets and paused data were cleared").into();
        self.client.tasks.notice_message(&self.client.message);
    }

    /// Remote update outcomes stay visible even when this interface did not start
    /// the operation, or its connection/log data is explicitly paused.
    fn update_alert(&self) -> Option<String> {
        use crate::protocol::updates::UpdatePhase;
        if let Some(error) = &self.client.tasks.update_error {
            return Some(tr!("Update status unavailable: {}", error));
        }
        let operation = self.update_status.as_ref()?.operation.as_ref()?;
        if !matches!(
            operation.phase,
            UpdatePhase::Failed | UpdatePhase::NeedsRecovery | UpdatePhase::Uncertain
        ) {
            return None;
        }
        // NeedsRecovery/Uncertain need action and stay; a plain failure changed
        // nothing, so an old or acknowledged one stays in F6 details only.
        if operation.phase == UpdatePhase::Failed
            && (operation.updated_at_ms < self.update_alert_since_ms
                || self.acknowledged_update.as_ref() == Some(&operation.request.operation_id))
        {
            return None;
        }
        Some(tr!(
            "Update {} · {} · {}",
            operation.request.operation_id,
            t(operation.phase.label()),
            operation.error.as_deref().unwrap_or("")
        ))
    }

    pub(super) fn background_paste(&mut self, text: &str) -> bool {
        if self.client.tasks.details.is_some() {
            return true;
        }
        if let Some(viewer) = &mut self.tui_viewer {
            viewer.paste(text);
            return true;
        }
        false
    }
    pub(super) fn background_key(&mut self, key: event::KeyEvent) -> Option<Input> {
        if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
            return Some(Input::Quit);
        }
        if key.code == KeyCode::F(6) {
            self.client.tasks.details = if self.client.tasks.details.is_some() {
                None
            } else {
                // F6 shows the full update status, which acknowledges a failure.
                self.acknowledged_update = self
                    .update_status
                    .as_ref()
                    .and_then(|status| status.operation.as_ref())
                    .filter(|operation| {
                        operation.phase == crate::protocol::updates::UpdatePhase::Failed
                    })
                    .map(|operation| operation.request.operation_id.clone())
                    .or(self.acknowledged_update.take());
                Some(0)
            };
            return Some(Input::Idle);
        }
        if key.code == KeyCode::F(7) {
            self.client.tasks.details = None;
            return Some(if self.tui_viewer.take().is_some() {
                Input::Idle
            } else {
                Input::Run(Action::Logs("@tui".into()), None)
            });
        }
        let viewport = self.task_view.viewport_height.get();
        let max_scroll = self.task_view.content_height.get().saturating_sub(viewport);
        if let Some(scroll) = &mut self.client.tasks.details {
            match key.code {
                KeyCode::Esc => self.client.tasks.details = None,
                KeyCode::Up => *scroll = scroll.saturating_sub(1),
                KeyCode::Down => *scroll = scroll.saturating_add(1).min(max_scroll),
                KeyCode::PageUp => *scroll = scroll.saturating_sub(viewport.max(1)),
                KeyCode::PageDown => {
                    *scroll = scroll.saturating_add(viewport.max(1)).min(max_scroll)
                }
                KeyCode::Home => *scroll = 0,
                KeyCode::End => *scroll = max_scroll,
                KeyCode::F(5) => return Some(Input::Run(Action::UpdateStatus, None)),
                _ => {}
            }
            return Some(Input::Idle);
        }
        if let Some(viewer) = &mut self.tui_viewer {
            match viewer.key(crate::input::key(key)) {
                logs::Command::Close => self.tui_viewer = None,
                logs::Command::Reconnect => {
                    return Some(Input::Run(Action::Logs("@tui".into()), None))
                }
                _ => {}
            }
            return Some(Input::Idle);
        }
        None
    }
    pub(super) fn render_background(&self, f: &mut Frame<'_>) {
        let area = f.area();
        if let Some(viewer) = &self.tui_viewer {
            viewer.render(f, area);
            self.hits.borrow_mut().clear();
        }
        if let Some(scroll) = self.client.tasks.details {
            let before = f.buffer_mut().clone();
            let mut lines = if let Some(status) = &self.update_status {
                update::View::new(status.clone()).lines()
            } else {
                vec![Line::from(t("Update status unavailable; F5 refresh"))]
            };
            if let Some(error) = &self.client.tasks.update_error {
                lines.push(Line::styled(
                    error.clone(),
                    Style::default().fg(Color::Yellow),
                ));
            }
            lines.push(Line::from(""));
            self.task_view
                .viewport_height
                .set(area.height.saturating_sub(2));
            for record in self.client.tasks.records.iter().rev() {
                lines.push(
                    Line::from(format!(
                        "#{} {} · {}",
                        record.id,
                        record.title,
                        if record.finished.is_some() {
                            t("Finished")
                        } else {
                            t("Running")
                        }
                    ))
                    .style(Style::default().fg(Color::Cyan)),
                );
                lines.extend(record.events.iter().map(|s| Line::from(s.as_str())));
                lines.push(Line::from(""));
            }
            f.render_widget(Clear, area);
            let content = Paragraph::new(lines).wrap(Wrap { trim: false });
            let content_height = content
                .line_count(area.width.saturating_sub(2).max(1))
                .min(u16::MAX as usize) as u16;
            self.task_view.content_height.set(content_height);
            let scroll =
                scroll.min(content_height.saturating_sub(self.task_view.viewport_height.get()));
            f.render_widget(
                content.scroll((scroll, 0)).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(t("Task details · F6 / Esc back · ↑↓ scroll")),
                ),
                area,
            );
            self.hits.borrow_mut().clear();
            self.hit(
                ratatui::layout::Rect::new(area.x, area.y, area.width, 1),
                dashboard::Target::Back,
            );
            self.animations
                .surface(f, animation::Kind::Drawer, area, &before);
        } else if area.height > 0 {
            let status =
                ratatui::layout::Rect::new(area.x, area.y + area.height - 1, area.width, 1);
            if self.menu.is_none() && self.binding_picker.is_none() && self.prompt.is_none() {
                self.hit(status, dashboard::Target::TaskDetails);
            }
            f.render_widget(Paragraph::new(self.status_line(status.width)), status);
        }
    }
}

impl Ui {
    /// One line for what just happened: a running task, else the latest result.
    /// Fresh results are bright, older ones fade; time and F6 sit on the right.
    fn status_line(&self, width: u16) -> Line<'static> {
        use crate::theme::MUTED;
        let running = self
            .tasks
            .records
            .iter()
            .rev()
            .find(|r| r.finished.is_none());
        let (icon, text, color, stamp) = if let Some(alert) = self.update_alert() {
            ("✗", alert.replace(['\n', '\r'], " "), Color::Red, None)
        } else if let Some(record) = running {
            // Events read "HH:MM:SS.mmm phase · detail"; the clock is shown on the right.
            let event = record.events.back().map(String::as_str).unwrap_or_default();
            let event = event.split_once(' ').map_or(event, |(_, rest)| rest);
            (
                self.animations.spinner(),
                format!(
                    "{} · {} · {:.1}s",
                    record.title,
                    event,
                    record.started.elapsed().as_secs_f64()
                ),
                Color::Yellow,
                None,
            )
        } else {
            let message = self.operation_text().replace(['\n', '\r'], " ");
            let notice = self
                .tasks
                .notice
                .as_ref()
                .filter(|(m, _)| t(m) == message || m == &message);
            let age = notice.map(|(_, ms)| progress::now_ms().saturating_sub(*ms));
            let failed = match &self.client.tasks.verdict {
                Some((verdict, failed)) if verdict == &message || t(verdict) == message => {
                    Some(*failed)
                }
                _ => None,
            };
            let color = match failed {
                Some(true) => Color::Red,
                Some(false) => Color::Green,
                None => crate::theme::outcome_color(&message),
            };
            let icon = match color {
                Color::Red => "✗",
                Color::Green => "✓",
                Color::Yellow => "⟳",
                _ => "•",
            };
            // Results older than a few seconds recede so a new one draws the eye.
            // A new result appears at full color at once; only the recession is a
            // slow one-way fade, so the line never dims and returns.
            let color = if age.is_some_and(|age| age > 6000) && color != Color::Red {
                self.animations
                    .progress(animation::Kind::Recede)
                    .map_or(MUTED, |p| animation::blend(color, MUTED, p))
            } else {
                color
            };
            (icon, message, color, notice.map(|(_, ms)| *ms))
        };
        let mut right = String::new();
        if let Some(ms) = stamp {
            // Seconds are enough here; F6 keeps the full timestamps.
            right.push_str(
                &crate::tui_log::stamp(ms)
                    .chars()
                    .take(8)
                    .collect::<String>(),
            );
            right.push_str(" · ");
        }
        right.push_str(t("F6 details"));
        let right = format!(" {right} ");
        let room = (width as usize).saturating_sub(Line::from(right.as_str()).width());
        if text.is_empty() {
            let mut spans = vec![Span::raw(" ".repeat(room))];
            spans.extend(crate::theme::key_hints(&right));
            return Line::from(spans);
        }
        let mut left = format!(" {icon} {text}");
        if Line::from(left.as_str()).width() > room {
            let mut cut = String::new();
            for c in left.chars() {
                if Line::from(cut.as_str()).width() + 2 > room {
                    break;
                }
                cut.push(c);
            }
            left = cut + "…";
        }
        let pad = room.saturating_sub(Line::from(left.as_str()).width());
        let color = if self.animations.marquee().status {
            animation::STATUS_MARK
        } else {
            color
        };
        let mut spans = vec![
            Span::styled(left, Style::default().fg(color)),
            Span::raw(" ".repeat(pad)),
        ];
        if room > 0 {
            spans.push(Span::styled(right, Style::default().fg(MUTED)));
        }
        Line::from(spans)
    }
}

#[cfg(test)]
use astra_client::feeds::feed_signature;
#[cfg(test)]
struct Feed(astra_client::feeds::Feed);
#[cfg(test)]
impl Feed {
    fn start(socket: PathBuf, action: Action) -> Self {
        Self(astra_client::feeds::Feed::start(
            socket,
            action,
            astra_client::waker::Waker::new(super::wake),
        ))
    }
    fn take(&mut self) -> Option<Result<Reply, String>> {
        self.0.take()
    }
}
pub(super) struct Feeds(astra_client::feeds::Feeds);
impl std::ops::Deref for Feeds {
    type Target = astra_client::feeds::Feeds;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}
impl Feeds {
    pub(super) fn new(socket: &std::path::Path) -> Self {
        Self(
            astra_client::feeds::Feeds::new(socket, astra_client::waker::Waker::new(super::wake))
                .with_local_build_info(astra_platform::build_info!()),
        )
    }
    pub(super) async fn poll(&mut self, ui: &mut Ui) -> bool {
        self.0.poll(ui).await
    }
}
impl astra_client::feeds::Host for Ui {
    fn refresh_program_info(&mut self) {
        Ui::refresh_program_info(self);
    }
    fn notice_host_restart(&mut self) {
        Ui::notice_host_restart(self);
    }
    fn reset_live_identity(&mut self) {
        Ui::reset_live_identity(self);
    }
    fn accept_runtime_view(&mut self, name: String, view: astra_model::runtime::RuntimeView) {
        Ui::accept_runtime_view(self, name, view);
    }
    fn runtime_disconnected(&mut self, error: String) {
        self.client.runtime_disconnected(error);
    }
}

#[cfg(test)]
#[path = "../test/ui_background_test.rs"]
mod tests;

#[cfg(test)]
#[path = "../test/ui_background_view_test.rs"]
mod view_tests;
