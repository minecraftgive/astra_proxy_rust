mod askpass;
mod backend;
mod keys;
mod lifecycle;
mod model;
mod pages;
mod theme;
pub mod views;
pub mod widgets;
mod window;
use astra_client::{bootstrap, i18n, protocol::Kind};
use gpui::AppContext;

pub fn run(kind: Kind) -> anyhow::Result<()> {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    if askpass::requested(&arguments) {
        std::process::exit(askpass::run());
    }
    i18n::init()?;
    let options = bootstrap::Options::parse((kind == Kind::Server).then_some(kind), arguments)?;
    if options.help {
        println!("Astra GUI\n\n--state-dir PATH  --config-dir PATH  --bin-dir PATH  --endpoint PATH  --agent ID\n--core proxy|rule|server  --view overview|proxies|connections|rules|logs|profiles|settings\n--silent  --shutdown  update|update-status|rollback");
        return Ok(());
    }
    if options.silent || options.operation.is_some() || options.shutdown || options.daemon {
        return std::thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            tokio::task::LocalSet::new().block_on(&runtime, async {
                let prepared = bootstrap::prepare(&options).await?;
                if let Some(message) = bootstrap::run_maintenance(&prepared, &options).await? {
                    println!("{message}");
                }
                Ok(())
            })
        })
        .join()
        .map_err(|_| anyhow::anyhow!("Backend thread stopped unexpectedly"))?;
    }
    let kind = options.launch.kind.unwrap_or(kind);
    let resume_ui = options.resume_ui;
    let restart_ready = options.restart_ready.clone();
    let restart_plan: model::RestartPlan = Default::default();
    let next_restart = restart_plan.clone();
    let frontend_home = options.home.clone();
    let backend = backend::Backend::start(options)?;
    let commands = backend.commands.clone();
    let events = backend.events.clone();
    let handle = backend.handle.clone();
    let waker = backend.waker.clone();
    let notifications = backend.notifications.clone();
    gpui_platform::application()
        .with_quit_mode(gpui::QuitMode::Explicit)
        .run(move |cx| {
            let (native_quit, native_quit_requests) = async_channel::unbounded();
            lifecycle::install_quit_hook(native_quit);
            gpui_component::init(cx);
            cx.bind_keys(keys::bindings(cx));
            let model = cx.new(|_| {
                let mut model = model::AppModel::new(kind, commands, handle, waker);
                model.configure_launch(resume_ui, restart_ready, next_restart);
                model
            });
            model.update(cx, |_, cx| {
                cx.spawn(async move |model, cx| {
                    while native_quit_requests.recv().await.is_ok() {
                        if model
                            .update(cx, |model, cx| model.request_quit(cx))
                            .is_err()
                        {
                            break;
                        }
                    }
                })
                .detach();
            });
            model::AppModel::bridge(&model, events, cx);
            model::AppModel::observe_notifications(&model, notifications, cx);
            model::AppModel::observe_quit(&model, cx);
            if let Err(error) = window::open(model.clone(), cx) {
                eprintln!("{error:#}");
                model.update(cx, |model, cx| model.request_quit(cx));
            }
            cx.on_window_closed(move |cx, _| {
                if cx.windows().is_empty() {
                    model.update(cx, |model, cx| model.request_quit(cx));
                }
            })
            .detach();
        });
    backend.shutdown();
    if let Ok(log) =
        astra_platform::tui_log::Log::open(&astra_platform::tui_log::frontend_path(&frontend_home))
    {
        log.write("lifecycle", "native event loop and backend stopped");
    }
    let restart = restart_plan
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .take();
    if let Some((context, launch)) = restart {
        // All native windows and the manager executor are closed before replacing this process.
        astra_client::self_update::restart(&context, &launch)?;
    }
    Ok(())
}

#[cfg(feature = "test-support")]
pub mod test_support {
    pub use crate::backend::{Backend, BackendCmd, BackendEvent};
    pub use crate::model::AppModel;
    pub use crate::window::open;
    pub fn init(cx: &mut gpui::App) {
        gpui_component::init(cx);
        cx.bind_keys(crate::keys::bindings(cx));
    }
}
