use super::{AppModel, AppWindow, Panel};
use crate::{
    backend::{BackendCmd, BackendEvent, Start},
    keys,
};
use astra_client::{
    operations::JobResult,
    protocol::{Action, Instance, Kind, Row},
    state::{Input, Prompt},
};
use gpui::{AppContext, Entity, TestAppContext, WindowHandle};
use gpui_component::Root;

type Window = (Entity<AppWindow>, WindowHandle<Root>);
fn fixture(
    cx: &mut TestAppContext,
) -> (
    Entity<AppModel>,
    async_channel::Receiver<BackendCmd>,
    Window,
    Window,
) {
    cx.update(|cx| {
        gpui_component::init(cx);
        cx.bind_keys(keys::bindings(cx));
    });
    let (sender, receiver) = async_channel::unbounded();
    let model = cx.new(|_| AppModel::new_test(Kind::Proxy, sender));
    model.update(cx, |model, _| {
        model.ready = true;
        model.client.rows.push(Row {
            instance: Instance {
                name: "proxy".into(),
                kind: Kind::Proxy,
                config: "proxy.toml".into(),
            },
            pid: None,
            state: "Stopped".into(),
            uptime: 0,
            changed: false,
        });
    });
    let first = add_window(cx, model.clone(), 0);
    let second = add_window(cx, model.clone(), 5);
    cx.run_until_parked();
    (model, receiver, first, second)
}
fn add_window(cx: &mut TestAppContext, model: Entity<AppModel>, page: usize) -> Window {
    let mut app = None;
    let window = cx.add_window(|window, cx| {
        let view = cx.new(|cx| AppWindow::new(model, Some(page), window, cx));
        app = Some(view.clone());
        Root::new(view, window, cx)
    });
    (app.unwrap(), window)
}
fn request(
    cx: &mut TestAppContext,
    window: &Window,
    receiver: &async_channel::Receiver<BackendCmd>,
) -> Start {
    window
        .1
        .update(cx, |_, native, cx| {
            window.0.update(cx, |app, cx| {
                app.handle_input(Input::Run(Action::Read("proxy".into()), None), native, cx)
            })
        })
        .unwrap();
    let BackendCmd::Start(request) = receiver.try_recv().unwrap() else {
        panic!("read request")
    };
    *request
}
fn complete(cx: &mut TestAppContext, model: &Entity<AppModel>, request: Start) {
    let prepared = request
        .input
        .prepare("proxy".into(), "standalone=true\n".into());
    model.update(cx, |model, cx| {
        model.apply(
            BackendEvent::JobDone {
                id: request.id,
                action: Box::new(request.action),
                result: Box::new(JobResult::Document {
                    prepared,
                    binding: None,
                }),
                origin: request.origin,
            },
            cx,
        );
        cx.notify();
    });
    cx.run_until_parked();
}

#[gpui::test]
fn async_editor_opens_only_its_originating_window_after_other_window_navigation(
    cx: &mut TestAppContext,
) {
    let (model, receiver, first, second) = fixture(cx);
    let read = request(cx, &first, &receiver);
    second
        .1
        .update(cx, |_, window, cx| {
            second.0.update(cx, |app, cx| app.navigate(6, window, cx))
        })
        .unwrap();
    complete(cx, &model, read);
    first.0.read_with(cx, |app, _| {
        assert_eq!(app.page, Some(0));
        assert_eq!(app.panel, Some(Panel::Editor));
    });
    second.0.read_with(cx, |app, _| {
        assert_eq!(app.page, Some(6));
        assert_eq!(app.panel, None);
    });
    model.update(cx, |model, cx| {
        model
            .client
            .editor
            .as_mut()
            .unwrap()
            .set_source_buffer("broken = [".into());
        model.client.remember_draft();
        cx.notify();
    });
    second
        .1
        .update(cx, |_, window, cx| {
            second.0.update(cx, |app, cx| app.navigate(2, window, cx))
        })
        .unwrap();
    model.read_with(cx, |model, _| {
        assert_eq!(
            model
                .client
                .editor
                .as_ref()
                .unwrap()
                .source_buffer
                .as_deref(),
            Some("broken = [")
        )
    });
    first
        .1
        .update(cx, |_, window, cx| {
            first.0.update(cx, |app, cx| {
                assert!(!app.leave_editor(window, cx));
            })
        })
        .unwrap();
}

