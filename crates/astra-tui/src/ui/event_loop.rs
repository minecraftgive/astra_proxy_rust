use super::*;
use futures_util::StreamExt;

pub(super) async fn run_local(
    socket: PathBuf,
    context: crate::windows::Context,
    launch: crate::windows::Launch,
    resume_ui: bool,
    mut restart_ready: Option<PathBuf>,
) -> Result<()> {
    let session_path = launch.session_path(&crate::protocol::session_home(&context.home)?)?;
    let mut ui = restore_session(&session_path, restart_ready.is_some())?;
    // Preferences are per user, shared by every window and remote target.
    ui.load_preferences(context.home.join(settings::FILE));
    #[cfg(any(windows, target_os = "macos", target_os = "linux"))]
    ui.load_startup_context(&context);
    ui.client.maintenance.register(&context)?;
    let mut startup = if resume_ui || astra_management::maintenance::pending(&context.home)? {
        startup::Instances::resumed()
    } else {
        startup::Instances::new(&launch)
    };
    ui.client.page = ui.client.preferences.start_page.page();
    if let Some(view) = launch.view {
        if let Some(page) = view.page() {
            ui.client.page = page;
        }
    }
    if let Some(kind) = launch.kind {
        ui.kind = kind;
    }
    ui.initialize_installation(&context, &socket, &launch, resume_ui);
    let window_id = launch.window_id.clone();
    let mut initial = Some(launch);
    let mut restart = update::Restart::new(resume_ui);
    let mut restarted = false;
    let mut opening: Vec<tokio::task::JoinHandle<Result<()>>> = Vec::new();
    ui.mouse_request = None;
    ui.client.maintenance.terminal = cfg!(unix);
    if astra_management::maintenance::pending(&context.home)? {
        ui.client.installation.notice = false;
        ui.client.installation.open = true;
    }
    let mut screen = TerminalSession::enter(TerminalOptions {
        mouse: ui.client.preferences.mouse,
        bracketed_paste: true,
    })?;
    let mut terminal = astra_tui_kit::frame::screen()?;
    let mut jobs = background::Jobs::new(&socket);
    let mut feeds = background::Feeds::new(&socket);
    if let Some(name) = ui.active_log.clone() {
        jobs.start(&mut ui, Action::Logs(name), None);
    }
    let mut events = Some(event::EventStream::new());
    let mut maintenance = tokio::time::interval(crate::UI_REFRESH_INTERVAL);
    maintenance.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let mut redraw = true;
    let mut refresh = true;
    let mut last_frame = Instant::now() - Duration::from_secs(1);
    let mut last_notice = (ui.client.message.clone(), false);
    let mut last_health = (ui.runtime_health(), ui.runtime_rates_available());
    let mut checkpoint_at: Option<Instant> = None;
    let shutdown = astra_platform::platform::shutdown_signal();
    tokio::pin!(shutdown);
    // Keep errors as loop outcomes so every exit reaches checkpoint and terminal restore.
    // Avoid wrapping this large event loop in another async future (Windows stack budget).
    let outcome = 'ui: loop {
        if refresh {
            let previous = MotionState::of(&ui);
            redraw |= ui.poll_terminal_scan().await;
            redraw |= ui.poll_installation(&socket).await;
            redraw |= feeds.poll(&mut ui).await;
            redraw |= ui.client.poll_dns_query(&socket);
            if feeds.ready {
                if let Some(launch) = initial.take() {
                    if let Some(action) = ui.apply_launch(&launch) {
                        jobs.start(&mut ui, action, None);
                    }
                }
            }
            let mut index = 0;
            while index < opening.len() {
                if opening[index].is_finished() {
                    ui.client.message = match opening.remove(index).await {
                        Ok(Ok(())) => t("Opened in a new window").into(),
                        Ok(Err(error)) => format!("{}: {error:#}", t("Cannot open a new window")),
                        Err(error) => error.to_string(),
                    };
                    ui.client.tasks.notice_message(&ui.client.message);
                    redraw = true;
                } else {
                    index += 1;
                }
            }
            let quit = jobs.poll(&mut ui).await;
            #[cfg(windows)]
            {
                redraw |= ui.poll_service_change().await;
            }
            if !quit {
                let recovering = ui.recovery.is_some();
                if let Some(action) = startup.poll(&mut ui, &socket, feeds.ready).await {
                    jobs.start(&mut ui, action, None);
                    redraw = true;
                }
                redraw |= recovering != ui.recovery.is_some();
            }
            let changed = jobs.take_changed();
            redraw |= changed;
            if changed || quit {
                checkpoint_at.get_or_insert(Instant::now() + Duration::from_millis(250));
                if quit && ui.checkpoint(&session_path) {
                    break Ok(());
                }
            }
            if let Some(view) = &mut ui.client.diagnostics {
                redraw |= view.poll();
                ui.client.remember_exit_ips();
            }
            redraw |= ui.client.poll_exit_probe(&socket);
            if let Some(view) = &mut ui.monitor {
                redraw |= view.poll(&socket).await;
            }
            let idle = !quit && initial.is_none() && opening.is_empty() && ui.restart_ready();
            let previous_message = ui.client.message.clone();
            let launch_label = ui.client.update_launch.label();
            match restart.poll_client(&mut ui.client, &context, idle) {
                Ok(Some(mut target)) => {
                    // Do not relinquish the old UI until its complete draft is
                    // durable. The new process opens this exact window session.
                    if ui.checkpoint(&session_path) {
                        match ui.client.maintenance.checkpointed(
                            &context,
                            &target,
                            Default::default(),
                            ui.client.preferences.silent_startup,
                        ) {
                            Ok(false) => {
                                redraw = true;
                                continue;
                            }
                            Err(error) => {
                                ui.client.update_launch.clear();
                                ui.client.message = format!("{error:#}");
                                redraw = true;
                                continue;
                            }
                            Ok(true) => {}
                        }
                        let launch = ui.restart_launch(window_id.clone());
                        target.language = crate::i18n::language();
                        drop(events.take());
                        let result = screen
                            .suspend()
                            .map_err(anyhow::Error::from)
                            .and_then(|()| crate::self_update::restart(&target, &launch));
                        match result {
                            Ok(()) => {
                                restarted = true;
                                break Ok(());
                            }
                            Err(error) => {
                                ui.client.update_launch.clear();
                                if let Err(restore) = screen.resume() {
                                    break Err(
                                        error.context(format!("Restore terminal: {restore}"))
                                    );
                                }
                                events = Some(event::EventStream::new());
                                terminal.backend_mut().writer_mut().invalidate();
                                ui.animations.resize();
                                ui.client.message = tr!(
                                    "Cannot restart updated interface: {}",
                                    format!("{error:#}")
                                );
                            }
                        }
                    } else {
                        restart.defer(target);
                    }
                    redraw = true;
                }
                Ok(None) => {}
                Err(error) => {
                    ui.client.message =
                        tr!("Cannot restart updated interface: {}", format!("{error:#}"));
                    redraw = true;
                }
            }
            if let Some(id) = ui.client.maintenance.needs_checkpoint() {
                if ui.checkpoint(&session_path) {
                    if let Err(error) = ui.client.maintenance.acknowledge(&id) {
                        ui.client.message = format!("{error:#}");
                    }
                }
            }
            if let Some((record, action)) = ui.client.maintenance.authorization.take() {
                drop(events.take());
                screen.suspend()?;
                eprintln!("{}", t("Maintenance will interrupt local connections. Authorize before stopping the old backend."));
                let result = astra_client::maintenance::authorize_terminal(record, action).await;
                screen.resume()?;
                events = Some(event::EventStream::new());
                terminal.backend_mut().writer_mut().invalidate();
                ui.animations.resize();
                if let Err(error) = result {
                    ui.client.maintenance.error = Some(format!("{error:#}"));
                }
                redraw = true;
            }
            redraw |= previous_message != ui.client.message;
            let next_label = ui.client.update_launch.label();
            redraw |= launch_label != next_label;
            if ui.client.update_launch.pending() && ui.recovery.is_none() && !ui.persistence_paused
            {
                redraw |= ui.client.message != next_label;
                ui.client.message = next_label;
            }
            for viewer in [&mut ui.client.viewer, &mut ui.client.tui_viewer]
                .into_iter()
                .flatten()
            {
                let revision = viewer.revision();
                viewer.poll();
                redraw |= revision != viewer.revision();
            }
            // One redraw when a successful notice recedes, never a permanent idle timer.
            let notice = (ui.client.message.clone(), ui.client.tasks.notice_is_old());
            if notice.1 && !last_notice.1 && notice.0 == last_notice.0 {
                ui.animations.trigger(animation::Kind::Recede);
            }
            redraw |= notice != last_notice;
            last_notice = notice;
            let health = (ui.runtime_health(), ui.runtime_rates_available());
            redraw |= health != last_health;
            last_health = health;
            previous.apply(&ui);
            refresh = false;
        }
        if redraw && last_frame.elapsed() >= ui.animations.draw_interval() {
            if let Err(error) = ui.draw(&mut terminal) {
                break Err(error);
            }
            if let Err(error) = ui.client.maintenance.mark_ready(&context) {
                ui.client.message = format!("{error:#}");
            }
            if let Some(path) = restart_ready.take() {
                if let Err(error) = crate::self_update::signal_ready(&path) {
                    break Err(error);
                }
            }
            last_frame = Instant::now();
            redraw = false;
        }
        let animated = ui.animations.active();
        let spinner =
            ui.client.tasks.running() && ui.animations.mode() == animation::MotionMode::Normal;
        let marquee = ui.animations.marquee_running();
        let scheduled = redraw || animated || spinner || marquee;
        let frame_time = match (redraw || animated, marquee) {
            (true, true) => ui
                .animations
                .frame_interval()
                .min(ui.animations.marquee_interval()),
            (true, false) => ui.animations.frame_interval(),
            (false, true) => ui.animations.marquee_interval(),
            (false, false) => Duration::from_millis(80),
        };
        let deadline = tokio::time::Instant::from_std(last_frame + frame_time);
        let checkpoint_deadline = tokio::time::Instant::from_std(
            checkpoint_at.unwrap_or_else(|| Instant::now() + Duration::from_secs(3600)),
        );
        let connection_expiry = ui.next_connection_expiry();
        let connection_deadline = tokio::time::Instant::from_std(
            connection_expiry.unwrap_or_else(|| Instant::now() + Duration::from_secs(3600)),
        );
        let notice_expiry = ui.next_notice_expiry();
        let notice_deadline = tokio::time::Instant::from_std(
            notice_expiry.unwrap_or_else(|| Instant::now() + Duration::from_secs(3600)),
        );
        let event = tokio::select! {
            result = &mut shutdown => break 'ui result.map_err(Into::into),
            event = events.as_mut().expect("terminal input is attached").next() => {
                match event {
                    Some(Ok(event)) => event,
                    Some(Err(error)) => break 'ui Err(error.into()),
                    None => break 'ui Ok(()),
                }
            }
            _ = animation::notify().notified() => { refresh = true; continue; }
            _ = maintenance.tick() => { refresh = true; continue; }
            _ = tokio::time::sleep_until(connection_deadline), if connection_expiry.is_some() => {
                redraw |= ui.expire_connection_highlights();
                continue;
            }
            _ = tokio::time::sleep_until(notice_deadline), if notice_expiry.is_some() => {
                redraw |= ui.expire_notices();
                continue;
            }
            _ = tokio::time::sleep_until(deadline), if scheduled => { redraw = true; continue; }
            _ = tokio::time::sleep_until(checkpoint_deadline), if checkpoint_at.is_some() => {
                checkpoint_at = None;
                if !ui.checkpoint(&session_path) { redraw = true; }
                continue;
            }
        };
        let checkpoint_now = matches!(&event, Event::Key(key) if key.code == KeyCode::Esc);
        let previous = MotionState::of(&ui);
        // Further navigation must never wait for a reveal or an outgoing overlay.
        if matches!(&event, Event::Key(_) | Event::Mouse(_) | Event::Paste(_)) {
            if ui.palette.is_some() {
                ui.animations.finish(animation::Kind::Palette);
            }
            if ui.client.tasks.showing_details() {
                ui.animations.finish(animation::Kind::Drawer);
            }
        }
        redraw = true;
        let input = match event {
            Event::Paste(text) if ui.recovery.is_none() => {
                ui.paste(&text);
                Input::Idle
            }
            Event::Key(key) if key.kind != KeyEventKind::Release => ui.key(key),
            Event::Key(key) => {
                ui.key_release(key);
                Input::Idle
            }
            Event::Mouse(mouse) => ui.mouse(mouse),
            Event::Resize(..) => {
                ui.animations.resize();
                Input::Idle
            }
            _ => Input::Idle,
        };
        let quit = match input {
            Input::Frontend(_) => unreachable!("TUI commands consume their presentation effects"),
            Input::Install(request) => {
                let result = if request.elevate {
                    drop(events.take());
                    let result = installation::elevate(&socket, &request, &mut screen).await;
                    if let Err(error) = screen.resume() {
                        break Err(error.into());
                    }
                    events = Some(event::EventStream::new());
                    terminal.backend_mut().writer_mut().invalidate();
                    ui.animations.resize();
                    result
                } else {
                    Ok(())
                };
                match result {
                    Ok(()) => ui.start_installation(&socket, request),
                    Err(error) => ui.installation_failed(format!("{error:#}")),
                }
                false
            }
            Input::Quit => true,
            Input::Run(action, origin) => {
                if let Action::PrepareElevation { name, expected_pid } = action {
                    // sudo needs the real terminal, so this step cannot run as a
                    // background job; the wait for the new core can.
                    drop(events.take());
                    elevate(
                        &mut ui,
                        &mut jobs,
                        &socket,
                        name,
                        expected_pid,
                        &mut terminal,
                        &mut screen,
                    )
                    .await;
                    if let Err(error) = screen.resume() {
                        break Err(error.into());
                    }
                    events = Some(event::EventStream::new());
                    ui.animations.resize();
                } else {
                    jobs.start(&mut ui, action, origin.map(|origin| *origin));
                }
                false
            }
            Input::Idle => false,
            Input::OpenWindow(launch) => {
                if opening.len() < 4 {
                    let mut context = context.clone();
                    // New windows open in the language chosen in Settings now.
                    context.language = crate::i18n::language();
                    let terminal = ui.client.preferences.terminal.clone();
                    opening.push(tokio::task::spawn_blocking(move || {
                        let result = context.open(&launch, &terminal);
                        super::wake();
                        result
                    }));
                }
                false
            }
            Input::ExternalEditor => {
                drop(events.take());
                let result = (|| -> Result<()> {
                    let command = crate::external_editor::command()?;
                    let (draft, directory) =
                        ui.prepare_external(&crate::protocol::session_home(&context.home)?)?;
                    anyhow::ensure!(
                        ui.checkpoint(&session_path),
                        t("Cannot open editor until the draft is checkpointed")
                    );
                    let launched = (|| -> Result<()> {
                        screen.suspend()?;
                        crate::external_editor::launch(&command, &draft, &directory)
                    })();
                    // Always restore the TUI, including when the executable cannot start.
                    screen.resume()?;
                    terminal.backend_mut().writer_mut().invalidate();
                    launched?;
                    ui.read_external()
                })();
                if let Err(error) = screen.resume() {
                    break Err(error.into());
                }
                events = Some(event::EventStream::new());
                ui.animations.resize();
                if let Err(error) = result {
                    ui.fail(format!("{error:#}"), Resume::Editor);
                }
                false
            }
        };
        previous.apply(&ui);
        if let Some(mouse) = ui.mouse_request.take() {
            if let Err(error) = screen.set_mouse(mouse) {
                ui.client.message = format!("{error:#}");
            }
        }
        refresh = true;
        if quit || checkpoint_now {
            checkpoint_at = None;
            // Failed persistence leaves the in-memory draft open and recoverable.
            if ui.checkpoint(&session_path) && quit {
                break Ok(());
            }
        } else {
            // Coalesce fast typing without extending the loss window on every key.
            checkpoint_at.get_or_insert(Instant::now() + Duration::from_millis(250));
        }
    };
    drop(events.take());
    finish_session(
        outcome,
        || {
            // The replacement may already be saving its own session. Never
            // overwrite it with a late checkpoint from the outgoing process.
            if restarted {
                return Ok(());
            }
            anyhow::ensure!(
                ui.checkpoint(&session_path),
                "Cannot save final UI session: {}",
                session_path.display()
            );
            Ok(())
        },
        || screen.suspend().map_err(Into::into),
    )
}

