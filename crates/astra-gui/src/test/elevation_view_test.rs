use super::*;
use gpui::TestAppContext;
use gpui_component::Root;
#[gpui::test]
fn elevation_stays_visible_during_preparation_and_cancellation_releases_the_guard(
    cx: &mut TestAppContext,
) {
    cx.update(gpui_component::init);
    let (commands, _) = async_channel::unbounded();
    let model = cx.new(|_| AppModel::new_test(astra_client::protocol::Kind::Proxy, commands));
    let mut host = None;
    let window = cx.add_window(|window, cx| {
        let view = cx.new(|cx| ElevationHost::new(model.clone(), window, cx));
        host = Some(view.clone());
        Root::new(view, window, cx)
    });
    let host = host.unwrap();
    window
        .update(cx, |_, window, cx| {
            host.update(cx, |host, cx| {
                assert!(!host.start(Input::Idle, window, cx));
                let active = model.read(cx).elevation_active.clone();
                active.store(true, Ordering::Release);
                let control = Arc::new(Control::new(
                    model.read(cx).handle.clone(),
                    PathBuf::from("/nonexistent/test-elevation"),
                    active.clone(),
                ));
                host.control = Some(control.clone());
                host.phase = "Preparing administrator request";
                assert!(host.visible());
                assert!(host.start(
                    Input::Run(
                        Action::PrepareElevation {
                            name: "test".into(),
                            expected_pid: Some(1)
                        },
                        None
                    ),
                    window,
                    cx
                ));
                assert!(Arc::ptr_eq(host.control.as_ref().unwrap(), &control));
                host.cancel(cx);
                assert!(!host.visible());
                assert!(control.cancelled());
                assert!(!active.load(Ordering::Acquire));
            })
        })
        .unwrap();
}
use std::path::PathBuf;
