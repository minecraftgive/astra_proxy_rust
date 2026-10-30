//! Background operations and bounded, reconnecting state subscriptions.
use super::*;
use astra_ipc::progress::{self, Progress};
use tokio::{sync::watch, task::JoinHandle};

#[derive(Default)]
pub(super) struct Tasks {
    records: std::collections::VecDeque<Record>,
    details: Option<u16>,
    notice: Option<(String, u64)>,
    toast: Option<(String, u64, Instant)>,
}
struct Record {
    id: u64,
    title: String,
    started: Instant,
    finished: Option<Instant>,
    events: std::collections::VecDeque<String>,
}
impl Tasks {
    pub(super) fn showing_details(&self) -> bool {
        self.details.is_some()
    }
    pub(super) fn notice_message(&mut self, message: &str) {
        self.notice = Some((message.into(), progress::now_ms()));
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
    fn line(&self) -> String {
        let record = self
            .records
            .iter()
            .rev()
            .find(|r| r.finished.is_none())
            .or_else(|| self.records.back());
        record
            .map(|r| {
                format!(
                    "{} · {} · {:.1}s · {}",
                    r.events.back().cloned().unwrap_or_default(),
                    r.title,
                    r.finished
                        .unwrap_or_else(Instant::now)
                        .duration_since(r.started)
                        .as_secs_f64(),
                    t("F6 tasks · F7 TUI log")
                )
            })
            .unwrap_or_else(|| t("F6 tasks · F7 TUI log").into())
    }
}
struct Job {
    id: u64,
    action: Action,
    selected: Option<String>,
    progress: watch::Receiver<Option<Progress>>,
    handle: JoinHandle<(Ui, bool)>,
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
        let mut worker = Ui::new(ui.session.clone());
        if !matches!(
            &action,
            Action::Save { .. } | Action::UpdateSubscriptions(_)
        ) {
            worker.editor = None;
        }
        worker.recovery = None;
        worker.prompt = None;
        worker.rows = ui.rows.clone();
        worker.details = ui.details.clone();
        worker.details_supported = ui.details_supported;
        let selected = ui.selected().map(|r| r.instance.name.clone());
        let socket = self.socket.clone();
        let request = action.clone();
        let handle = tokio::task::spawn_local(async move {
            let quit = progress::scope(sender, worker.execute(&socket, request, origin)).await;
            (worker, quit)
        });
        self.jobs.push(Job {
            id,
            action,
            selected,
            progress: receiver,
            handle,
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
        }
        let mut quit = false;
        let mut index = 0;
        while index < self.jobs.len() {
            let job = &mut self.jobs[index];
            if job.progress.has_changed().unwrap_or(false) {
                if let Some(event) = job.progress.borrow_and_update().clone() {
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
            self.changed = true;
            match job.handle.await {
                Ok((mut worker, exit)) => {
                    let failed = worker.operation_failed;
                    let detail = worker
                        .recovery
                        .as_ref()
                        .map(|r| r.message.clone())
                        .unwrap_or_else(|| worker.message.clone());
                    ui.tasks.event(
                        job.id,
                        Progress::new(if failed { "Failed" } else { "Completed" }, detail),
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
                    let selected = ui.selected().map(|r| r.instance.name.clone());
                    let same = job.selected == selected
                        || match &job.action {
                            Action::Create { name, .. } => selected.as_ref() == Some(name),
                            Action::Add(instance) => selected.as_ref() == Some(&instance.name),
                            _ => false,
                        };
                    if let Action::Save { name, text, .. } = &job.action {
                        if !failed {
                            if let Some(editor) = ui.editor.as_mut().filter(|e| e.name == *name) {
                                editor.original = text.clone();
                            }
                        }
                    } else if matches!(&job.action,Action::Logs(name) if name=="@tui") {
                        ui.tui_viewer = worker.viewer.take();
                    } else if let Action::Logs(_) = &job.action {
                        if same {
                            ui.viewer = worker.viewer.take();
                            ui.active_log = worker.active_log.clone();
                        }
                    } else if same && !failed {
                        // Background completions never replace a draft being edited.
                        if ui.editor.is_none() {
                            if matches!(
                                &job.action,
                                Action::Create { .. } | Action::Add(_) | Action::SetConfig(..)
                            ) {
                                ui.kind = worker.kind;
                                ui.selections = worker.selections;
                            }
                            ui.editor = worker.editor.take();
                        }
                        if worker.diagnostics.is_some() {
                            ui.diagnostics = worker.diagnostics.take();
                        }
                        if worker.explanation.is_some() {
                            ui.explanation = worker.explanation.take();
                        }
                        if let Some(snapshot) = worker.runtime_snapshot.take() {
                            ui.accept_runtime(worker.runtime_instance.clone(), snapshot);
                        }
                    }
                    if let Action::SetLogLevel { name, level } = &job.action {
                        if !failed {
                            if let Some(viewer) = ui.viewer.as_mut().filter(|v| v.name == *name) {
                                viewer.capture_changed(*level);
                            }
                        }
                    }
                    if same && ui.prompt.is_none() && ui.recovery.is_none() {
                        ui.recovery = worker.recovery.take();
                    }
                    if let Some(editor) = worker.editor.take() {
                        if !matches!(
                            &job.action,
                            Action::Save { .. } | Action::UpdateSubscriptions(_)
                        ) {
                            ui.ready_editors.insert(editor.name.clone(), editor);
                        }
                    }
                    ui.message = worker.message;
                    quit |= exit;
                }
                Err(error) => {
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
            match key.code {
                KeyCode::Esc => self.tasks.details = None,
                KeyCode::Up => *scroll = scroll.saturating_sub(1),
                KeyCode::Down => *scroll = scroll.saturating_add(1),
                KeyCode::PageUp => *scroll = scroll.saturating_sub(10),
                KeyCode::PageDown => *scroll = scroll.saturating_add(10),
                KeyCode::Home => *scroll = 0,
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
            let mut lines = vec![];
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
            f.render_widget(
                Paragraph::new(lines)
                    .wrap(Wrap { trim: false })
                    .scroll((scroll, 0))
                    .block(
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
        } else if area.height > 0 {
            let status =
                ratatui::layout::Rect::new(area.x, area.y + area.height - 1, area.width, 1);
            if self.menu.is_none() && self.binding_picker.is_none() && self.prompt.is_none() {
                self.hit(status, dashboard::Target::TaskDetails);
            }
            f.render_widget(
                Paragraph::new(
                    if let Some((message, ms, _)) = self
                        .tasks
                        .toast
                        .as_ref()
                        .filter(|(_, _, at)| at.elapsed() < Duration::from_secs(4))
                    {
                        format!("{} {}", crate::tui_log::stamp(*ms), message)
                    } else if self.tasks.records.is_empty() {
                        self.tasks
                            .notice
                            .as_ref()
                            .filter(|(message, _)| !message.is_empty())
                            .map(|(message, ms)| {
                                format!(
                                    "{} {} · F6 / F7",
                                    crate::tui_log::stamp(*ms),
                                    t(message).replace(['\n', '\r'], " ")
                                )
                            })
                            .unwrap_or_else(|| self.tasks.line())
                    } else {
                        self.tasks.line()
                    },
                )
                .style(Style::default().fg(Color::Cyan)),
                status,
            );
        }
    }
}

struct Feed {
    receiver: watch::Receiver<Option<Result<Reply, String>>>,
    handle: JoinHandle<()>,
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
                        sender.send_replace(Some(Ok(reply)));
                        delay = 250;
                    }
                }
                .await;
                if let Err(error) = result {
                    if online {
                        if let Some(log) = &log {
                            log.write("connection", &format!("disconnected {label}; reconnecting"));
                        }
                        online = false;
                    }
                    sender.send_replace(Some(Err(format!("连接中断，自动重连：{error:#}"))));
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
    metadata: Option<(String, JoinHandle<Ui>)>,
    metadata_time: Instant,
}
impl Drop for Feeds {
    fn drop(&mut self) {
        if let Some((_, task)) = &self.metadata {
            task.abort();
        }
    }
}
impl Feeds {
    pub(super) fn new(socket: &std::path::Path) -> Self {
        Self {
            socket: socket.into(),
            list: Feed::start(socket.into(), Action::WatchList),
            ready: false,
            runtime: None,
            metadata: None,
            metadata_time: Instant::now() - Duration::from_secs(3),
        }
    }
    pub(super) async fn poll(&mut self, ui: &mut Ui) {
        if let Some(result) = self.list.take() {
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
            ui.runtime_snapshot = None;
        }
        if let Some(((name, pid), feed)) = &mut self.runtime {
            if let Some(result) = feed.take() {
                match result.and_then(|r| {
                    serde_json::from_str::<astra_ipc::runtime::Snapshot>(&r.text)
                        .map_err(|e| e.to_string())
                }) {
                    Ok(snapshot) if snapshot.pid == *pid => {
                        ui.accept_runtime(name.clone(), snapshot)
                    }
                    Ok(_) => {}
                    Err(error) => {
                        ui.runtime_snapshot = None;
                        ui.runtime_error = error;
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
            if let Ok(mut worker) = task.await {
                if ui.editor.as_ref().is_some_and(|e| e.name == name) {
                    // Metadata setters are copied without replacing the current draft.
                    if let Some(editor) = worker.editor.take() {
                        if let Some(current) = &mut ui.editor {
                            current.merge_metadata(&editor);
                        }
                    }
                }
            }
        }
        if self.metadata.is_none() && self.metadata_time.elapsed() > Duration::from_secs(2) {
            if let Some(editor) = &ui.editor {
                let name = editor.name.clone();
                let mut worker = Ui::new(ui.session.clone());
                worker.rows = ui.rows.clone();
                worker.details = ui.details.clone();
                let socket = self.socket.clone();
                self.metadata = Some((
                    name,
                    tokio::task::spawn_local(async move {
                        worker.refresh_node_tags(&socket).await;
                        worker
                    }),
                ));
                self.metadata_time = Instant::now();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn slow_save_keeps_navigation_live_and_preserves_newer_draft() {
        tokio::task::LocalSet::new().run_until(async {
            let root=std::env::temp_dir().join(format!("astra-ui-background-{}",std::process::id()));
            std::fs::create_dir_all(&root).unwrap();
            let path=root.join("manager.sock");
            let listener=astra_ipc::platform::LocalListener::bind(&path).unwrap();
            let started=std::sync::Arc::new(tokio::sync::Notify::new());
            let release=std::sync::Arc::new(tokio::sync::Notify::new());
            let (entered,finish)=(started.clone(),release.clone());
            let server=tokio::spawn(async move {
                let (mut stream,_)=listener.accept().await.unwrap();
                let hello:Request=astra_ipc::receive_management(&mut stream).await.unwrap();
                assert!(matches!(hello.action,Action::Session));
                astra_ipc::send_management(&mut stream,&astra_ipc::session::Event::<Reply>::Ready(astra_ipc::API_VERSION)).await.unwrap();
                let request:Request=astra_ipc::receive_management(&mut stream).await.unwrap();
                assert!(matches!(request.action,Action::Save{..}));
                astra_ipc::send_management(&mut stream,&astra_ipc::session::Event::<Reply>::Progress(Progress::new("Validate","检查配置"))).await.unwrap();
                entered.notify_one();finish.notified().await;
                astra_ipc::send_management(&mut stream,&astra_ipc::session::Event::Reply(Reply::text("saved"))).await.unwrap();
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
            assert_eq!(ui.tasks.line().lines().count(),1);
            assert!(ui.tasks.line().contains(t("Completed")));
            assert!(ui.tasks.records[0].events.back().unwrap().chars().nth(2)==Some(':'));
            let draft=ui.editor.as_ref().unwrap().text();
            assert!(matches!(ui.key(event::KeyEvent::new(KeyCode::F(7),KeyModifiers::NONE)),Input::Run(Action::Logs(name),_)if name=="@tui"));
            assert_eq!(ui.editor.as_ref().unwrap().text(),draft);
            drop(jobs);astra_ipc::platform::remove_socket(&path);std::fs::remove_dir_all(root).unwrap();
        }).await;
    }
    #[tokio::test]
    async fn state_feed_reconnects_after_server_disconnects() {
        let path = std::env::temp_dir().join(format!("astra-ui-feed-{}.sock", std::process::id()));
        let listener = astra_ipc::platform::LocalListener::bind(&path).unwrap();
        let server = tokio::spawn(async move {
            for text in ["first", "reconnected"] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let request: Request = astra_ipc::receive_management(&mut stream).await.unwrap();
                assert!(matches!(request.action, Action::WatchList));
                astra_ipc::send_management(&mut stream, &Reply::text(text))
                    .await
                    .unwrap();
                tokio::time::sleep(Duration::from_millis(30)).await;
            }
        });
        let mut feed = Feed::start(path.clone(), Action::WatchList);
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                if matches!(feed.take(),Some(Ok(reply))if reply.text=="reconnected") {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap();
        drop(feed);
        server.await.unwrap();
        astra_ipc::platform::remove_socket(&path);
    }
}

#[cfg(test)]
mod view_tests {
    use super::*;
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
