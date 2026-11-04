use super::*;
use futures_util::StreamExt;

pub(super) async fn run_local(
    socket: PathBuf,
    context: crate::windows::Context,
    launch: crate::windows::Launch,
) -> Result<()> {
    let session_path = launch.session_path(&crate::protocol::session_home(&context.home)?)?;
    let mut ui = match Session::load(&session_path) {
        Ok(session) => Ui::new(session),
        Err(error) => {
            let mut ui = Ui::new(Session::default());
            ui.fail(
                tr!("Cannot restore session: {}", format!("{error:#}")),
                Resume::Session,
            );
            ui
        }
    };
    // Preferences are per user, shared by every window and remote target.
    ui.load_preferences(context.home.join(settings::FILE));
    ui.page = ui.preferences.start_page.page();
    if let Some(view) = launch.view {
        if let Some(page) = view.page() {
            ui.page = page;
        }
    }
    if let Some(kind) = launch.kind {
        ui.kind = kind;
    }
    let mut initial = Some(launch);
    let mut opening: Vec<tokio::task::JoinHandle<Result<()>>> = Vec::new();
    ui.mouse_request = None;
    let mut screen = TerminalSession::enter(TerminalOptions {
        mouse: ui.preferences.mouse,
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
    let mut last_notice = (ui.message.clone(), false);
    let mut last_health = (ui.runtime_health(), ui.runtime_rates_available());
    let mut checkpoint_at: Option<Instant> = None;
    let shutdown = astra_platform::platform::shutdown_signal();
    tokio::pin!(shutdown);
    // Keep errors as loop outcomes so every exit reaches checkpoint and terminal restore.
    // Avoid wrapping this large event loop in another async future (Windows stack budget).
    let outcome = 'ui: loop {
        if refresh {
            let previous = MotionState::of(&ui);
            redraw |= feeds.poll(&mut ui).await;
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
                    ui.message = match opening.remove(index).await {
                        Ok(Ok(())) => t("Opened in a new window").into(),
                        Ok(Err(error)) => format!("{}: {error:#}", t("Cannot open a new window")),
                        Err(error) => error.to_string(),
                    };
                    ui.tasks.notice_message(&ui.message);
                    redraw = true;
                } else {
                    index += 1;
                }
            }
            let quit = jobs.poll(&mut ui).await;
            let changed = jobs.take_changed();
            redraw |= changed;
            if changed || quit {
                checkpoint_at.get_or_insert(Instant::now() + Duration::from_millis(250));
                if quit && ui.checkpoint(&session_path) {
                    break Ok(());
                }
            }
            if let Some(view) = &mut ui.diagnostics {
                redraw |= view.poll();
            }
            if let Some(view) = &mut ui.monitor {
                redraw |= view.poll(&socket).await;
            }
            for viewer in [&mut ui.viewer, &mut ui.tui_viewer].into_iter().flatten() {
                let revision = viewer.revision();
                viewer.poll();
                redraw |= revision != viewer.revision();
            }
            // One redraw when a successful notice recedes, never a permanent idle timer.
            let notice = (ui.message.clone(), ui.tasks.notice_is_old());
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
        if redraw && last_frame.elapsed() >= ui.animations.frame_interval() {
            if let Err(error) = ui.draw(&mut terminal) {
                break Err(error);
            }
            last_frame = Instant::now();
            redraw = false;
        }
        let animated = ui.animations.active();
        let spinner = ui.tasks.running() && ui.animations.mode() == animation::MotionMode::Normal;
        let marquee = ui.animations.marquee_running();
        let scheduled = redraw || animated || spinner || marquee;
        let frame_time = if redraw || animated {
            ui.animations.frame_interval()
        } else if marquee {
            ui.animations
                .frame_interval()
                .max(Duration::from_millis(50))
        } else {
            Duration::from_millis(80)
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
            if ui.tasks.showing_details() {
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
                    opening.push(tokio::task::spawn_blocking(move || {
                        let result = context.open(&launch);
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
                ui.message = format!("{error:#}");
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

#[cfg(test)]
mod exit_tests {
    use super::*;
    #[test]
    fn error_exit_attempts_both_cleanup_steps_and_preserves_original_failure() {
        let calls = std::cell::RefCell::new(Vec::new());
        let result = finish_session(
            Err(anyhow::anyhow!("draw failed")),
            || {
                calls.borrow_mut().push("checkpoint");
                anyhow::bail!("disk full")
            },
            || {
                calls.borrow_mut().push("restore");
                anyhow::bail!("terminal closed")
            },
        );
        assert_eq!(*calls.borrow(), ["checkpoint", "restore"]);
        let text = result.unwrap_err().to_string();
        assert!(
            text.contains("draw failed")
                && text.contains("disk full")
                && text.contains("terminal closed")
        );
    }
    #[test]
    fn clean_exit_checkpoints_before_restoring_terminal() {
        let calls = std::cell::RefCell::new(Vec::new());
        finish_session(
            Ok(()),
            || {
                calls.borrow_mut().push("checkpoint");
                Ok(())
            },
            || {
                calls.borrow_mut().push("restore");
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(*calls.borrow(), ["checkpoint", "restore"]);
    }
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
            drawer: ui.tasks.showing_details(),
        }
    }
    fn apply(self, ui: &Ui) {
        use animation::Kind;
        match (self.palette, ui.palette.is_some()) {
            (false, true) => ui.animations.open(Kind::Palette),
            (true, false) => ui.animations.close(Kind::Palette),
            _ => {}
        }
        match (self.drawer, ui.tasks.showing_details()) {
            (false, true) => ui.animations.open(Kind::Drawer),
            (true, false) => ui.animations.close(Kind::Drawer),
            _ => {}
        }
    }
}
