use super::*;
use crate::views::{connections, overview, proxies, recovery, rules, server};
use astra_client::{
    protocol::{Action, Instance, Row},
    state::{Prompt, Resume},
};
use astra_model::runtime::{Command, Connection, Group, Member, Snapshot};
use gpui::{TestAppContext, WindowHandle};
use gpui_component::Root;
use std::{cell::RefCell, rc::Rc};

type Events = Rc<RefCell<Vec<ViewEvent>>>;
struct Harness<V: Render + EventEmitter<ViewEvent> + 'static> {
    view: Entity<V>,
    subscription: Subscription,
}
impl<V: Render + EventEmitter<ViewEvent> + 'static> Render for Harness<V> {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        let _ = &self.subscription;
        div().size_full().child(self.view.clone())
    }
}
fn model(cx: &mut TestAppContext) -> Entity<AppModel> {
    let (commands, _) = async_channel::unbounded();
    let model = cx.new(|_| AppModel::new_test(Kind::Proxy, commands));
    model.update(cx, |model, _| {
        model.client.rows = vec![Row {
            instance: Instance {
                name: "proxy".into(),
                kind: Kind::Proxy,
                config: "proxy.toml".into(),
            },
            pid: Some(42),
            state: "Running".into(),
            uptime: 0,
            changed: false,
        }];
        model.client.accept_runtime(
            "proxy".into(),
            Snapshot {
                pid: 42,
                groups: vec![Group {
                    name: "PROXY".into(),
                    mode: "select".into(),
                    selected: Some("first".into()),
                    testing: false,
                    members: vec![
                        Member {
                            name: "first".into(),
                            delay_ms: Some(10),
                            alive: Some(true),
                        },
                        Member {
                            name: "second".into(),
                            delay_ms: None,
                            alive: None,
                        },
                    ],
                }],
                connections: vec![Connection {
                    id: 7,
                    host: "example.com".into(),
                    port: 443,
                    source: None,
                    network: "Tcp".into(),
                    inbound: "mixed".into(),
                    outbound: "first".into(),
                    upload: 1,
                    download: 2,
                    elapsed_ms: 3000,
                }],
                ..Default::default()
            },
        );
    });
    model
}
fn fixture<V: Render + Focusable + EventEmitter<ViewEvent> + 'static>(
    cx: &mut TestAppContext,
    model: Entity<AppModel>,
    new: fn(Entity<AppModel>, &mut Window, &mut Context<V>) -> V,
) -> (Entity<V>, WindowHandle<Root>, Events) {
    let events = Rc::new(RefCell::new(vec![]));
    let mut entity = None;
    let window = cx.add_window(|window, cx| {
        let host = cx.new(|cx| {
            let view = cx.new(|cx| new(model, window, cx));
            window.focus(&view.focus_handle(cx), cx);
            entity = Some(view.clone());
            let output = events.clone();
            let subscription = cx.subscribe(&view, move |_, _, event: &ViewEvent, _| {
                output.borrow_mut().push(event.clone())
            });
            Harness { view, subscription }
        });
        Root::new(host, window, cx)
    });
    cx.run_until_parked();
    (entity.unwrap(), window, events)
}
#[gpui::test]
fn native_page_controls_emit_shared_actions_and_preserve_runtime_identity(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let model = model(cx);
    let (_, window, events) = fixture(cx, model.clone(), overview::View::new);
    cx.simulate_keystrokes(window.into(), "m shift-s");
    assert_eq!(events.borrow().len(), 2);
    assert!(events.borrow().iter().all(|event|matches!(event,ViewEvent::Input(ClientInput::Run(Action::Runtime{name,expected_pid:Some(42),..},_))if name=="proxy")));
    let (view, _, events) = fixture(cx, model.clone(), proxies::View::new);
    view.update(cx, |view, cx| {
        view.choose("PROXY".into(), "second".into(), cx)
    });
    assert!(
        matches!(&events.borrow()[0],ViewEvent::Input(ClientInput::Run(Action::Runtime{command:Command::Select{member,..},..},_))if member=="second")
    );
    assert!(model.read_with(cx, |model, _| model
        .client
        .proxy_member_pending("PROXY", "second")));
}
#[gpui::test]
fn profile_search_keeps_single_letter_shortcuts_as_text(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let model = model(cx);
    let (view, window, events) = fixture(cx, model.clone(), View::new);
    window
        .update(cx, |_, window, cx| {
            view.update(cx, |view, cx| {
                view.search.update(cx, |input, cx| input.focus(window, cx))
            });
        })
        .unwrap();
    cx.simulate_keystrokes(window.into(), "n space c r e");
    assert!(events.borrow().is_empty());
    assert_eq!(
        view.read_with(cx, |view, cx| view.search.read(cx).value().to_string()),
        "n cre"
    );
    assert!(model.read_with(cx, |model, _| model.client.prompt.is_none()));
    window
        .update(cx, |_, window, cx| window.focus(&view.focus_handle(cx), cx))
        .unwrap();
    cx.simulate_keystrokes(window.into(), "n");
    assert!(model.read_with(cx, |model, _| matches!(
        model.client.prompt,
        Some(Prompt::AddName)
    )));
}
#[gpui::test]
fn connection_menu_rechecks_its_captured_target_after_a_restart(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let model = model(cx);
    let (view, _, events) = fixture(cx, model.clone(), connections::View::new);
    let target = model.read_with(cx, |model, _| {
        model
            .client
            .connection_action_target(&astra_client::connections::ConnectionRowKey::Connection(7))
            .unwrap()
    });
    model.update(cx, |model, _| model.client.rows[0].pid = Some(99));
    view.update(cx, |view, cx| view.close(target, cx));
    assert!(matches!(
        &events.borrow()[0],
        ViewEvent::Input(ClientInput::Idle)
    ));
}
#[gpui::test]
fn server_page_has_no_proxy_fallback_and_recovery_retries_the_saved_operation(
    cx: &mut TestAppContext,
) {
    cx.update(gpui_component::init);
    let model = model(cx);
    let (view, _, events) = fixture(cx, model.clone(), server::View::new);
    view.update(cx, |view, cx| {
        view.action(astra_client::intents::ServerAction::Toggle, cx)
    });
    assert!(matches!(
        &events.borrow()[0],
        ViewEvent::Input(ClientInput::Idle)
    ));
    model.update(cx, |model, _| {
        model.client.fail(
            "address in use",
            Resume::Request(Action::Start("proxy".into())),
        )
    });
    let (_, window, events) = fixture(cx, model.clone(), recovery::View::new);
    cx.simulate_keystrokes(window.into(), "c");
    assert!(
        matches!(&events.borrow()[0],ViewEvent::Input(ClientInput::Run(Action::Start(name),None))if name=="proxy")
    );
    let _ = fixture(cx, model, rules::View::new);
}

