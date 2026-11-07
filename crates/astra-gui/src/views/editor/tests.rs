use super::View;
use crate::{model::AppModel, views::ViewEvent};
use astra_client::{
    commands::Id,
    editor::{Editor, FlatRow, Mode},
    import::MergeMode,
    node_transfer::TransferAction,
    protocol::{Action, Kind},
    state::{Input as ClientInput, Prompt},
};
use gpui::{
    div, AppContext, Context, Entity, Focusable, IntoElement, ParentElement, Render, Subscription,
    TestAppContext, Window, WindowHandle,
};
use gpui_component::{input::InputEvent, Root};
use std::{cell::RefCell, rc::Rc};

type Events = Rc<RefCell<Vec<ViewEvent>>>;
struct Harness {
    view: Entity<View>,
    _subscription: Subscription,
}
impl Render for Harness {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().child(self.view.clone())
    }
}
fn fixture(
    cx: &mut TestAppContext,
    text: &str,
) -> (Entity<AppModel>, Entity<View>, WindowHandle<Root>, Events) {
    cx.update(gpui_component::init);
    let (commands, _) = async_channel::unbounded();
    let model = cx.new(|_| AppModel::new_test(Kind::Proxy, commands));
    model.update(cx, |model, _| {
        model.client.editor = Some(Editor::new("proxy".into(), text.into()).with_kind(Kind::Proxy));
    });
    let events = Rc::new(RefCell::new(vec![]));
    let mut view = None;
    let window = cx.add_window(|window, cx| {
        let host = cx.new(|cx| {
            let entity = cx.new(|cx| View::new(model.clone(), window, cx));
            view = Some(entity.clone());
            window.focus(&entity.focus_handle(cx), cx);
            let output = events.clone();
            let subscription = cx.subscribe(&entity, move |_, _, event: &ViewEvent, _| {
                output.borrow_mut().push(event.clone())
            });
            Harness {
                view: entity,
                _subscription: subscription,
            }
        });
        Root::new(host, window, cx)
    });
    cx.run_until_parked();
    (model, view.unwrap(), window, events)
}

#[gpui::test]
fn native_new_node_delete_and_undo_use_shared_history_once(cx: &mut TestAppContext) {
    let (model, view, window, _) = fixture(cx, "standalone=true\n");
    cx.simulate_keystrokes(window.into(), "n");
    assert!(model.read_with(cx, |model, _| matches!(
        model.client.editor.as_ref().unwrap().form.mode,
        Mode::NewOutbound(0)
    )));
    cx.simulate_keystrokes(window.into(), "enter");
    model.read_with(cx, |model, _| {
        let editor = model.client.editor.as_ref().unwrap();
        assert!(editor.text().contains("[[outbounds]]"));
        assert_eq!(editor.history.len(), 1);
    });
    view.update(cx, |view, cx| {
        view.edit(
            |editor| {
                editor.cancel_input();
                let rows = editor.visible_rows(&editor.document()?);
                let index = rows
                    .iter()
                    .position(|row| matches!(row, FlatRow::Node { .. }))
                    .unwrap();
                editor.select_row(index)?;
                editor.delete_selected()
            },
            cx,
        );
    });
    model.read_with(cx, |model, _| {
        assert!(!model
            .client
            .editor
            .as_ref()
            .unwrap()
            .text()
            .contains("[[outbounds]]"))
    });
    cx.simulate_keystrokes(window.into(), "ctrl-z");
    model.read_with(cx, |model, _| {
        let editor = model.client.editor.as_ref().unwrap();
        assert!(editor.text().contains("[[outbounds]]"));
        assert_eq!(editor.future.len(), 1);
    });
}

#[gpui::test]
fn source_error_retains_draft_and_native_undo_does_not_touch_document_history(
    cx: &mut TestAppContext,
) {
    let (model, view, window, events) = fixture(cx, "standalone=true\n");
    cx.simulate_keystrokes(window.into(), "tab");
    cx.simulate_keystrokes(
        window.into(),
        if cfg!(target_os = "macos") {
            "cmd-a q space p / 1 2 3"
        } else {
            "ctrl-a q space p / 1 2 3"
        },
    );
    model.read_with(cx, |model, _| {
        assert_eq!(
            model
                .client
                .editor
                .as_ref()
                .unwrap()
                .source_buffer
                .as_deref(),
            Some("q p/123")
        );
        assert!(model.client.editor.as_ref().unwrap().history.is_empty());
    });
    cx.simulate_keystrokes(window.into(), "ctrl-s");
    assert!(view.read_with(cx, |view, _| view.source_error.is_some()));
    assert!(events.borrow().is_empty());
    cx.simulate_keystrokes(window.into(), "ctrl-z");
    model.read_with(cx, |model, _| {
        assert!(model.client.editor.as_ref().unwrap().history.is_empty())
    });
    window
        .update(cx, |_, window, cx| {
            view.update(cx, |view, cx| {
                view.source.update(cx, |input, cx| {
                    input.set_value("standalone=false\n", window, cx);
                    cx.emit(InputEvent::Change);
                });
            });
        })
        .unwrap();
    cx.run_until_parked();
    cx.simulate_keystrokes(window.into(), "ctrl-s");
    assert!(events.borrow().iter().any(|event| matches!(event, ViewEvent::Input(ClientInput::Run(Action::Save { original, text, .. }, _)) if original == "standalone=true\n" && text.contains("standalone=false"))));
}

