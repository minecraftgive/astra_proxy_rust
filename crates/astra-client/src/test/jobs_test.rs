use super::*;
use crate::state::Client;

fn origin() -> Origin {
    Origin {
        selected: Some("proxy".into()),
        view_context: (5, Kind::Proxy, Some("proxy".into())),
        overlay_epoch: 2,
    }
}

#[test]
fn completion_cannot_replace_a_different_page_or_reopened_log() {
    let started = origin();
    let mut now = origin();
    assert!(
        started
            .apply_context(&Action::Read("proxy".into()), &now, false)
            .same
    );
    now.view_context.0 = 1;
    assert!(
        !started
            .apply_context(&Action::Read("proxy".into()), &now, false)
            .same
    );
    now = origin();
    now.overlay_epoch += 1;
    assert!(
        !started
            .apply_context(&Action::Logs("@tui".into()), &now, true)
            .log_target_current
    );
    assert!(
        !started
            .apply_context(&Action::Logs("@gui".into()), &now, true)
            .log_target_current
    );
    assert!(
        started
            .apply_context(&Action::Logs("proxy".into()), &now, false)
            .log_target_current
    );
}

#[tokio::test]
async fn queue_limits_and_save_exclusivity_survive_cancellation() {
    tokio::task::LocalSet::new()
        .run_until(async {
            let client = Client::new(Kind::Proxy);
            let mut jobs = Jobs::new(Path::new("/unused-astra-test-endpoint"), Waker::default());
            let save = Action::Save {
                name: "proxy".into(),
                original: "".into(),
                text: "".into(),
            };
            let first = jobs
                .start(
                    OperationInput::capture(&client),
                    save.clone(),
                    None,
                    origin(),
                )
                .unwrap();
            assert!(jobs
                .start(
                    OperationInput::capture(&client),
                    save.clone(),
                    None,
                    origin()
                )
                .is_err());
            for _ in 1..8 {
                jobs.start(
                    OperationInput::capture(&client),
                    Action::List,
                    None,
                    origin(),
                )
                .unwrap();
            }
            assert!(jobs
                .start(
                    OperationInput::capture(&client),
                    Action::List,
                    None,
                    origin()
                )
                .is_err());
            jobs.cancel(first);
            assert!(jobs
                .start(OperationInput::capture(&client), save, None, origin())
                .is_ok());
            let handles: Vec<_> = jobs.jobs.iter().map(|j| j.handle.abort_handle()).collect();
            drop(jobs);
            tokio::task::yield_now().await;
            assert!(handles.iter().all(|handle| handle.is_finished()));
        })
        .await;
}