#[gpui::test]
fn large_connection_and_profile_tables_render_through_virtual_lists(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let model = model(cx);
    model.update(cx, |model, _| {
        let template = model.client.rows[0].clone();
        for index in 0..2000 {
            let mut row = template.clone();
            row.instance.name = format!("profile-{index}");
            model.client.rows.push(row);
        }
        let snapshot = model.client.runtime_snapshot.as_mut().unwrap();
        let template = snapshot.connections[0].clone();
        for index in 1..2001 {
            let mut connection = template.clone();
            connection.id = index + 10;
            connection.host = format!("host-{index}.example.com");
            snapshot.connections.push(connection);
        }
    });
    let _ = fixture(cx, model.clone(), connections::View::new);
    let _ = fixture(cx, model, View::new);
    cx.run_until_parked();
}

#[gpui::test]
fn observability_logs_route_commands_to_the_visible_source(cx: &mut TestAppContext) {
    use crate::views::logs;
    cx.update(gpui_component::init);
    let model = model(cx);
    model.update(cx, |model, _| {
        model.client.viewer = Some(astra_client::logs::Viewer::new(
            "proxy".into(),
            Default::default(),
        ));
        let mut frontend = astra_client::logs::Viewer::new("@gui".into(), Default::default());
        for index in 0..2000 {
            frontend.records.push_back(astra_client::logs::Record {
                text: format!("INFO event {index} {}", "a long record ".repeat(20)),
                level: None,
            });
        }
        frontend.settings.wrap = true;
        model.client.tui_viewer = Some(frontend);
    });
    let (view, window, events) = fixture(cx, model.clone(), logs::View::frontend);
    cx.simulate_keystrokes(window.into(), "space r");
    assert!(model.read_with(cx, |model, _| model
        .client
        .tui_viewer
        .as_ref()
        .unwrap()
        .paused()));
    assert!(!model.read_with(cx, |model, _| model
        .client
        .viewer
        .as_ref()
        .unwrap()
        .paused()));
    assert!(events.borrow().iter().any(|event|matches!(event,ViewEvent::Input(ClientInput::Run(Action::Logs(name),None))if name=="@gui")));
    view.update(cx, |view, cx| {
        view.command(astra_client::logs::Command::Close, cx)
    });
    assert!(model.read_with(cx, |model, _| model.client.tui_viewer.is_none()));
    assert!(model.read_with(cx, |model, _| model.client.viewer.is_some()));
    assert!(events
        .borrow()
        .iter()
        .any(|event| matches!(event, ViewEvent::Dismiss)));
}
#[gpui::test]
fn observability_tasks_cancel_only_running_records_and_refresh_updates(cx: &mut TestAppContext) {
    use crate::views::tasks;
    cx.update(gpui_component::init);
    let model = model(cx);
    model.update(cx, |model, _| {
        for id in 0..32 {
            model
                .client
                .tasks
                .records
                .push_back(astra_client::tasks::Record {
                    id,
                    title: format!("Task {id}"),
                    started: std::time::Instant::now(),
                    finished: (id != 31).then(std::time::Instant::now),
                    events: (0..128).map(|i| format!("event {i}")).collect(),
                });
        }
    });
    let (view, window, events) = fixture(cx, model, tasks::View::new);
    view.update(cx, |view, cx| {
        view.cancel(0, cx);
        view.cancel(31, cx);
        view.cancel(99, cx);
    });
    assert_eq!(
        events
            .borrow()
            .iter()
            .filter(|e| matches!(e, ViewEvent::Cancel(31)))
            .count(),
        1
    );
    assert_eq!(events.borrow().len(), 1);
    cx.simulate_keystrokes(window.into(), "f5");
    assert!(events.borrow().iter().any(|event| matches!(
        event,
        ViewEvent::Input(ClientInput::Run(Action::UpdateStatus, None))
    )));
}
#[gpui::test]
fn observability_monitor_and_network_tools_use_shared_models(cx: &mut TestAppContext) {
    use crate::views::{diagnostics, monitor, nettools};
    cx.update(gpui_component::init);
    let model = model(cx);
    let (_, window, _) = fixture(cx, model.clone(), monitor::View::new);
    assert_eq!(
        model.read_with(cx, |m, _| m.client.monitor.as_ref().unwrap().name.clone()),
        "proxy"
    );
    cx.simulate_keystrokes(window.into(), "space c");
    assert!(model.read_with(cx, |m, _| m.client.monitor.as_ref().unwrap().paused));
    model.update(cx, |model, _| {
        model.tokio(|c| {
            let mut diagnostics = astra_client::diagnostics::View::start(
                std::path::Path::new("/missing-astra-socket"),
                "proxy".into(),
            );
            if let Some(task) = diagnostics.task.take() {
                task.abort();
            }
            diagnostics.receiver = None;
            diagnostics.loading = false;
            diagnostics.targets = vec!["first".into(), "second".into()];
            diagnostics.select_all(true);
            c.diagnostics = Some(diagnostics);
        })
    });
    fixture(cx, model.clone(), diagnostics::View::new);
    model.update(cx, |model, _| {
        model.tokio(|c| c.diagnostics.as_mut().unwrap().open_tools_page())
    });
    let (view, window, _) = fixture(cx, model.clone(), nettools::View::new);
    window
        .update(cx, |_, window, cx| {
            view.update(cx, |view, cx| {
                view.set_kind(astra_client::nettools::Kind::Dns, window, cx)
            })
        })
        .unwrap();
    assert!(model.read_with(cx, |m, _| m
        .client
        .diagnostics
        .as_ref()
        .unwrap()
        .tools
        .as_ref()
        .unwrap()
        .fields()
        .contains(&astra_client::nettools::Field::Record)));
    view.update(cx, |view, cx| view.start(cx));
    assert!(model.read_with(cx, |m, _| m
        .client
        .diagnostics
        .as_ref()
        .unwrap()
        .tools
        .as_ref()
        .unwrap()
        .running()));
}
