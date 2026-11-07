//! Real isolated manager/core integration. Build astra-host and astra-proxy first.
#![cfg(all(feature = "test-support", unix))]
use astra_client::{
    bootstrap::Options,
    protocol::{self, Action, Instance, Kind},
    state::Input,
};
use astra_gui::test_support::{self, AppModel, Backend};
use astra_model::runtime::{Command as RuntimeCommand, Mode};
use gpui::{AppContext, Entity, TestAppContext};
use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

struct Manager {
    root: PathBuf,
    args: Vec<String>,
    child: Child,
    host: PathBuf,
}
impl Drop for Manager {
    fn drop(&mut self) {
        let _ = Command::new(&self.host)
            .args(&self.args)
            .arg("--shutdown")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let deadline = Instant::now() + Duration::from_secs(5);
        while self.child.try_wait().ok().flatten().is_none() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.root);
    }
}
impl Manager {
    fn start() -> Self {
        let bin = std::env::var_os("ASTRA_GUI_TEST_BIN_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                std::env::current_exe()
                    .unwrap()
                    .parent()
                    .unwrap()
                    .parent()
                    .unwrap()
                    .to_path_buf()
            });
        let host = bin.join("astra-host");
        assert!(
            host.is_file() && bin.join("astra-proxy").is_file(),
            "build astra-host and astra-proxy, or set ASTRA_GUI_TEST_BIN_DIR"
        );
        let root = PathBuf::from(format!(
            "/tmp/ag-live-{}-{}",
            std::process::id(),
            astra_ipc::progress::now_ms()
        ));
        for dir in ["state", "config", "gui"] {
            astra_platform::storage::private_dir(&root.join(dir)).unwrap();
        }
        let args = vec![
            "--state-dir".into(),
            root.join("state").display().to_string(),
            "--config-dir".into(),
            root.join("config").display().to_string(),
            "--bin-dir".into(),
            bin.display().to_string(),
        ];
        let log = fs::File::create(root.join("manager.log")).unwrap();
        let child = Command::new(&host)
            .args(&args)
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        let mut manager = Self {
            root,
            args,
            child,
            host,
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while !manager.socket().exists() {
            assert!(Instant::now() < deadline, "manager did not start");
            assert!(manager.child.try_wait().unwrap().is_none());
            std::thread::sleep(Duration::from_millis(20));
        }
        manager
    }
    fn socket(&self) -> PathBuf {
        self.root.join("state/management.sock")
    }
}
fn pump(
    cx: &mut TestAppContext,
    model: &Entity<AppModel>,
    backend: &Backend,
    condition: impl Fn(&AppModel) -> bool,
) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        while let Ok(event) = backend.events.try_recv() {
            model.update(cx, |model, cx| {
                model.apply(event, cx);
                cx.notify();
            });
        }
        cx.run_until_parked();
        if model.read_with(cx, |model, _| condition(model)) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "GUI backend timed out: {}",
            model.read_with(cx, |model, _| model.client.message.clone())
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}
fn dispatch(
    cx: &mut TestAppContext,
    model: &Entity<AppModel>,
    input: impl FnOnce(&mut astra_client::state::Client) -> Input,
) {
    model.update(cx, |model, cx| {
        let result = model.tokio(input);
        let Input::Run(action, prompt) = result else {
            panic!("expected a shared backend operation")
        };
        model.start_prompt(action, prompt.map(|p| *p), cx);
    });
}
fn call(runtime: &tokio::runtime::Runtime, socket: &Path, action: Action) {
    let reply = runtime.block_on(protocol::call(socket, action)).unwrap();
    assert!(reply.error.is_none(), "{:?}", reply.error);
}

