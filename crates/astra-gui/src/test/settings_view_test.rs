use super::*;
use astra_client::{motion::MotionMode, preferences::Accent, protocol::Kind};
use gpui::TestAppContext;
use gpui_component::Root;

#[gpui::test]
fn settings_controls_apply_shared_preferences_and_cancel_marquee_hold(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (commands, _) = async_channel::unbounded();
    let model = cx.new(|_| AppModel::new_test(Kind::Proxy, commands));
    let mut view = None;
    let window = cx.add_window(|window, cx| {
        let entity = cx.new(|cx| SettingsView::new(model.clone(), window, cx));
        view = Some(entity.clone());
        Root::new(entity, window, cx)
    });
    let view = view.unwrap();
    cx.run_until_parked();
    window
        .update(cx, |_, window, cx| {
            view.update(cx, |view, cx| {
                view.choose(Field::Accent, 2, window, cx);
                view.choose(Field::Motion, 2, window, cx);
                view.choose(Field::VimKeys, 0, window, cx);
                assert_eq!(model.read(cx).client.preferences.accent, Accent::Green);
                assert_eq!(model.read(cx).client.preferences.motion, MotionMode::Off);
                assert!(model.read(cx).client.vim_keys);
                window.focus(&view.accent_focus, cx);
                view.start_hold(window, cx);
                assert!(view.hold.is_some());
                assert!(model.read(cx).client.marquee_hold.is_some());
                view.end_hold(cx);
                assert!(view.hold.is_none());
                assert!(model.read(cx).client.marquee_hold.is_none());
                view.choose(Field::Installation, 0, window, cx);
                assert!(model.read(cx).client.installation.open);
            })
        })
        .unwrap();
    cx.run_until_parked();
}
