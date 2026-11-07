use super::*;
use astra_client::{
    protocol::{
        updates::{UpdateIntent, UpdateStatus},
        Kind,
    },
    state::Prompt,
};
use gpui::TestAppContext;
use gpui_component::Root;

#[gpui::test]
fn update_controls_use_capabilities_and_freeze_confirmation_target(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (commands, _) = async_channel::unbounded();
    let model = cx.new(|_| AppModel::new_test(Kind::Proxy, commands));
    let mut view = None;
    let _window = cx.add_window(|window, cx| {
        let entity = cx.new(|cx| UpdatesView::new(model.clone(), window, cx));
        view = Some(entity.clone());
        Root::new(entity, window, cx)
    });
    let view = view.unwrap();
    view.update(cx, |view, cx| view.command(Command::Activate, cx));
    assert!(model.read_with(cx, |model, _| model.client.prompt.is_none()));
    model.update(cx, |model, _| {
        let mut status = UpdateStatus {
            staged_release_id: Some("verified-target".into()),
            staged_compatible: Some(true),
            ..Default::default()
        };
        status.capabilities.activation_allowed = true;
        model.client.update_status = Some(status);
    });
    view.update(cx, |view, cx| view.command(Command::Activate, cx));
    model.update(cx, |model, _| {
        model.client.update_status.as_mut().unwrap().staged_release_id = Some("new-target".into());
        assert!(matches!(&model.client.prompt, Some(Prompt::UpdateConfirm(request)) if matches!(&request.intent, UpdateIntent::Activate { release_id } if release_id == "verified-target")));
    });
    cx.run_until_parked();
}
