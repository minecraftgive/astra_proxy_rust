mod backend;
mod keys;
mod model;
mod pages;
mod theme;
mod window;
use astra_client::{bootstrap, i18n, protocol::Kind};
use gpui::AppContext;

pub fn run(kind: Kind) -> anyhow::Result<()> {
    i18n::init()?;
    let options = bootstrap::Options::parse(
        (kind == Kind::Server).then_some(kind),
        std::env::args().skip(1),
    )?;
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
    let backend = backend::Backend::start(options)?;
    let commands = backend.commands.clone();
    let events = backend.events.clone();
    let handle = backend.handle.clone();
    let waker = backend.waker.clone();
    let notifications = backend.notifications.clone();
    gpui_platform::application().run(move |cx| {
        gpui_component::init(cx);
        cx.bind_keys(keys::bindings(cx));
        let model = cx.new(|_| model::AppModel::new(kind, commands, handle, waker));
        model::AppModel::bridge(&model, events, cx);
        model::AppModel::observe_notifications(&model, notifications, cx);
        model::AppModel::observe_quit(&model, cx);
        if let Err(error) = window::open(model.clone(), cx) {
            eprintln!("{error:#}");
            cx.quit();
        }
        cx.on_window_closed(move |cx, _| {
            if cx.windows().is_empty() {
                model.update(cx, |model, cx| model.request_quit(cx));
            }
        })
        .detach();
    });
    backend.shutdown();
    Ok(())
}
