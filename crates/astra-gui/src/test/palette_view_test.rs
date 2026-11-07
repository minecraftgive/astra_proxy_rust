use super::{AppModel, InputState, PaletteContext, PaletteMode, View, ViewEvent};
use astra_client::{protocol::Kind, state::Input as ClientInput};
use gpui::TestAppContext;
use gpui::{
    div, AppContext, Context, Entity, IntoElement, ParentElement, Render, Styled, Subscription,
    Window,
};
use gpui_component::Root;
use std::{cell::RefCell, rc::Rc};
struct Harness {
    view: Entity<View>,
    _subscription: Subscription,
}
impl Render for Harness {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.view.clone())
    }
}
#[gpui::test]
fn palette_keyboard_uses_shared_matches_and_disabled_entries_do_not_run(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (commands, receiver) = async_channel::unbounded();
    let model = cx.new(|_| AppModel::new_test(Kind::Proxy, commands));
    let events = Rc::new(RefCell::new(Vec::<ViewEvent>::new()));
    let mut palette = None;
    let window = cx.add_window(|window, cx| {
        let harness = cx.new(|cx| {
            let search = cx.new(|cx| InputState::new(window, cx));
            let view = cx.new(|cx| {
                View::new(
                    model.clone(),
                    search,
                    PaletteMode::Commands,
                    PaletteContext {
                        page: 0,
                        window_id: 0,
                        editor_active: false,
                    },
                    window,
                    cx,
                )
            });
            palette = Some(view.clone());
            let output = events.clone();
            let subscription = cx.subscribe(&view, move |_, _, event, cx| {
                output.borrow_mut().push(event.clone());
                cx.notify();
            });
            Harness {
                view,
                _subscription: subscription,
            }
        });
        Root::new(harness, window, cx)
    });
    let palette = palette.unwrap();
    cx.run_until_parked();
    window
        .update(cx, |_, window, cx| {
            palette.update(cx, |view, cx| {
                view.search.update(cx, |input, cx| {
                    input.set_value("Connect profile", window, cx)
                })
            })
        })
        .unwrap();
    cx.run_until_parked();
    palette.read_with(cx, |view, cx| {
        assert!(view
            .entries(cx)
            .iter()
            .all(|entry| entry.unavailable.is_some()))
    });
    cx.simulate_keystrokes(window.into(), "enter");
    assert!(events.borrow().is_empty());
    assert!(receiver.is_empty());
    window
        .update(cx, |_, window, cx| {
            palette.update(cx, |view, cx| {
                view.search
                    .update(cx, |input, cx| input.set_value("", window, cx))
            })
        })
        .unwrap();
    cx.run_until_parked();
    // Another window may change its local page while this palette remains open.
    model.update(cx, |model, _| model.client.page = 6);
    cx.simulate_keystrokes(window.into(), "down enter");
    assert!(events
        .borrow()
        .iter()
        .any(|event| matches!(event, ViewEvent::Navigate(1))));
    assert!(events
        .borrow()
        .iter()
        .any(|event| matches!(event, ViewEvent::Input(ClientInput::Idle))));
    cx.simulate_keystrokes(window.into(), "escape");
    assert!(matches!(events.borrow().last(), Some(ViewEvent::Dismiss)));
}
