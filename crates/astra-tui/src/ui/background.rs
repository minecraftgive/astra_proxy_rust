//! Background operations and bounded, reconnecting state subscriptions.
use super::*;
use astra_ipc::progress;
use astra_ipc::progress::Progress;
use tokio::{sync::watch, task::JoinHandle};

#[derive(Default)]
pub(super) struct Tasks {
    records: std::collections::VecDeque<Record>,
    details: Option<u16>,
    notice: Option<(String, u64)>,
    /// How the last background task ended, keyed by the message it left behind.
    verdict: Option<(String, bool)>,
    update_error: Option<String>,
    viewport_height: std::cell::Cell<u16>,
    content_height: std::cell::Cell<u16>,
}
struct Record {
    id: u64,
    title: String,
    started: Instant,
    finished: Option<Instant>,
    events: std::collections::VecDeque<String>,
}
impl Tasks {
    pub(super) fn running(&self) -> bool {
        self.records.iter().any(|r| r.finished.is_none())
    }
    pub(super) fn notice_is_old(&self) -> bool {
        self.notice
            .as_ref()
            .is_some_and(|(_, ms)| progress::now_ms().saturating_sub(*ms) > 6000)
    }
    pub(super) fn showing_details(&self) -> bool {
        self.details.is_some()
    }
    pub(super) fn notice_message(&mut self, message: &str) {
        self.notice = Some((message.into(), progress::now_ms()));
    }
    pub(super) fn notice_result(&mut self, message: &str, failed: bool) {
        self.notice_message(message);
        self.verdict = Some((message.into(), failed));
    }
    fn event(&mut self, id: u64, event: Progress) {
        if let Some(record) = self.records.iter_mut().find(|r| r.id == id) {
            let count = match (event.current, event.total) {
                (Some(c), Some(t)) => format!(" [{c}/{t}]"),
                _ => String::new(),
            };
            let detail: String = event
                .detail
                .replace(['\n', '\r'], " ")
                .chars()
                .take(4096)
                .collect();
            record.events.push_back(format!(
                "{} {}{count} · {detail}",
                crate::tui_log::stamp(event.timestamp_ms),
                t(&event.phase)
            ));
            if record.events.len() > 128 {
                record.events.pop_front();
            }
        }
    }
}
struct Job {
    id: u64,
    action: Action,
    selected: Option<String>,
    progress: watch::Receiver<Option<Progress>>,
    handle: JoinHandle<operations::JobResult>,
    progress_wake: JoinHandle<()>,
    view_context: (usize, Kind, Option<String>),
    overlay_epoch: u64,
}
pub(super) struct Jobs {
    socket: PathBuf,
    next: u64,
    jobs: Vec<Job>,
    changed: bool,
    log: Option<std::sync::Arc<crate::tui_log::Log>>,
}
impl Jobs {
    pub(super) fn new(socket: &std::path::Path) -> Self {
        Self {
            socket: socket.into(),
            next: 0,
            jobs: vec![],
            changed: false,
            log: socket
                .parent()
                .and_then(|p| crate::tui_log::Log::open(&p.join("tui.log")).ok())
                .map(std::sync::Arc::new),
        }
    }
    pub(super) fn take_changed(&mut self) -> bool {
        std::mem::take(&mut self.changed)
    }
    pub(super) fn start(&mut self, ui: &mut Ui, action: Action, origin: Option<(Prompt, String)>) {
        if self.jobs.len() >= 8 {
            ui.message = "任务队列已满，请等待现有任务完成".into();
            return;
        }
        if let Action::Save { name, .. } = &action {
            if self
                .jobs
                .iter()
                .any(|j| matches!(&j.action,Action::Save{name:other,..} if other==name))
            {
                ui.message = "此配置正在保存；可继续编辑，完成后再保存".into();
                return;
            }
        }
        self.next += 1;
        if matches!(&action,Action::Logs(name) if name=="@tui") && ui.tui_viewer.is_none() {
            let mut viewer = logs::Viewer::new(
                "@tui".into(),
                ui.log_settings.get("@tui").cloned().unwrap_or_default(),
            );
            viewer.set_vim_navigation(ui.vim_keys);
            ui.tui_viewer = Some(viewer);
        }
        let id = self.next;
        let title = action_description(&action);
        if let Some(log) = &self.log {
            log.write("UI task", &format!("#{id} started {title}"));
        }
        if ui.tasks.records.len() >= 32 {
            if let Some(index) = ui.tasks.records.iter().position(|r| r.finished.is_some()) {
                ui.tasks.records.remove(index);
            }
        }
        ui.tasks.records.push_back(Record {
            id,
            title,
            started: Instant::now(),
            finished: None,
            events: Default::default(),
        });
        ui.tasks
            .event(id, Progress::new("Queued", "等待执行；界面可继续操作"));
        let (sender, receiver) = watch::channel(None);
        let mut signals = receiver.clone();
        let progress_wake = tokio::spawn(async move {
            while signals.changed().await.is_ok() {
                super::wake();
            }
        });
        let input = operations::OperationInput::capture(ui);
        let selected = ui.selected().map(|r| r.instance.name.clone());
        let socket = self.socket.clone();
        let request = action.clone();
        let handle = tokio::task::spawn_local(async move {
            let result =
                progress::scope(sender, operations::perform(&socket, input, request, origin)).await;
            super::wake();
            result
        });
        self.jobs.push(Job {
            id,
            action,
            selected,
            progress: receiver,
            handle,
            progress_wake,
            view_context: (
                ui.page,
                ui.kind,
                ui.active_proxy().map(|row| row.instance.name.clone()),
            ),
            overlay_epoch: ui.overlay_epoch,
        });
    }
    pub(super) async fn poll(&mut self, ui: &mut Ui) -> bool {
        if ui
            .tasks
            .notice
            .as_ref()
            .is_none_or(|(message, _)| message != &ui.message)
        {
            ui.tasks.notice = Some((ui.message.clone(), progress::now_ms()));
            self.changed = true;
        }
        let mut quit = false;
        let mut index = 0;
        while index < self.jobs.len() {
            let job = &mut self.jobs[index];
            if job.progress.has_changed().unwrap_or(false) {
                if let Some(event) = job.progress.borrow_and_update().clone() {
                    self.changed = true;
                    if let Some(log) = &self.log {
                        log.write("UI progress", &format!("#{} {}", job.id, event.phase));
                    }
                    ui.tasks.event(job.id, event);
                }
            }
            if !job.handle.is_finished() {
                index += 1;
                continue;
            }
            let job = self.jobs.remove(index);
            job.progress_wake.abort();
            self.changed = true;
            match job.handle.await {
                Ok(result) => {
                    let failed = result.failed();
                    let message = result.message();
                    if matches!(&result, operations::JobResult::Update(_)) {
                        ui.tasks.details = Some(0);
                    }
                    ui.tasks.event(
                        job.id,
                        Progress::new(if failed { "Failed" } else { "Completed" }, message.clone()),
                    );
                    if let Some(log) = &self.log {
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
                    if let Some(record) = ui.tasks.records.iter_mut().find(|r| r.id == job.id) {
                        record.finished = Some(Instant::now());
                    }
                    ui.tasks.verdict = Some((message, failed));
                    let selected = ui.selected().map(|r| r.instance.name.clone());
                    let context = (
                        ui.page,
                        ui.kind,
                        ui.active_proxy().map(|row| row.instance.name.clone()),
                    );
                    let same = job.selected == selected && job.view_context == context
                        || match &job.action {
                            Action::Create { name, .. } => selected.as_ref() == Some(name),
                            Action::Add(instance) => selected.as_ref() == Some(&instance.name),
                            _ => false,
                        };
                    let tui_log = matches!(&job.action,Action::Logs(name) if name=="@tui");
                    let log_target_current = !tui_log
                        || (job.overlay_epoch == ui.overlay_epoch && ui.tui_viewer.is_some());
                    quit |= ui.apply_operation(
                        result,
                        &job.action,
                        operations::ApplyContext {
                            same,
                            log_target_current,
                            background: true,
                        },
                    );
                }
                Err(error) => {
                    if matches!(&job.action,Action::Logs(name) if name=="@tui")
                        && job.overlay_epoch == ui.overlay_epoch
                        && ui.tui_viewer.is_some()
                    {
                        ui.tui_viewer = None;
                        ui.message = error.to_string();
                    }
                    ui.tasks.verdict = Some((error.to_string(), true));
                    ui.tasks
                        .event(job.id, Progress::new("Failed", error.to_string()));
                    if let Some(r) = ui.tasks.records.iter_mut().find(|r| r.id == job.id) {
                        r.finished = Some(Instant::now());
                    }
                }
            }
        }
        quit
    }
}
impl Ui {
    pub(super) fn notice_host_restart(&mut self) {
        self.message =
            t("Host restarted; previous connection targets and paused data were cleared").into();
        self.tasks.notice_message(&self.message);
    }