#[gpui::test]
fn ordinary_view_events_keep_local_page_and_late_results_do_not_reopen_departed_editor(
    cx: &mut TestAppContext,
) {
    let (model, receiver, first, second) = fixture(cx);
    second
        .1
        .update(cx, |_, window, cx| {
            second.0.update(cx, |app, cx| app.navigate(6, window, cx))
        })
        .unwrap();
    first
        .1
        .update(cx, |_, window, cx| {
            first
                .0
                .update(cx, |app, cx| app.handle_input(Input::Idle, window, cx))
        })
        .unwrap();
    first
        .0
        .read_with(cx, |app, _| assert_eq!(app.page, Some(0)));
    let read = request(cx, &first, &receiver);
    first
        .1
        .update(cx, |_, window, cx| {
            first.0.update(cx, |app, cx| app.navigate(2, window, cx))
        })
        .unwrap();
    complete(cx, &model, read);
    first.0.read_with(cx, |app, _| {
        assert_eq!(app.page, Some(2));
        assert_eq!(app.panel, None);
    });
    second.0.read_with(cx, |app, _| assert_eq!(app.panel, None));
    model.read_with(cx, |model, _| {
        assert!(model.client.editor.is_none());
        assert!(model.client.ready_editors.contains_key("proxy"));
    });
}

#[gpui::test]
fn dialog_owner_keeps_other_window_shortcuts_and_search_targets_native_editor(
    cx: &mut TestAppContext,
) {
    let (model, receiver, first, second) = fixture(cx);
    model.update(cx, |model, _| model.client.prompt(Prompt::AddName));
    second
        .1
        .update(cx, |_, window, cx| {
            second
                .0
                .update(cx, |app, cx| app.handle_input(Input::Idle, window, cx))
        })
        .unwrap();
    cx.run_until_parked();
    cx.simulate_keystrokes(first.1.into(), "7");
    first
        .0
        .read_with(cx, |app, _| assert_eq!(app.page, Some(6)));
    cx.simulate_keystrokes(second.1.into(), "q / 1");
    model.read_with(cx, |model, _| assert_eq!(model.client.input, "q/1"));
    model.update(cx, |model, cx| {
        model.client.cancel_prompt();
        cx.notify();
    });
    let read = request(cx, &first, &receiver);
    complete(cx, &model, read);
    first
        .1
        .update(cx, |_, window, cx| {
            first.0.update(cx, |app, cx| {
                app.execute(astra_client::commands::Id::Search, window, cx)
            })
        })
        .unwrap();
    model.read_with(cx, |model, _| {
        assert!(matches!(
            model.client.editor.as_ref().unwrap().form.mode,
            astra_client::editor::Mode::Search(_)
        ))
    });
    assert!(!model.read_with(cx, |model, _| model.client.connection_filtering));
}

#[gpui::test]
fn selecting_server_kind_in_profiles_does_not_turn_the_regular_app_into_server_mode(
    cx: &mut TestAppContext,
) {
    let (model, _, first, _) = fixture(cx);
    model.update(cx, |model, _| model.client.switch_core(Kind::Server));
    first
        .1
        .update(cx, |_, window, cx| {
            first.0.update(cx, |app, cx| app.navigate(5, window, cx))
        })
        .unwrap();
    cx.run_until_parked();
    first.0.read_with(cx, |app, _| {
        assert!(app
            .mounted
            .as_ref()
            .unwrap()
            .view
            .clone()
            .downcast::<crate::views::profiles::View>()
            .is_ok())
    });
    first
        .1
        .update(cx, |_, window, cx| {
            first.0.update(cx, |app, cx| app.navigate(0, window, cx))
        })
        .unwrap();
    first
        .0
        .read_with(cx, |app, _| assert_eq!(app.page, Some(0)));
    assert!(!model.read_with(cx, |model, _| model.server_mode));
}

#[gpui::test]
fn deferred_frontend_log_result_is_owned_by_the_requesting_window(cx: &mut TestAppContext) {
    let (model, receiver, first, second) = fixture(cx);
    first
        .1
        .update(cx, |_, window, cx| {
            first
                .0
                .update(cx, |app, cx| app.show_panel(Panel::GuiLog, window, cx))
        })
        .unwrap();
    cx.run_until_parked();
    let BackendCmd::Start(request) = receiver.try_recv().unwrap() else {
        panic!("frontend log request")
    };
    assert!(matches!(&request.action, Action::Logs(name) if name == "@gui"));
    second
        .1
        .update(cx, |_, window, cx| {
            second.0.update(cx, |app, cx| app.navigate(6, window, cx))
        })
        .unwrap();
    model.update(cx, |model, cx| {
        model.apply(
            BackendEvent::JobDone {
                id: request.id,
                action: Box::new(request.action),
                result: Box::new(JobResult::Logs(astra_client::logs::Viewer::new(
                    "@gui".into(),
                    Default::default(),
                ))),
                origin: request.origin,
            },
            cx,
        );
        cx.notify();
    });
    cx.run_until_parked();
    model.read_with(cx, |model, _| {
        assert_eq!(model.client.tui_viewer.as_ref().unwrap().name, "@gui")
    });
    first
        .0
        .read_with(cx, |app, _| assert_eq!(app.panel, Some(Panel::GuiLog)));
    second.0.read_with(cx, |app, _| {
        assert_eq!(app.panel, None);
        assert_eq!(app.page, Some(6));
    });
}
