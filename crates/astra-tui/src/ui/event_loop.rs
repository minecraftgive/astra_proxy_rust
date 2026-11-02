use super::*;
use futures_util::StreamExt;

pub(super) async fn run_local(
    socket: PathBuf,
    context: crate::windows::Context,
    launch: crate::windows::Launch,
) -> Result<()> {
    let session_path =
        launch.session_path(socket.parent().context("Manager socket has no directory")?)?;
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
    enable_raw_mode()?;
    let _screen = Screen;
    execute!(
        io::stdout(),
        EnterAlternateScreen,
        EnableBracketedPaste,
        EnableMouseCapture
    )?;
    let mut terminal = Terminal::new(CrosstermBackend::new(io::stdout()))?;
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
    loop {
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
                    break;
                }
            }
            if let Some(view) = &mut ui.diagnostics {
                redraw |= view.poll();
            }
            for viewer in [&mut ui.viewer, &mut ui.tui_viewer].into_iter().flatten() {
                let revision = viewer.revision();
                viewer.poll();
                redraw |= revision != viewer.revision();
            }
            // One redraw when a successful notice recedes, never a permanent idle timer.
            let notice = (ui.message.clone(), ui.tasks.notice_is_old());
            redraw |= notice != last_notice;
            last_notice = notice;
            let health = (ui.runtime_health(), ui.runtime_rates_available());
            redraw |= health != last_health;
            last_health = health;
            previous.apply(&ui, false);
            refresh = false;
        }
        if redraw && last_frame.elapsed() >= ui.animations.frame_interval() {
            ui.draw(&mut terminal)?;
            last_frame = Instant::now();
            redraw = false;
        }
        let animated = ui.animations.active();
        let spinner = ui.tasks.running() && ui.animations.mode() == animation::MotionMode::Normal;
        let scheduled = redraw || animated || spinner;
        let frame_time = if redraw || animated {
            ui.animations.frame_interval()
        } else {
            Duration::from_millis(80)
        };
        let deadline = tokio::time::Instant::from_std(last_frame + frame_time);
        let checkpoint_deadline = tokio::time::Instant::from_std(
            checkpoint_at.unwrap_or_else(|| Instant::now() + Duration::from_secs(3600)),
        );
        let event = tokio::select! {
            event = events.as_mut().expect("terminal input is attached").next() => {
                match event { Some(event) => event?, None => break }
            }
            _ = animation::notify().notified() => { refresh = true; continue; }
            _ = maintenance.tick() => { refresh = true; continue; }
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
                    )
                    .await;
                    events = Some(event::EventStream::new());
                    ui.animations.resize();
                } else {
                    jobs.start(&mut ui, action, origin);
                }
                false
            }
            Input::Idle => false,
            Input::OpenWindow(launch) => {
                if opening.len() < 4 {
                    let context = context.clone();
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
                    let (draft, directory) = ui.prepare_external(
                        socket.parent().context("Manager socket has no directory")?,
                    )?;
                    anyhow::ensure!(
                        ui.checkpoint(&session_path),
                        t("Cannot open editor until the draft is checkpointed")
                    );
                    let launched = (|| -> Result<()> {
                        disable_raw_mode()?;
                        execute!(
                            io::stdout(),
                            DisableBracketedPaste,
                            DisableMouseCapture,
                            crossterm::cursor::Show,
                            LeaveAlternateScreen
                        )?;
                        crate::external_editor::launch(&command, &draft, &directory)
                    })();
                    // Always restore the TUI, including when the executable cannot start.
                    enable_raw_mode()?;
                    execute!(
                        io::stdout(),
                        EnterAlternateScreen,
                        EnableBracketedPaste,
                        EnableMouseCapture
                    )?;
                    terminal.clear()?;
                    launched?;
                    ui.read_external()
                })();
                events = Some(event::EventStream::new());
                ui.animations.resize();
                if let Err(error) = result {
                    ui.fail(format!("{error:#}"), Resume::Editor);
                }
                false
            }
        };
        previous.apply(&ui, true);
        refresh = true;
        if quit || checkpoint_now {
            checkpoint_at = None;
            // Failed persistence leaves the in-memory draft open and recoverable.
            if ui.checkpoint(&session_path) && quit {
                break;
            }
        } else {
            // Coalesce fast typing without extending the loss window on every key.
            checkpoint_at.get_or_insert(Instant::now() + Duration::from_millis(250));
        }
    }
    Ok(())
}

struct MotionState {
    page: usize,
    focus: String,
    palette: bool,
    drawer: bool,
    fold: Option<String>,
    message: String,
}
impl MotionState {
    fn of(ui: &Ui) -> Self {
        Self {
            page: ui.page,
            focus: format!(
                "{:?}:{}:{:?}:{}:{}:{:?}",
                ui.focus.region,
                ui.focus.cursor,
                ui.selections,
                ui.connection_selection,
                ui.group_selection,
                ui.runtime_picker
            ),
            palette: ui.palette.is_some(),
            drawer: ui.tasks.showing_details(),
            fold: ui.editor.as_ref().map(|e| e.fold_signature()),
            message: ui.message.clone(),
        }
    }
    fn apply(self, ui: &Ui, input: bool) {
        use animation::Kind;
        if self.page != ui.page {
            ui.animations.trigger(Kind::Page);
        }
        if input
            && self.focus
                != format!(
                    "{:?}:{}:{:?}:{}:{}:{:?}",
                    ui.focus.region,
                    ui.focus.cursor,
                    ui.selections,
                    ui.connection_selection,
                    ui.group_selection,
                    ui.runtime_picker
                )
        {
            ui.animations.trigger(Kind::Focus);
        }
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
        if self.fold != ui.editor.as_ref().map(|e| e.fold_signature()) {
            ui.animations.trigger(Kind::Accordion);
        }
        if self.message != ui.message {
            ui.animations.trigger(Kind::Feedback);
        }
    }
}
