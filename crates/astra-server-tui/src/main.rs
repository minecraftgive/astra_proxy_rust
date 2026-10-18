use anyhow::{Context, Result};
use astra_server_api::{Action, Reply};
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, Paragraph, Wrap},
};
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

struct Screen;
impl Screen {
    fn enter() -> Result<Self> {
        terminal::enable_raw_mode()?;
        execute!(std::io::stdout(), EnterAlternateScreen)?;
        Ok(Self)
    }
    fn leave(&self) {
        let _ = terminal::disable_raw_mode();
        let _ = execute!(std::io::stdout(), LeaveAlternateScreen);
    }
}
impl Drop for Screen {
    fn drop(&mut self) {
        self.leave()
    }
}
fn core_default() -> Result<PathBuf> {
    let exe = std::env::current_exe()?;
    Ok(exe
        .parent()
        .context("executable directory")?
        .join(if cfg!(windows) {
            "astra-server-core.exe"
        } else {
            "astra-server-core"
        }))
}
fn background(core: &Path, config: &Path, control: &Path) -> Result<()> {
    let parent = control.parent().context("control directory")?;
    let new_directory = !parent.exists();
    std::fs::create_dir_all(parent)?;
    if new_directory {
        astra_ipc::platform::set_private(parent, 0o700)?;
    }
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(parent.join("server.log"))?;
    astra_ipc::platform::set_private(parent.join("server.log"), 0o600)?;
    let mut command = Command::new(core);
    command
        .args(["--config"])
        .arg(config)
        .arg("--control")
        .arg(control)
        .stdin(Stdio::null())
        .stdout(log.try_clone()?)
        .stderr(log);
    astra_ipc::platform::background_command(&mut command, false);
    let mut child = command.spawn().context("start server core")?;
    // Reap a child that exits while this TUI stays open.
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    Ok(())
}
#[tokio::main]
async fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let mut config = PathBuf::from("server.toml");
    let mut control = astra_ipc::platform::home_dir()?.join(".astra-server/control.sock");
    let mut core = core_default()?;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--config" => config = args.next().context("configuration path required")?.into(),
            "--control" => control = args.next().context("control path required")?.into(),
            "--core" => core = args.next().context("core path required")?.into(),
            "--help" | "-h" => {
                println!("astra-server-tui [--config server.toml] [--control PATH] [--core PATH]\nIndependent native server manager. q leaves the server running.");
                return Ok(());
            }
            _ => anyhow::bail!("unknown option {arg}"),
        }
    }
    let config = if config.is_absolute() {
        config
    } else {
        std::env::current_dir()?.join(config)
    };
    let control = if control.is_absolute() {
        control
    } else {
        std::env::current_dir()?.join(control)
    };
    let mut screen = Screen::enter()?;
    let mut terminal = Terminal::new(CrosstermBackend::new(std::io::stdout()))?;
    let mut status: Option<Reply> = None;
    let mut message = "Press i to create a private configuration; s to start.".to_string();
    let mut logs = String::new();
    let mut refresh = Instant::now() - Duration::from_secs(2);
    loop {
        if refresh.elapsed() >= Duration::from_secs(1) {
            status = astra_server_api::call(&control, Action::Status).await.ok();
            if status.is_some() {
                if let Ok(reply) = astra_server_api::call(&control, Action::Logs).await {
                    logs = reply.logs;
                }
            }
            refresh = Instant::now();
        }
        terminal.draw(|frame|{
            let areas=Layout::vertical([Constraint::Length(4),Constraint::Length(5),Constraint::Min(4),Constraint::Length(5)]).split(frame.area());
            frame.render_widget(Paragraph::new(format!("Astra Server · Native Rust\nConfiguration: {}\nControl: {}",config.display(),control.display())).style(Style::default().fg(Color::Cyan)),areas[0]);
            let summary=match &status{Some(r)=>format!("RUNNING   PID {}   uptime {}s\nConnections: {} active / {} accepted / {} rejected\nTraffic: ↑ {} B   ↓ {} B\n{}",r.pid,r.uptime_secs,r.active,r.accepted,r.rejected,r.upload,r.download,r.listeners.join(" | ")),None=>"STOPPED / UNREACHABLE".into()};
            frame.render_widget(Paragraph::new(summary).block(Block::default().borders(Borders::ALL).title("Status")).wrap(Wrap{trim:false}),areas[1]);
            let height=areas[2].height.saturating_sub(2) as usize;let tail=logs.lines().rev().take(height).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");frame.render_widget(Paragraph::new(tail).block(Block::default().borders(Borders::ALL).title("Recent events")),areas[2]);
            frame.render_widget(Paragraph::new(format!("s Start · x Stop · r Restart · l Reload · e Edit · v Validate · i Initialize\nq Quit (server keeps running) · Q Stop and quit\n{message}")).block(Block::default().borders(Borders::ALL).title("Controls")).wrap(Wrap{trim:false}),areas[3]);
        })?;
        if !event::poll(Duration::from_millis(100))? {
            continue;
        }
        let Event::Key(key) = event::read()? else {
            continue;
        };
        if key.kind != KeyEventKind::Press {
            continue;
        }
        let result: Result<String> = match key.code {
            KeyCode::Char('q') => break,
            KeyCode::Char('Q') => {
                if status.is_some() {
                    astra_server_api::call(&control, Action::Stop).await?;
                }
                break;
            }
            KeyCode::Char('s') => {
                if status.is_some() {
                    Ok("Server is already running".into())
                } else {
                    background(&core, &config, &control).map(|_| "Server launch requested".into())
                }
            }
            KeyCode::Char('x') => astra_server_api::call(&control, Action::Stop)
                .await
                .map(|_| "Stopping server".into()),
            KeyCode::Char('l') => astra_server_api::call(&control, Action::Reload { path: None })
                .await
                .map(|_| "Configuration reloaded".into()),
            KeyCode::Char('r') => {
                let stopped = if status.is_some() {
                    astra_server_api::call(&control, Action::Stop)
                        .await
                        .map(|_| ())
                } else {
                    Ok(())
                };
                match stopped {
                    Err(e) => Err(e),
                    Ok(()) => {
                        let deadline = Instant::now() + Duration::from_secs(15);
                        while control.exists() && Instant::now() < deadline {
                            tokio::time::sleep(Duration::from_millis(100)).await;
                        }
                        if control.exists() {
                            Err(anyhow::anyhow!("Server is still stopping; restart later"))
                        } else {
                            background(&core, &config, &control).map(|_| "Server restarted".into())
                        }
                    }
                }
            }
            KeyCode::Char('i') => Command::new(&core)
                .arg("--init")
                .arg(&config)
                .output()
                .map_err(Into::into)
                .and_then(|o| {
                    if o.status.success() {
                        Ok("Created configuration with a random UUID".into())
                    } else {
                        Err(anyhow::anyhow!(
                            String::from_utf8_lossy(&o.stderr).into_owned()
                        ))
                    }
                }),
            KeyCode::Char('v') => Command::new(&core)
                .arg("--config")
                .arg(&config)
                .arg("--check")
                .output()
                .map_err(Into::into)
                .and_then(|o| {
                    if o.status.success() {
                        Ok(String::from_utf8_lossy(&o.stdout).trim().into())
                    } else {
                        Err(anyhow::anyhow!(
                            String::from_utf8_lossy(&o.stderr).into_owned()
                        ))
                    }
                }),
            KeyCode::Char('e') => {
                screen.leave();
                let editor = std::env::var_os("VISUAL")
                    .or_else(|| std::env::var_os("EDITOR"))
                    .unwrap_or_else(|| {
                        if cfg!(windows) {
                            "notepad.exe".into()
                        } else {
                            "vi".into()
                        }
                    });
                let result = Command::new(editor)
                    .arg(&config)
                    .status()
                    .map_err(anyhow::Error::from)
                    .and_then(|s| {
                        anyhow::ensure!(s.success(), "editor exited unsuccessfully");
                        Ok("Configuration saved; validate and reload or restart to apply".into())
                    });
                screen = Screen::enter()?;
                terminal.clear()?;
                result
            }
            _ => continue,
        };
        message = match result {
            Ok(m) => m,
            Err(e) => format!("Error: {e}"),
        };
        refresh = Instant::now() - Duration::from_secs(2);
    }
    drop(screen);
    Ok(())
}
