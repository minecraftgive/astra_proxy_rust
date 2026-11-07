use super::{keys, AppModel, AppWindow, Id, Kind, Panel, Root};
use astra_client::editor::{Editor, Mode};
use gpui::{AppContext, Focusable, TestAppContext};

#[gpui::test]
fn dialog_focus_blocks_application_shortcuts_and_enter_submits(cx: &mut TestAppContext) {
    cx.update(|cx| {
        gpui_component::init(cx);
        cx.bind_keys(keys::bindings(cx));
    });
    let (commands, receiver) = async_channel::unbounded();
    let model = cx.new(|_| AppModel::new_test(Kind::Proxy, commands));
    model.update(cx, |model, _| {
        model.client.prompt(astra_client::state::Prompt::Shutdown)
    });
    let handle = cx.add_window(|window, cx| {
        let view = cx.new(|cx| AppWindow::new(model.clone(), Some(0), window, cx));
        Root::new(view, window, cx)
    });
    cx.run_until_parked();
    cx.simulate_keystrokes(handle.into(), "q 1 / space");
    model.update(cx, |model, _| {
        assert_eq!(model.client.page, 0);
        assert!(matches!(
            model.client.prompt,
            Some(astra_client::state::Prompt::Shutdown)
        ));
    });
    assert!(!receiver.is_closed());
    cx.simulate_keystrokes(handle.into(), "escape");
    model.update(cx, |model, cx| {
        model.client.prompt(astra_client::state::Prompt::AddName);
        cx.notify();
    });
    cx.run_until_parked();
    cx.simulate_keystrokes(handle.into(), "q 1 enter");
    model.update(cx, |model, _| assert!(model.client.prompt.is_none()));
    let crate::backend::BackendCmd::Start(request) = receiver.try_recv().unwrap() else {
        panic!("submitting the dialog must dispatch the shared operation");
    };
    assert!(
        matches!(request.action, astra_client::protocol::Action::Create { name, .. } if name == "q1")
    );
    assert!(!receiver.is_closed());
}

#[gpui::test]
fn shortcuts_keep_working_after_palette_result_and_navigation(cx: &mut TestAppContext) {
    cx.update(|cx| {
        gpui_component::init(cx);
        cx.bind_keys(keys::bindings(cx));
    });
    let (commands, _) = async_channel::unbounded();
    let model = cx.new(|_| AppModel::new_test(Kind::Proxy, commands));
    let mut app = None;
    let handle = cx.add_window(|window, cx| {
        let view = cx.new(|cx| AppWindow::new(model, Some(0), window, cx));
        app = Some(view.clone());
        Root::new(view, window, cx)
    });
    let app = app.unwrap();
    cx.run_until_parked();
    cx.simulate_keystrokes(handle.into(), "ctrl-k");
    handle
        .update(cx, |_, window, cx| {
            app.update(cx, |app, cx| {
                assert!(app.palette);
                assert!(app.search.read(cx).focus_handle(cx).is_focused(window));
                // The same path used by clicking a palette result.
                app.execute(Id::Tasks, window, cx);
                assert!(!app.palette);
                assert_eq!(app.panel, Some(Panel::Tasks));
                assert!(
                    app.focus.is_focused(window)
                        || app
                            .mounted
                            .as_ref()
                            .is_some_and(|mounted| mounted.focus.is_focused(window))
                );
            })
        })
        .unwrap();
    cx.simulate_keystrokes(handle.into(), "escape 7 f1");
    app.update(cx, |app, _| {
        assert_eq!(app.page, Some(6));
        assert_eq!(app.panel, Some(Panel::Help));
    });
    cx.simulate_keystrokes(handle.into(), "ctrl-k");
    handle
        .update(cx, |_, window, cx| {
            app.update(cx, |app, cx| {
                assert!(app.palette);
                // The same path used by clicking the sidebar while search has focus.
                app.navigate(5, window, cx);
                assert!(
                    app.focus.is_focused(window)
                        || app
                            .mounted
                            .as_ref()
                            .is_some_and(|mounted| mounted.focus.is_focused(window))
                );
            })
        })
        .unwrap();
    cx.simulate_keystrokes(handle.into(), "f9");
    app.update(cx, |app, _| assert_eq!(app.panel, Some(Panel::Monitor)));
    cx.simulate_keystrokes(handle.into(), "f8 alt-7");
    assert_eq!(cx.read(|cx| cx.windows().len()), 3);
    app.update(cx, |app, _| assert_eq!(app.page, Some(5)));
}

