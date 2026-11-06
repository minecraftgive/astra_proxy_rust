use super::{PromptEvent, PromptHost};
use crate::model::AppModel;
use astra_client::{
    protocol::{Action, Kind},
    state::{Input as ClientInput, Prompt},
};
use gpui::{
    div, AppContext, Context, Entity, IntoElement, ParentElement, Render, Subscription,
    TestAppContext, Window, WindowHandle,
};
use gpui_component::Root;
use std::{cell::RefCell, rc::Rc};

struct Harness {
    prompt: Entity<PromptHost>,
    _subscription: Subscription,
}
impl Render for Harness {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().child(self.prompt.clone())
    }
}
type Events = Rc<RefCell<Vec<PromptEvent>>>;
fn fixture(
    cx: &mut TestAppContext,
    initial: Prompt,
) -> (
    Entity<AppModel>,
    Entity<PromptHost>,
    WindowHandle<Root>,
    Events,
) {
    cx.update(gpui_component::init);
    let (commands, _) = async_channel::unbounded();
    let model = cx.new(|_| AppModel::new_test(Kind::Proxy, commands));
    model.update(cx, |model, _| model.client.prompt(initial));
    let events = Rc::new(RefCell::new(vec![]));
    let mut host = None;
    let window = cx.add_window(|window, cx| {
        let view = cx.new(|cx| {
            let prompt = cx.new(|cx| PromptHost::new(model.clone(), window, cx));
            host = Some(prompt.clone());
            let output = events.clone();
            let subscription =
                cx.subscribe_in(&prompt, window, move |_, _, event: &PromptEvent, _, _| {
                    output.borrow_mut().push(event.clone())
                });
            Harness {
                prompt,
                _subscription: subscription,
            }
        });
        Root::new(view, window, cx)
    });
    cx.run_until_parked();
    (model, host.unwrap(), window, events)
}

#[gpui::test]
fn validation_stays_inline_and_enter_submits_exact_shared_origin(cx: &mut TestAppContext) {
    let (model, host, window, events) = fixture(cx, Prompt::AddName);
    cx.simulate_keystrokes(window.into(), "q space 1 / enter");
    assert!(host.read_with(cx, |host, _| host.error.is_some()));
    model.read_with(cx, |model, _| {
        assert!(matches!(model.client.prompt, Some(Prompt::AddName)));
        assert_eq!(model.client.input, "q 1/");
    });
    assert!(events.borrow().is_empty());
    window
        .update(cx, |_, window, cx| {
            host.update(cx, |host, cx| {
                host.edit("sample".into(), cx);
                host.reconcile(window, cx);
            })
        })
        .unwrap();
    cx.run_until_parked();
    cx.simulate_keystrokes(window.into(), "enter");
    assert!(model.read_with(cx, |model, _| model.client.prompt.is_none()));
    assert!(
        matches!(&events.borrow()[0], PromptEvent::Input(ClientInput::Run(Action::Create {name, ..}, Some(origin))) if name == "sample" && matches!(origin.0, Prompt::AddName) && origin.1 == "sample")
    );
}

#[gpui::test]
fn confirmation_defaults_to_cancel_and_escape_preserves_add_name(cx: &mut TestAppContext) {
    let (model, host, window, events) = fixture(cx, Prompt::Remove("old".into()));
    cx.simulate_keystrokes(window.into(), "enter");
    assert!(model.read_with(cx, |model, _| model.client.prompt.is_none()));
    assert!(matches!(
        events.borrow().last(),
        Some(PromptEvent::Cancelled)
    ));
    model.update(cx, |model, cx| {
        model.client.prompt(Prompt::Remove("accepted".into()));
        cx.notify();
    });
    cx.run_until_parked();
    cx.simulate_keystrokes(window.into(), "tab enter");
    assert!(
        matches!(events.borrow().last(), Some(PromptEvent::Input(ClientInput::Run(Action::Remove(name), _))) if name == "accepted")
    );
    model.update(cx, |model, cx| {
        model.client.prompt(Prompt::AddPath("saved-name".into()));
        cx.notify();
    });
    cx.run_until_parked();
    cx.simulate_keystrokes(window.into(), "escape");
    model.read_with(cx, |model, _| {
        assert!(matches!(model.client.prompt, Some(Prompt::AddName)));
        assert_eq!(model.client.input, "saved-name");
    });
    assert_eq!(
        host.read_with(cx, |host, cx| host.input.read(cx).value().to_string()),
        "saved-name"
    );
}

#[gpui::test]
fn filtered_picker_keeps_search_and_selects_visible_original_row(cx: &mut TestAppContext) {
    let (model, host, window, _) = fixture(
        cx,
        Prompt::ExportNodeDestination {
            text: "node".into(),
            share: false,
            clipboard: true,
        },
    );
    cx.simulate_keystrokes(window.into(), "n o d e s down");
    model.read_with(cx, |model, _| {
        assert!(matches!(
            model.client.prompt,
            Some(Prompt::ExportNodeDestination {
                clipboard: false,
                ..
            })
        ))
    });
    assert_eq!(
        host.read_with(cx, |host, cx| host.search.read(cx).value().to_string()),
        "nodes"
    );
    cx.simulate_keystrokes(window.into(), "space 1 / q");
    assert_eq!(
        host.read_with(cx, |host, cx| host.search.read(cx).value().to_string()),
        "nodes 1/q"
    );
}

#[gpui::test]
fn stale_native_file_choice_cannot_replace_a_new_prompt(cx: &mut TestAppContext) {
    let (model, host, window, _) = fixture(cx, Prompt::AddPath("old".into()));
    let generation = host.read_with(cx, |host, _| host.generation);
    model.update(cx, |model, cx| {
        model.client.prompt(Prompt::AddPath("new".into()));
        cx.notify();
    });
    window
        .update(cx, |_, window, cx| {
            host.update(cx, |host, cx| {
                host.accept_path(generation, Ok(Some("/tmp/stale.toml".into())), window, cx);
            })
        })
        .unwrap();
    model.read_with(cx, |model, _| {
        assert!(model.client.input.is_empty());
        assert!(matches!(&model.client.prompt, Some(Prompt::AddPath(name)) if name == "new"));
    });
}