#[gpui::test]
#[ignore = "requires prebuilt astra-host and astra-proxy; uses isolated local processes"]
fn live_window_connect_mode_proxy_disconnect_and_save(cx: &mut TestAppContext) {
    let manager = Manager::start();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let reserved = TcpListener::bind("127.0.0.1:0").unwrap();
    let proxy_address = reserved.local_addr().unwrap();
    drop(reserved);
    let source=format!("standalone=true\nstandalone_outbound='main'\nmixed_listen='{proxy_address}'\n[[outbounds]]\nkind='http'\ntag='a'\nserver='127.0.0.1:9'\n[[outbounds]]\nkind='http'\ntag='b'\nserver='127.0.0.1:9'\n[proxy_groups.main]\nmode='select'\nmembers=['a','b']\nselected='a'\n");
    let config = manager.root.join("config/live.toml");
    fs::write(&config, &source).unwrap();
    call(
        &runtime,
        &manager.socket(),
        Action::Add(Instance {
            name: "live".into(),
            kind: Kind::Proxy,
            config: config.clone(),
        }),
    );
    let options = Options::parse(
        None,
        [
            "--endpoint".to_owned(),
            manager.socket().display().to_string(),
            "--state-dir".into(),
            manager.root.join("gui").display().to_string(),
            "--config-dir".into(),
            manager.root.join("config").display().to_string(),
            "--bin-dir".into(),
            manager.host.parent().unwrap().display().to_string(),
        ],
    )
    .unwrap();
    let backend = Backend::start(options).unwrap();
    cx.update(test_support::init);
    let model = cx.new(|_| {
        AppModel::new(
            Kind::Proxy,
            backend.commands.clone(),
            backend.handle.clone(),
            backend.waker.clone(),
        )
    });
    pump(cx, &model, &backend, |model| {
        model.ready && !model.client.rows.is_empty()
    });
    model.update(cx, |model, _| {
        model.client.installation.notice = false;
        model.client.installation.open = false;
        model.client.activate_profile("live".into());
    });
    cx.update(|cx| test_support::open(model.clone(), cx).unwrap());
    cx.run_until_parked();
    let window = cx.read(|cx| cx.windows()[0]);
    cx.simulate_keystrokes(window, "c");
    pump(cx, &model, &backend, |model| model.client.live().is_some());
    dispatch(cx, &model, |client| {
        client.runtime_action(RuntimeCommand::SetMode(Mode::Global))
    });
    pump(cx, &model, &backend, |model| {
        model
            .client
            .live()
            .is_some_and(|snapshot| snapshot.mode == Mode::Global)
    });
    dispatch(cx, &model, |client| client.select_proxy_member("main", "b"));
    pump(cx, &model, &backend, |model| {
        model.client.live().is_some_and(|snapshot| {
            snapshot
                .groups
                .iter()
                .any(|group| group.name == "main" && group.selected.as_deref() == Some("b"))
        })
    });
    dispatch(cx, &model, |client| {
        client.runtime_action(RuntimeCommand::SetMode(Mode::Direct))
    });
    pump(cx, &model, &backend, |model| {
        model
            .client
            .live()
            .is_some_and(|snapshot| snapshot.mode == Mode::Direct)
    });
    // A loopback SOCKS connection keeps a real flow alive until GUI disconnects it.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let destination = listener.local_addr().unwrap();
    let mut stream = TcpStream::connect_timeout(&proxy_address, Duration::from_secs(3)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    stream.write_all(&[5, 1, 0]).unwrap();
    let mut auth = [0; 2];
    stream.read_exact(&mut auth).unwrap();
    assert_eq!(auth, [5, 0]);
    let mut request = vec![5, 1, 0, 1, 127, 0, 0, 1];
    request.extend_from_slice(&destination.port().to_be_bytes());
    stream.write_all(&request).unwrap();
    let mut response = [0; 10];
    stream.read_exact(&mut response).unwrap();
    assert_eq!(&response[..2], &[5, 0]);
    let (_accepted, _) = listener.accept().unwrap();
    pump(cx, &model, &backend, |model| {
        model
            .client
            .live()
            .is_some_and(|snapshot| !snapshot.connections.is_empty())
    });
    let id = model.read_with(cx, |model, _| {
        model.client.live().unwrap().connections[0].id
    });
    dispatch(cx, &model, |client| {
        client.runtime_action(RuntimeCommand::Disconnect(Some(id)))
    });
    pump(cx, &model, &backend, |model| {
        model.client.live().is_some_and(|snapshot| {
            snapshot
                .connections
                .iter()
                .all(|connection| connection.id != id)
        })
    });
    let changed = format!("# saved by GUI integration\n{source}");
    model.update(cx, |model, _| {
        let mut editor = astra_client::editor::Editor::new("live".into(), source.clone());
        editor.import(changed.clone()).unwrap();
        model.client.editor = Some(editor);
    });
    dispatch(cx, &model, |client| {
        client.run_command(astra_client::commands::Id::Save)
    });
    pump(cx, &model, &backend, |_| {
        fs::read_to_string(&config).ok().as_deref() == Some(changed.as_str())
    });
    // Backend shutdown drains the session checkpoint before joining the thread.
    model.update(cx, |model, cx| model.request_quit(cx));
    pump(cx, &model, &backend, |model| model.commands.is_closed());
    backend.shutdown();
    assert!(manager.root.join("gui/ui-session.json").is_file());
}