#[gpui::test]
fn printable_shortcuts_are_text_in_search_fields(cx: &mut TestAppContext) {
    cx.update(|cx| {
        gpui_component::init(cx);
        cx.bind_keys(keys::bindings(cx));
    });
    let (commands, receiver) = async_channel::unbounded();
    let model = cx.new(|_| AppModel::new_test(Kind::Proxy, commands));
    model.update(cx, |model, _| {
        let mut editor = Editor::new("test".into(), "standalone = true\n".into());
        editor.history.push("standalone = false\n".into());
        model.client.editor = Some(editor);
    });
    let mut app = None;
    let handle = cx.add_window(|window, cx| {
        let view = cx.new(|cx| AppWindow::new(model.clone(), Some(0), window, cx));
        app = Some(view.clone());
        Root::new(view, window, cx)
    });
    let app = app.unwrap();
    for command in [Id::Palette, Id::Profiles] {
        handle
            .update(cx, |_, window, cx| {
                app.update(cx, |app, cx| app.execute(command, window, cx));
            })
            .unwrap();
        cx.run_until_parked();
        cx.simulate_keystrokes(handle.into(), "q p / space 1 2 3 4 5 6 7");
        app.update(cx, |app, cx| {
            assert_eq!(app.search.read(cx).value().as_str(), "qp/ 1234567");
            assert_eq!(app.page, Some(0));
            assert_eq!(app.palette, command == Id::Palette);
            assert_eq!(app.profile_chooser, command == Id::Profiles);
        });
        assert!(!receiver.is_closed(), "typing must not quit the app");
        for chord in ["ctrl-z", "ctrl-y"] {
            cx.simulate_keystrokes(handle.into(), chord);
            model.update(cx, |model, _| {
                assert!(!model.client.refresh_paused);
                let editor = model.client.editor.as_ref().unwrap();
                assert_eq!(editor.text(), "standalone = true\n");
                assert_eq!(editor.history.len(), 1);
                assert!(editor.future.is_empty());
                assert!(matches!(editor.form.mode, Mode::Browse));
            });
        }
        // Non-text app shortcuts remain available from either search field.
        cx.simulate_keystrokes(handle.into(), "f6");
        handle
            .update(cx, |_, window, cx| {
                app.update(cx, |app, _| {
                    assert!(!app.palette);
                    assert!(!app.profile_chooser);
                    assert_eq!(app.panel, Some(Panel::Tasks));
                    assert!(
                        app.focus.is_focused(window)
                            || app
                                .mounted
                                .as_ref()
                                .is_some_and(|mounted| mounted.focus.is_focused(window))
                    );
                });
            })
            .unwrap();
    }
}

#[gpui::test]
fn only_the_visible_focused_editor_receives_keys_and_undo_runs_once(cx: &mut TestAppContext) {
    cx.update(|cx| {
        gpui_component::init(cx);
        cx.bind_keys(keys::bindings(cx));
    });
    let (commands, receiver) = async_channel::unbounded();
    let model = cx.new(|_| AppModel::new_test(Kind::Proxy, commands));
    let original = "# original\nstandalone = true\n";
    let previous = "# previous\nstandalone = true\n";
    let current = "# current\nstandalone = true\n";
    model.update(cx, |model, _| {
        let mut editor = Editor::new("test".into(), current.into());
        editor.history = vec![original.into(), previous.into()];
        model.client.editor = Some(editor);
    });
    let mut app = None;
    let handle = cx.add_window(|window, cx| {
        let view = cx.new(|cx| AppWindow::new(model.clone(), Some(0), window, cx));
        app = Some(view.clone());
        Root::new(view, window, cx)
    });
    let app = app.unwrap();
    cx.run_until_parked();
    // Retaining a draft while another page is visible must not make it editable.
    cx.simulate_keystrokes(handle.into(), "u ctrl-z");
    model.update(cx, |model, _| {
        let editor = model.client.editor.as_ref().unwrap();
        assert_eq!(editor.text(), current);
        assert_eq!(editor.history.len(), 2);
    });
    handle
        .update(cx, |_, window, cx| {
            app.update(cx, |app, cx| {
                app.panel = Some(Panel::Editor);
                window.focus(&app.focus, cx);
                cx.notify();
            });
        })
        .unwrap();
    cx.run_until_parked();
    cx.simulate_keystrokes(handle.into(), "ctrl-z");
    model.update(cx, |model, _| {
        let editor = model.client.editor.as_ref().unwrap();
        assert_eq!(editor.text(), previous);
        assert_eq!(editor.history, [original]);
        assert_eq!(editor.future, [current]);
    });
    // Raw editor keys still work, including printable application shortcuts in
    // the shared editor's own text-entry mode.
    cx.simulate_keystrokes(handle.into(), "/ q p space 1 2 3 4 5 6 7");
    model.update(cx, |model, _| {
        let Mode::Search(input) = &model.client.editor.as_ref().unwrap().form.mode else {
            panic!("the focused editor must receive its search key");
        };
        assert_eq!(input.text, "qp 1234567");
        assert!(!model.client.refresh_paused);
    });
    app.update(cx, |app, _| {
        assert_eq!(app.page, Some(0));
        assert_eq!(app.panel, Some(Panel::Editor));
        assert!(!app.profile_chooser);
    });
    assert!(!receiver.is_closed());
}

#[gpui::test]
fn server_windows_reject_proxy_pages_in_navigation_and_shortcuts(cx: &mut TestAppContext) {
    cx.update(|cx| {
        gpui_component::init(cx);
        cx.bind_keys(keys::bindings(cx));
    });
    let (commands, _) = async_channel::unbounded();
    let model = cx.new(|_| AppModel::new_test(Kind::Server, commands));
    let mut app = None;
    let handle = cx.add_window(|window, cx| {
        let view = cx.new(|cx| AppWindow::new(model.clone(), None, window, cx));
        app = Some(view.clone());
        Root::new(view, window, cx)
    });
    let app = app.unwrap();
    cx.run_until_parked();
    cx.simulate_keystrokes(handle.into(), "1 2 3 4 alt-1 alt-2 alt-3 alt-4");
    assert_eq!(cx.read(|cx| cx.windows().len()), 1);
    app.read_with(cx, |app, cx| {
        assert_eq!(app.page.unwrap_or(app.model.read(cx).client.page), 5)
    });
    handle
        .update(cx, |_, window, cx| {
            app.update(cx, |app, cx| {
                for page in 0..4 {
                    app.navigate(page, window, cx);
                }
                assert_ne!(app.page, Some(0));
            })
        })
        .unwrap();
    cx.simulate_keystrokes(handle.into(), "7 alt-7");
    assert_eq!(cx.read(|cx| cx.windows().len()), 2);
    app.read_with(cx, |app, _| assert_eq!(app.page, Some(6)));
}