    /// Remote update outcomes stay visible even when this interface did not start
    /// the operation, or its connection/log data is explicitly paused.
    fn update_alert(&self) -> Option<String> {
        use crate::protocol::updates::UpdatePhase;
        if let Some(error) = &self.tasks.update_error {
            return Some(tr!("Update status unavailable: {}", error));
        }
        let operation = self.update_status.as_ref()?.operation.as_ref()?;
        if !matches!(
            operation.phase,
            UpdatePhase::Failed | UpdatePhase::NeedsRecovery | UpdatePhase::Uncertain
        ) {
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
        if self.tasks.details.is_some() {
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
            self.tasks.details = if self.tasks.details.is_some() {
                None
            } else {
                Some(0)
            };
            return Some(Input::Idle);
        }
        if key.code == KeyCode::F(7) {
            self.tasks.details = None;
            return Some(if self.tui_viewer.take().is_some() {
                Input::Idle
            } else {
                Input::Run(Action::Logs("@tui".into()), None)
            });
        }
        if let Some(scroll) = &mut self.tasks.details {
            let max_scroll = self
                .tasks
                .content_height
                .get()
                .saturating_sub(self.tasks.viewport_height.get());
            match key.code {
                KeyCode::Esc => self.tasks.details = None,
                KeyCode::Up => *scroll = scroll.saturating_sub(1),
                KeyCode::Down => *scroll = scroll.saturating_add(1).min(max_scroll),
                KeyCode::PageUp => {
                    *scroll = scroll.saturating_sub(self.tasks.viewport_height.get().max(1))
                }
                KeyCode::PageDown => {
                    *scroll = scroll
                        .saturating_add(self.tasks.viewport_height.get().max(1))
                        .min(max_scroll)
                }
                KeyCode::Home => *scroll = 0,
                KeyCode::End => *scroll = max_scroll,
                KeyCode::F(5) => return Some(Input::Run(Action::UpdateStatus, None)),
                _ => {}
            }
            return Some(Input::Idle);
        }
        if let Some(viewer) = &mut self.tui_viewer {
            match viewer.key(key) {
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
        if let Some(scroll) = self.tasks.details {
            let before = f.buffer_mut().clone();
            let mut lines = if let Some(status) = &self.update_status {
                update::View::new(status.clone()).lines()
            } else {
                vec![Line::from(t("Update status unavailable; F5 refresh"))]
            };
            if let Some(error) = &self.tasks.update_error {
                lines.push(Line::styled(
                    error.clone(),
                    Style::default().fg(Color::Yellow),
                ));
            }
            lines.push(Line::from(""));
            self.tasks
                .viewport_height
                .set(area.height.saturating_sub(2));
            for record in self.tasks.records.iter().rev() {
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
            self.tasks.content_height.set(content_height);
            let scroll =
                scroll.min(content_height.saturating_sub(self.tasks.viewport_height.get()));
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
            let failed = match &self.tasks.verdict {
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
            let color = if age.is_some_and(|age| age > 6000) && color != Color::Red {
                MUTED
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

struct Feed {
    receiver: watch::Receiver<Option<Result<Reply, String>>>,
    handle: JoinHandle<()>,
}
fn feed_signature(reply: &Reply, action: &Action) -> Result<String> {
    if matches!(action, Action::WatchUpdates) {
        let mut value: serde_json::Value = serde_json::from_str(&reply.text)?;
        // Observing unchanged hosts is a heartbeat, not a visual change.
        if let Some(object) = value.as_object_mut() {
            object.remove("observed_at_ms");
        }
        return Ok(serde_json::to_string(&value)?);
    }
    Ok(serde_json::to_string(reply)?)
}
impl Drop for Feed {
    fn drop(&mut self) {
        self.handle.abort();
    }
}
impl Feed {
    fn start(socket: PathBuf, action: Action) -> Self {
        let (sender, receiver) = watch::channel(None);
        let handle = tokio::spawn(async move {
            let mut delay = 250;
            let mut previous = String::new();
            let log = socket
                .parent()
                .and_then(|p| crate::tui_log::Log::open(&p.join("tui.log")).ok());
            let label = if matches!(&action, Action::WatchList) {
                "instance state"
            } else {
                "core runtime"
            };
            let mut online = false;
            if let Some(log) = &log {
                log.write("connection", &format!("connecting {label}"));
            }
            loop {
                let result: Result<()> = async {
                    let mut stream = tokio::time::timeout(
                        Duration::from_secs(3),
                        crate::protocol::subscribe(&socket, action.clone()),
                    )
                    .await??;
                    loop {
                        let reply: Reply = tokio::time::timeout(
                            Duration::from_secs(7),
                            astra_ipc::receive_management(&mut stream),
                        )
                        .await??;
                        anyhow::ensure!(
                            reply.version == astra_ipc::API_VERSION,
                            "通信版本不匹配，请重启管理器和核心"
                        );
                        if let Some(error) = &reply.error {
                            anyhow::bail!(error.clone());
                        }
                        if !online {
                            if let Some(log) = &log {
                                log.write("connection", &format!("connected {label}"));
                            }
                            online = true;
                        }
                        let encoded = feed_signature(&reply, &action)?;
                        if encoded != previous {
                            previous = encoded;
                            sender.send_replace(Some(Ok(reply)));
                            super::wake();
                        }
                        delay = 250;
                    }
                }
                .await;
                if let Err(error) = result {
                    previous.clear();
                    if online {
                        if let Some(log) = &log {
                            log.write("connection", &format!("disconnected {label}; reconnecting"));
                        }
                        online = false;
                    }
                    sender.send_replace(Some(Err(format!("连接中断，自动重连：{error:#}"))));
                    super::wake();
                }
                tokio::time::sleep(Duration::from_millis(delay)).await;
                delay = (delay * 2).min(5000);
            }
        });
        Self { receiver, handle }
    }
    fn take(&mut self) -> Option<Result<Reply, String>> {
        if self.receiver.has_changed().unwrap_or(false) {
            self.receiver.borrow_and_update().clone()
        } else {
            None
        }
    }
}
pub(super) struct Feeds {
    pub(super) ready: bool,
    socket: PathBuf,
    list: Feed,
    runtime: Option<((String, u32), Feed)>,
    metadata: Option<(String, JoinHandle<operations::Metadata>)>,
    metadata_time: Instant,
    updates: Option<Feed>,
    update_probe: Option<JoinHandle<Option<crate::protocol::updates::UpdateStatus>>>,
}
impl Drop for Feeds {
    fn drop(&mut self) {
        if let Some((_, task)) = &self.metadata {
            task.abort();
        }
        if let Some(task) = &self.update_probe {
            task.abort();
        }
    }
}
impl Feeds {
    pub(super) fn new(socket: &std::path::Path) -> Self {
        let probe_socket = socket.to_owned();
        let update_probe = tokio::spawn(async move {
            let status = async {
                let reply = tokio::time::timeout(
                    Duration::from_secs(3),
                    call(&probe_socket, Action::UpdateStatus),
                )
                .await
                .ok()?
                .ok()?;
                let value: serde_json::Value = serde_json::from_str(&reply.text).ok()?;
                if value.get("schema_version").and_then(|v| v.as_u64()) != Some(1) {
                    return None;
                }
                serde_json::from_value(value).ok()
            }
            .await;
            super::wake();
            status
        });
        Self {
            socket: socket.into(),
            list: Feed::start(socket.into(), Action::WatchList),
            ready: false,
            runtime: None,
            metadata: None,
            metadata_time: Instant::now() - Duration::from_secs(3),
            updates: None,
            update_probe: Some(update_probe),
        }
    }
    pub(super) async fn poll(&mut self, ui: &mut Ui) -> bool {
        let mut changed = false;
        if self
            .update_probe
            .as_ref()
            .is_some_and(|task| task.is_finished())
        {
            if let Ok(Some(status)) = self.update_probe.take().unwrap().await {
                ui.update_status = Some(status);
                self.updates = Some(Feed::start(self.socket.clone(), Action::WatchUpdates));
                changed = true;
            }
        }
        if let Some(feed) = &mut self.updates {
            if let Some(result) = feed.take() {
                changed = true;
                match result
                    .and_then(|reply| serde_json::from_str(&reply.text).map_err(|e| e.to_string()))
                {
                    Ok(status) => {
                        ui.update_status = Some(status);
                        ui.tasks.update_error = None;
                    }
                    Err(error) => ui.tasks.update_error = Some(error),
                }
            }
        }
        if let Some(result) = self.list.take() {
            changed = true;
            match result {
                Ok(reply) => {
                    self.ready = true;
                    ui.update_rows(reply)
                }
                Err(error) => ui.runtime_error = error,
            }
        }
        // Runtime data always follows the active profile, whatever Profiles shows.
        let key = ui
            .active_proxy()
            .and_then(|r| r.pid.map(|pid| (r.instance.name.clone(), pid)));
        if key.as_ref() != self.runtime.as_ref().map(|(key, _)| key) {
            if self.runtime.as_ref().zip(key.as_ref()).is_some_and(
                |(((old_name, old_pid), _), (new_name, new_pid))| {
                    old_name == new_name && old_pid != new_pid
                },
            ) {
                ui.notice_host_restart();
            }
            self.runtime = key.map(|(name, pid)| {
                let feed = Feed::start(
                    self.socket.clone(),
                    Action::WatchRuntime {
                        name: name.clone(),
                        expected_pid: pid,
                        interval_ms: crate::UI_REFRESH_INTERVAL.as_millis() as u64,
                    },
                );
                ((name, pid), feed)
            });
            // This is a profile/host PID transition, not a reconnect of the same Feed.
            // Clear identity before the old Snapshot is lost, including legacy peers.
            ui.reset_live_identity();
        }
        if let Some(((name, pid), feed)) = &mut self.runtime {
            if let Some(result) = feed.take() {
                changed = true;
                match result.and_then(|reply| {
                    serde_json::from_str::<astra_model::runtime::RuntimeView>(&reply.text)
                        .map_err(|e| e.to_string())
                }) {
                    Ok(view) if view.snapshot.pid == *pid => {
                        ui.accept_runtime_view(name.clone(), view)
                    }
                    Ok(_) => ui.runtime_disconnected(t("Runtime identity changed").into()),
                    Err(error) => {
                        ui.runtime_disconnected(error);
                    }
                }
            }
        }
        if self
            .metadata
            .as_ref()
            .is_some_and(|(_, task)| task.is_finished())
        {
            let (name, task) = self.metadata.take().unwrap();
            changed = true;
            if let Ok(metadata) = task.await {
                if ui.editor.as_ref().is_some_and(|e| e.name == name) {
                    metadata.apply(ui);
                }
            }
        }
        if self.metadata.is_none() && self.metadata_time.elapsed() > Duration::from_secs(2) {
            if let Some(target) = operations::metadata_target(ui) {
                let name = ui.editor.as_ref().unwrap().name.clone();
                let socket = self.socket.clone();
                self.metadata = Some((
                    name,
                    tokio::task::spawn_local(async move {
                        let result = operations::metadata(&socket, target).await;
                        super::wake();
                        result
                    }),
                ));
                self.metadata_time = Instant::now();
            }
        }
        changed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn remote_update_failure_is_visible_without_a_local_job() {
        use crate::protocol::updates::UpdatePhase;
        crate::i18n::with_language(crate::i18n::Language::English, || {
            let mut ui = Ui::new(Session::default());
            let status = serde_json::from_value(serde_json::json!({
                "operation": {
                    "request": {"operation_id":"remote-op", "intent":{"kind":"activate", "release_id":"v2"}},
                    "phase":"needs_recovery", "before_directory":"old", "target_directory":"new",
                    "release_id":"v2", "runtime_committed":true, "next_launch_saved":false,
                    "pending_record_saved":true, "participants":[], "error":"save failed", "updated_at_ms":1
                }
            })).unwrap();
            ui.update_status = Some(status);
            let line = ui.status_line(180).to_string();
            assert!(line.contains("remote-op") && line.contains("save failed"));
            assert!(ui.tasks.records.is_empty());
            ui.update_status
                .as_mut()
                .unwrap()
                .operation
                .as_mut()
                .unwrap()
                .phase = UpdatePhase::Complete;
            assert!(ui.update_alert().is_none());
            ui.tasks.update_error = Some("disconnected".into());
            assert!(ui.status_line(180).to_string().contains("disconnected"));
        });
    }

    #[test]
    fn background_disk_observation_preserves_newer_foreground_draft_and_conflict_diff() {
        let mut ui = Ui::new(Session::default());
        ui.open_draft("proxy".into(), "standalone=true\n".into());
        ui.editor
            .as_mut()
            .unwrap()
            .import("standalone=false\n# local\n".into())
            .unwrap();
        ui.remember_draft();
        let observed = operations::OperationInput::capture(&ui)
            .prepare("proxy".into(), "standalone=true\n# external\n".into());
        ui.editor
            .as_mut()
            .unwrap()
            .import("standalone=false\n# newer local\n".into())
            .unwrap();
        operations::merge_observation(&mut ui, &observed);
        assert!(ui.editor.as_ref().unwrap().text().contains("newer local"));
        ui.remember_draft();
        let draft = ui.drafts.values().next().unwrap();
        assert!(draft.conflict);
        assert!(draft.observed_disk.as_ref().unwrap().contains("external"));
        ui.compare_draft();
        assert!(ui
            .help_override
            .as_ref()
            .is_some_and(|text| text.contains("external") && text.contains("newer local")));
    }
    #[test]
    fn unchanged_update_heartbeat_does_not_request_another_frame() {
        let a = Reply::text(
            r#"{"schema_version":1,"observed_at_ms":10,"operation":{"phase":"preparing"}}"#,
        );
        let b = Reply::text(
            r#"{"schema_version":1,"observed_at_ms":20,"operation":{"phase":"preparing"}}"#,
        );
        let c = Reply::text(
            r#"{"schema_version":1,"observed_at_ms":20,"operation":{"phase":"complete"}}"#,
        );
        assert_eq!(
            feed_signature(&a, &Action::WatchUpdates).unwrap(),
            feed_signature(&b, &Action::WatchUpdates).unwrap()
        );
        assert_ne!(
            feed_signature(&b, &Action::WatchUpdates).unwrap(),
            feed_signature(&c, &Action::WatchUpdates).unwrap()
        );
    }
    #[tokio::test]
    async fn slow_save_keeps_navigation_live_and_preserves_newer_draft() {
        tokio::task::LocalSet::new().run_until(async {
            let root=std::env::temp_dir().join(format!("astra-ui-background-{}",std::process::id()));
            std::fs::create_dir_all(&root).unwrap();
            let path=root.join("management.sock");
            let listener=astra_platform::platform::LocalListener::bind(&path).unwrap();
            let started=std::sync::Arc::new(tokio::sync::Notify::new());
            let release=std::sync::Arc::new(tokio::sync::Notify::new());
            let (entered,finish)=(started.clone(),release.clone());
            let server=tokio::spawn(async move {
                let (stream, request)=test_rpc::save_request(&listener,"standalone=false\n","standalone=true\n").await;
                entered.notify_one();finish.notified().await;
                test_rpc::reply(stream,request,astra_management::v2::ResponseBody::Draft(test_rpc::draft("standalone=true\n","standalone=true\n","2"))).await;
            });
            let mut ui=Ui::new(Session::default());
            ui.editor=Some(Editor::new("proxy".into(),"standalone=true\n".into()).with_kind(Kind::Proxy));
            let mut jobs=Jobs::new(&path);
            jobs.start(&mut ui,Action::Save{name:"proxy".into(),original:"standalone=false\n".into(),text:"standalone=true\n".into()},None);
            tokio::time::timeout(Duration::from_secs(2),started.notified()).await.unwrap();
            assert!(!jobs.jobs[0].handle.is_finished());
            ui.key(event::KeyEvent::new(KeyCode::F(6),KeyModifiers::NONE));
            assert!(ui.tasks.details.is_some());
            ui.key(event::KeyEvent::new(KeyCode::Esc,KeyModifiers::NONE));
            ui.editor.as_mut().unwrap().import("standalone=false\n".into()).unwrap();
            release.notify_one();server.await.unwrap();
            tokio::time::timeout(Duration::from_secs(2),async {
                while !jobs.jobs.is_empty(){jobs.poll(&mut ui).await;tokio::time::sleep(Duration::from_millis(5)).await;}
            }).await.unwrap();
            assert!(ui.editor.as_ref().unwrap().text().contains("standalone=false"));
            assert_eq!(ui.editor.as_ref().unwrap().original,"standalone=true\n");
            // The finished task's result is the status line, marked as a success.
            assert!(matches!(&ui.tasks.verdict, Some((_, false))));
            let status: String = ui.status_line(100).spans.iter().map(|s| s.content.as_ref()).collect();
            assert!(status.starts_with(" ✓ ") && status.lines().count() == 1, "{status}");
            assert!(ui.tasks.records[0].events.back().unwrap().contains(t("Completed")));
            assert!(ui.tasks.records[0].events.back().unwrap().chars().nth(2)==Some(':'));
            let draft=ui.editor.as_ref().unwrap().text();
            assert!(matches!(ui.key(event::KeyEvent::new(KeyCode::F(7),KeyModifiers::NONE)),Input::Run(Action::Logs(name),_)if name=="@tui"));
            assert_eq!(ui.editor.as_ref().unwrap().text(),draft);
            drop(jobs);astra_platform::platform::remove_socket(&path);std::fs::remove_dir_all(root).unwrap();
        }).await;
    }
    #[tokio::test]
    async fn state_feed_reconnects_after_server_disconnects() {
        let path = std::env::temp_dir().join(format!("astra-ui-feed-{}.sock", std::process::id()));
        let listener = astra_platform::platform::LocalListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            use astra_management::v2::*;
            for name in ["first", "reconnected"] {
                let (stream, request) = test_rpc::next(&listener).await;
                assert!(matches!(request.body, RequestBody::Query(Query::Instances)));
                test_rpc::reply(
                    stream,
                    request,
                    ResponseBody::Instances(vec![Instance {
                        id: name.into(),
                        name: name.into(),
                        kind: InstanceKind::Proxy,
                        config: "proxy.toml".into(),
                        pid: None,
                        state: "Stopped".into(),
                        uptime_secs: "0".into(),
                        capabilities: vec![],
                        changed: false,
                        details: None,
                    }]),
                )
                .await;
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        });
        let mut feed = Feed::start(path.clone(), Action::WatchList);
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if matches!(feed.take(),Some(Ok(reply))if reply.rows.first().is_some_and(|r|r.instance.name=="reconnected")) {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        drop(feed);
        server.await.unwrap();
        astra_platform::platform::remove_socket(&path);
    }
}

#[cfg(test)]
mod view_tests {
    use super::*;
    #[tokio::test]
    async fn late_tui_log_failure_does_not_reopen_a_replaced_overlay_and_current_failure_is_visible(
    ) {
        tokio::task::LocalSet::new()
            .run_until(async {
                for switch_to_palette in [true, false] {
                    let fixture = crate::import::tests::Fixture::new();
                    let socket = fixture.0.join("overlay-log.sock");
                    let listener = astra_platform::platform::LocalListener::bind(&socket).unwrap();
                    let started = std::sync::Arc::new(tokio::sync::Notify::new());
                    let release = std::sync::Arc::new(tokio::sync::Notify::new());
                    let (entered, finish) = (started.clone(), release.clone());
                    let server = tokio::spawn(async move {
                        use astra_management::v2::*;
                        let (stream, request) = test_rpc::next(&listener).await;
                        assert!(matches!(&request.body,RequestBody::Query(Query::LogLevel{instance}) if instance=="@tui"));
                        entered.notify_one();finish.notified().await;
                        test_rpc::error(stream,request,"TUI log unavailable in test").await;
                    });
                    let mut ui = Ui::new(Session::default());
                    ui.editor = Some(Editor::new("draft".into(), "standalone=true\n".into()));
                    let draft = serde_json::to_value(ui.editor.as_ref().unwrap()).unwrap();
                    let mut jobs = Jobs::new(&socket);
                    let Input::Run(action, origin) =
                        ui.key(event::KeyEvent::new(KeyCode::F(7), KeyModifiers::NONE))
                    else {
                        panic!("log request");
                    };
                    jobs.start(&mut ui, action, origin.map(|origin| *origin));
                    assert!(ui.tui_viewer.is_some());
                    tokio::time::timeout(Duration::from_secs(3), started.notified())
                        .await
                        .unwrap();
                    if switch_to_palette {
                        ui.key(event::KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE));
                    }
                    release.notify_one();
                    server.await.unwrap();
                    tokio::time::timeout(Duration::from_secs(3), async {
                        while !jobs.jobs.is_empty() {
                            jobs.poll(&mut ui).await;
                            tokio::time::sleep(Duration::from_millis(5)).await;
                        }
                    })
                    .await
                    .unwrap();
                    assert!(ui.tui_viewer.is_none());
                    assert_eq!(
                        serde_json::to_value(ui.editor.as_ref().unwrap()).unwrap(),
                        draft
                    );
                    if switch_to_palette {
                        assert!(ui.palette.is_some());
                        assert!(ui.recovery.is_none());
                    } else {
                        assert!(ui
                            .recovery
                            .as_ref()
                            .unwrap()
                            .message
                            .contains("TUI log unavailable in test"));
                    }
                    crate::i18n::with_language(crate::i18n::Language::English, || {
                        let mut terminal =
                            Terminal::new(ratatui::backend::TestBackend::new(110, 30)).unwrap();
                        terminal.draw(|frame| ui.render(frame)).unwrap();
                        let text: String = terminal
                            .backend()
                            .buffer()
                            .content
                            .iter()
                            .map(|cell| cell.symbol())
                            .collect();
                        assert!(
                            text.contains(if switch_to_palette {
                                "Commands"
                            } else {
                                "TUI log unavailable in test"
                            }),
                            "{text}"
                        );
                    });
                    drop(jobs);
                    astra_platform::platform::remove_socket(&socket);
                }
            })
            .await;
    }
    #[test]
    fn details_and_tui_log_capture_input_without_touching_editor() {
        let mut ui = Ui::new(Session::default());
        ui.editor =
            Some(Editor::new("draft".into(), "standalone=true\n".into()).with_kind(Kind::Proxy));
        let draft = ui.editor.as_ref().unwrap().text();
        ui.tasks.records.push_back(Record {
            id: 1,
            title: "Save draft".into(),
            started: Instant::now(),
            finished: None,
            events: Default::default(),
        });
        ui.tasks
            .event(1, Progress::new("Validate configuration", "draft.toml"));
        for language in [
            crate::i18n::Language::English,
            crate::i18n::Language::Simplified,
            crate::i18n::Language::Traditional,
        ] {
            crate::i18n::with_language(language, || {
                for (width, height) in [(100, 30), (24, 8)] {
                    let mut terminal =
                        ratatui::Terminal::new(ratatui::backend::TestBackend::new(width, height))
                            .unwrap();
                    ui.tasks.details = None;
                    terminal.draw(|frame| ui.render(frame)).unwrap();
                    assert!(ui
                        .hits
                        .borrow()
                        .iter()
                        .any(|(_, target)| matches!(target, dashboard::Target::TaskDetails)));
                    ui.key(event::KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE));
                    ui.paste("must not reach draft");
                    terminal.draw(|frame| ui.render(frame)).unwrap();
                    assert!(ui
                        .hits
                        .borrow()
                        .iter()
                        .all(|(_, target)| matches!(target, dashboard::Target::Back)));
                    ui.key(event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
                    ui.tui_viewer = Some(logs::Viewer::new("@tui".into(), Default::default()));
                    ui.paste("must not reach draft");
                    terminal.draw(|frame| ui.render(frame)).unwrap();
                    ui.key(event::KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
                    assert!(ui.tui_viewer.is_none());
                }
            });
        }
        assert_eq!(ui.editor.as_ref().unwrap().text(), draft);
        assert!(matches!(
            ui.key(event::KeyEvent::new(
                KeyCode::Char('c'),
                KeyModifiers::CONTROL
            )),
            Input::Quit
        ));
    }
}
