use super::*;
use gpui::{AppContext, TestAppContext};
#[gpui::test]
fn failed_quit_preserves_the_window_and_a_retry_saves_the_latest_snapshot(cx: &mut TestAppContext) {
    cx.update(gpui_component::init);
    let (commands, incoming) = async_channel::unbounded();
    let model = cx.new(|_| AppModel::new_test(Kind::Proxy, commands));
    let window = cx.add_window(|_, _| gpui::Empty);
    model.update(cx, |model, cx| {
        model.ready = true;
        model.register_window(5, 6);
        model.client.input = "draft before close".into();
        model.request_quit(cx);
        assert!(!model.commands.is_closed());
        assert!(model.quit_pending);
    });
    let BackendCmd::CheckpointForQuit { session } = incoming.try_recv().unwrap() else {
        panic!("missing quit checkpoint")
    };
    assert_eq!(session.gui_windows, vec![6]);
    model.update(cx, |model, cx| {
        model.apply(BackendEvent::QuitCheckpoint(Err("disk full".into())), cx);
        assert!(!model.quit_prepared);
        assert!(!model.quit_pending);
        assert!(!model.commands.is_closed());
        assert_eq!(model.client.input, "draft before close");
        assert!(model.client.tasks.verdict.as_ref().unwrap().1);
        assert!(!cx.windows().is_empty());
        model.client.input = "draft at retry".into();
        model.request_quit(cx);
    });
    let BackendCmd::CheckpointForQuit { session } = incoming.try_recv().unwrap() else {
        panic!("missing retry")
    };
    assert_eq!(session.input, "draft at retry");
    model.update(cx, |model, cx| {
        model.client.input = "edited during write".into();
        model.apply(BackendEvent::QuitCheckpoint(Ok(())), cx);
        assert!(!model.quit_prepared);
    });
    let BackendCmd::CheckpointForQuit { session } = incoming.try_recv().unwrap() else {
        panic!("missing latest snapshot")
    };
    assert_eq!(session.input, "edited during write");
    model.update(cx, |model, cx| {
        model.apply(BackendEvent::QuitCheckpoint(Ok(())), cx);
        assert!(model.quit_prepared);
        assert!(model.commands.is_closed());
    });
    let _ = window;
}
#[gpui::test]
fn removed_last_window_retains_its_page_for_the_final_checkpoint(cx: &mut TestAppContext) {
    let (commands, incoming) = async_channel::unbounded();
    let model = cx.new(|_| AppModel::new_test(Kind::Proxy, commands));
    model.update(cx, |model, cx| {
        model.ready = true;
        model.client.page = 0;
        model.register_window(2, 4);
        model.remove_window(2);
        model.request_quit(cx);
    });
    let BackendCmd::CheckpointForQuit { session } = incoming.try_recv().unwrap() else {
        panic!("missing checkpoint")
    };
    assert_eq!(session.gui_windows, vec![4]);
}