fn restore_session(path: &std::path::Path, handoff: bool) -> Result<Ui> {
    // A replacement must prove it can read the checkpoint before acknowledging
    // readiness. The outgoing process still owns the recoverable draft.
    anyhow::ensure!(
        !handoff || path.is_file(),
        "Missing saved interface session for restart"
    );
    match Session::load(path) {
        Ok(session) => Ok(Ui::new(session)),
        Err(error) if handoff => Err(error).context("Restore updated interface session"),
        Err(error) => {
            let mut ui = Ui::new(Session::default());
            ui.fail(
                tr!("Cannot restore session: {}", format!("{error:#}")),
                Resume::Session,
            );
            Ok(ui)
        }
    }
}

impl Ui {
    fn restart_ready(&self) -> bool {
        let ready = !self.client.tasks.running()
            && !self.client.installation.busy()
            && (!self.client.installation.visible() || self.client.update_launch.pending() || self.client.maintenance.record.as_ref().is_some_and(|r| matches!(r.phase, astra_management::maintenance::Phase::Ready | astra_management::maintenance::Phase::Complete)))
            && !self.persistence_paused
            && self.recovery.is_none()
            // Confirmation overlays are intentionally not persisted.
            && !self.prompt.as_ref().is_some_and(Prompt::confirmation);
        #[cfg(windows)]
        let ready = ready && self.service_change.is_none();
        ready
    }

