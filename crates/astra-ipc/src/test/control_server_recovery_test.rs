use super::*;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Mutex,
};

#[derive(Clone, Default)]
struct Logs(Arc<Mutex<Vec<u8>>>);
impl io::Write for Logs {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
impl Logs {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
    fn subscriber(&self) -> impl tracing::Subscriber + Send + Sync + 'static {
        let logs = self.clone();
        tracing_subscriber::fmt()
            .without_time()
            .with_ansi(false)
            .with_max_level(tracing::Level::TRACE)
            .with_writer(move || logs.clone())
            .finish()
    }
}
fn status(error: Option<String>) -> Status {
    Status {
        api_version: crate::API_VERSION,
        pid: std::process::id(),
        uptime_secs: 0,
        error,
        applied: None,
    }
}

#[tokio::test(flavor = "current_thread")]
async fn hup_timeout_keeps_running_and_logs_completion_rejection_and_full_queue() {
    let logs = Logs::default();
    let _subscriber = tracing::subscriber::set_default(logs.subscriber());
    let (release, wait) = oneshot::channel();
    let job = tokio::spawn(signal_reload(
        async move {
            wait.await.unwrap();
            Ok(())
        },
        Duration::from_millis(5),
    ));
    timeout(Duration::from_secs(2), async {
        while !logs.text().contains("reload is still running") {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(!job.is_finished());
    release.send(()).unwrap();
    job.await.unwrap();
    assert!(logs.text().contains("Configuration reload completed"));
    signal_reload(
        async { Err("injected rejection".into()) },
        Duration::from_secs(1),
    )
    .await;
    assert!(logs.text().contains("injected rejection"));
    assert!(!accept_signal_reload(MUTATION_LIMIT));
    assert!(logs.text().contains("control queue is full"));
}

#[tokio::test(flavor = "current_thread")]
async fn expired_response_does_not_cancel_or_replay_accepted_mutation() {
    let logs = Logs::default();
    let _subscriber = tracing::subscriber::set_default(logs.subscriber());
    let executions = Arc::new(AtomicUsize::new(0));
    let count = executions.clone();
    let (release, wait) = oneshot::channel();
    let (reply, receiver) = oneshot::channel();
    let job = tokio::spawn(finish_request(
        async move {
            count.fetch_add(1, Ordering::SeqCst);
            wait.await.unwrap();
            Response::Status(status(Some("late rejection".into())))
        },
        reply,
        0,
        status(None),
        true,
        Duration::from_millis(5),
    ));
    let response = timeout(Duration::from_secs(2), receiver)
        .await
        .unwrap()
        .unwrap();
    assert!(response_error(&response)
        .unwrap()
        .contains("outcome unknown"));
    assert!(!job.is_finished());
    release.send(()).unwrap();
    job.await.unwrap();
    assert_eq!(executions.load(Ordering::SeqCst), 1);
    assert!(logs.text().contains("late rejection"));
}
