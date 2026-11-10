use super::*;
use astra_client::{protocol::Kind, state::Input};
use astra_management::v2 as v;
use gpui::TestAppContext;
use gpui_component::Root;
use std::{cell::RefCell, rc::Rc};

#[gpui::test]
fn installation_controls_emit_frozen_package_scope_and_elevation_request(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (commands, _) = async_channel::unbounded();
    let model = cx.new(|_| AppModel::new_test(Kind::Proxy, commands));
    model.update(cx, |model, _| {
        let destination = v::InstallationDestination {
            directory: Some("/user/astra".into()),
            entry: Some("/user/bin/astra-gui".into()),
            installed: false,
            requires_elevation: false,
            unavailable_reason: None,
        };
        model.client.installation.open = true;
        model.client.installation.status = Some(v::InstallationStatus {
            target: "selected-target".into(),
            account: "selected-user".into(),
            packages: vec![v::InstallationPackage {
                id: "frozen-package".into(),
                role: "client".into(),
                release_id: "release-1".into(),
                source: "/package".into(),
                user: destination.clone(),
                system: v::InstallationDestination {
                    requires_elevation: true,
                    ..destination
                },
            }],
            unavailable_roles: Default::default(),
            warnings: vec![],
        });
    });
    let mut view = None;
    let _window = cx.add_window(|window, cx| {
        let entity = cx.new(|cx| InstallationView::new(model.clone(), window, cx));
        view = Some(entity.clone());
        Root::new(entity, window, cx)
    });
    let view = view.unwrap();
    let captured = Rc::new(RefCell::new(Vec::new()));
    let outputs = captured.clone();
    let _events = cx.update(|cx| {
        cx.subscribe(&view, move |_, event: &ViewEvent, _| {
            outputs.borrow_mut().push(event.clone())
        })
    });
    cx.run_until_parked();
    view.update(cx, |view, cx| {
        view.action(InstallationAction::ToggleChoice, cx);
        view.action(InstallationAction::Confirm, cx);
    });
    cx.run_until_parked();
    assert!(captured.borrow().iter().any(|event| matches!(event, ViewEvent::Input(Input::Install(request)) if request.package_id == "frozen-package" && request.expected_release_id == "release-1" && request.elevate && request.scope == v::InstallationScope::System)));
    // The progress section renders for a running copy and for a failed step.
    model.update(cx, |model, cx| {
        model.client.installation.latest = Some(astra_client::installation::ProgressView {
            step: v::InstallationStep::Copying,
            step_index: 1,
            step_count: 5,
            current: 1,
            total: 2,
        });
        cx.notify();
    });
    cx.run_until_parked();
    model.update(cx, |model, cx| {
        model.client.installation.error = Some("copy failed".into());
        cx.notify();
    });
    cx.run_until_parked();
    model.update(cx, |model, _| {
        model.client.installation.query_error = Some("disconnected".into())
    });
    assert!(!model.read_with(cx, |model, _| model.client.installation.can_install()));
}