    fn restart_launch(&self, window_id: Option<String>) -> crate::windows::Launch {
        let (view, instance) = if self.client.tui_viewer.is_some() {
            (crate::windows::View::TuiLog, None)
        } else if let Some(viewer) = &self.client.viewer {
            (crate::windows::View::Logs, Some(viewer.name.clone()))
        } else if let Some(view) = &self.client.diagnostics {
            (crate::windows::View::Diagnostics, Some(view.name.clone()))
        } else {
            (
                crate::windows::View::from_page(self.client.page),
                self.selected().map(|row| row.instance.name.clone()),
            )
        };
        crate::windows::Launch {
            view: Some(view),
            instance,
            kind: Some(self.kind),
            window_id,
        }
    }
}

fn finish_session(
    outcome: Result<()>,
    checkpoint: impl FnOnce() -> Result<()>,
    restore: impl FnOnce() -> Result<()>,
) -> Result<()> {
    let mut errors = Vec::new();
    if let Err(error) = outcome {
        errors.push(format!("{error:#}"));
    }
    if let Err(error) = checkpoint() {
        errors.push(format!("checkpoint: {error:#}"));
    }
    if let Err(error) = restore() {
        errors.push(format!("terminal restore: {error:#}"));
    }
    anyhow::ensure!(errors.is_empty(), "{}", errors.join("; "));
    Ok(())
}

/// Only overlays entering or leaving are animated; page, focus, fold and
/// message changes are instant (see the rules in `animation`).
struct MotionState {
    palette: bool,
    drawer: bool,
}
impl MotionState {
    fn of(ui: &Ui) -> Self {
        Self {
            palette: ui.palette.is_some(),
            drawer: ui.client.tasks.showing_details(),
        }
    }
    fn apply(self, ui: &Ui) {
        use animation::Kind;
        match (self.palette, ui.palette.is_some()) {
            (false, true) => ui.animations.open(Kind::Palette),
            (true, false) => ui.animations.close(Kind::Palette),
            _ => {}
        }
        match (self.drawer, ui.client.tasks.showing_details()) {
            (false, true) => ui.animations.open(Kind::Drawer),
            (true, false) => ui.animations.close(Kind::Drawer),
            _ => {}
        }
    }
}

#[cfg(test)]
#[path = "../test/ui_event_loop_exit_test.rs"]
mod exit_tests;