#[gpui::test]
fn conflict_compare_and_save_keep_original_disk_guard(cx: &mut TestAppContext) {
    let (model, view, window, events) = fixture(cx, "standalone=true\n");
    model.update(cx, |model, cx| {
        model
            .client
            .editor
            .as_mut()
            .unwrap()
            .import("standalone=false\n".into())
            .unwrap();
        model.client.remember_draft();
        let key = model.client.draft_key("proxy").0;
        let draft = model.client.drafts.get_mut(&key).unwrap();
        draft.conflict = true;
        draft.observed_disk = Some("standalone=true\n# changed externally\n".into());
        cx.notify();
    });
    window
        .update(cx, |_, window, cx| {
            view.update(cx, |view, cx| view.command(Id::CompareDraft, window, cx));
        })
        .unwrap();
    model.read_with(cx, |model, _| {
        let diff = model.client.help_override.as_ref().unwrap();
        assert!(diff.contains("- standalone=true"));
        assert!(diff.contains("+ standalone=false"));
    });
    window
        .update(cx, |_, window, cx| {
            view.update(cx, |view, cx| view.command(Id::Save, window, cx));
        })
        .unwrap();
    assert!(events.borrow().iter().any(|event| matches!(event, ViewEvent::Input(ClientInput::Run(Action::Save { original, text, .. }, _)) if original == "standalone=true\n" && text == "standalone=false\n")));
}

#[gpui::test]
fn node_import_picker_merges_selection_as_one_undoable_edit(cx: &mut TestAppContext) {
    let (model, view, window, _) = fixture(cx, "standalone=true\n");
    cx.simulate_keystrokes(window.into(), "ctrl-i");
    assert!(model.read_with(cx, |model, _| matches!(
        model.client.prompt,
        Some(Prompt::ImportNode(MergeMode::Append))
    )));
    model.update(cx, |model, cx| {
        model.client.prepare_node_import("[[outbounds]]\ntag='first'\nkind='socks5'\nserver='127.0.0.1:1080'\n[[outbounds]]\ntag='second'\nkind='http'\nserver='127.0.0.1:8080'\n", MergeMode::Append).unwrap();
        model.client.node_transfer(TransferAction::ToggleIndex(0));
        assert!(model.client.submit_prompt(String::new()).is_ok()); cx.notify();
    });
    cx.run_until_parked();
    model.read_with(cx, |model, _| {
        let editor = model.client.editor.as_ref().unwrap();
        assert!(editor.text().contains("second"));
        assert!(!editor.text().contains("first"));
        assert_eq!(editor.history.len(), 1);
    });
    window
        .update(cx, |_, window, cx| {
            view.update(cx, |view, cx| view.command(Id::Undo, window, cx));
        })
        .unwrap();
    model.read_with(cx, |model, _| {
        assert_eq!(
            model.client.editor.as_ref().unwrap().text(),
            "standalone=true\n"
        )
    });
}

#[gpui::test]
fn disabled_vim_keys_do_not_move_tree_and_leave_does_not_accept_new_node(cx: &mut TestAppContext) {
    let (model, view, window, _) = fixture(cx, "standalone=true\n");
    model.update(cx, |model, _| model.client.vim_keys = false);
    cx.simulate_keystrokes(window.into(), "j z shift-r");
    assert_eq!(
        model.read_with(cx, |model, _| model
            .client
            .editor
            .as_ref()
            .unwrap()
            .form
            .selected),
        0
    );
    cx.simulate_keystrokes(window.into(), "n");
    let close = window
        .update(cx, |_, window, cx| {
            view.update(cx, |view, cx| view.prepare_leave(window, cx))
        })
        .unwrap();
    assert!(!close);
    model.read_with(cx, |model, _| {
        assert!(!model
            .client
            .editor
            .as_ref()
            .unwrap()
            .text()
            .contains("[[outbounds]]"));
        assert!(matches!(
            model.client.editor.as_ref().unwrap().form.mode,
            Mode::NewOutbound(_)
        ));
    });
}
