//! Exercise the background path with room left for the real main/entry/UI stack.
//! Test harness threads normally have more stack than the 1 MiB Windows main
//! thread and previously hid a stack overflow in both Connect and Logs.
use super::*;
use astra_management::v2 as v;

const CHILD: &str = "ui::stack_tests::background_stack_child";
const CASE_ENV: &str = "ASTRA_TUI_STACK_TEST_CASE";

#[test]
fn connect_and_logs_fit_background_stack_budget() {
    // Stack overflows abort the process; isolate them so the parent reports the
    // failing path instead of losing every other test result.
    for case in ["connect", "logs"] {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", CHILD, "--ignored", "--nocapture"])
            .env(CASE_ENV, case)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{case} exceeded its stack budget or failed: {}\n{}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr),
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("stack case completed"));
    }
}

#[test]
#[ignore = "subprocess entry point for connect_and_logs_fit_background_stack_budget"]
fn background_stack_child() {
    let Ok(case) = std::env::var(CASE_ENV) else {
        return;
    };
    std::thread::Builder::new()
        .name(format!("tui-{case}-stack"))
        .stack_size(512 * 1024)
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap()
                .block_on(tokio::task::LocalSet::new().run_until(exercise(case)));
        })
        .unwrap()
        .join()
        .unwrap();
    println!("stack case completed");
}

async fn exercise(case: String) {
    let fixture = crate::import::tests::Fixture::new();
    #[cfg(windows)]
    if astra_platform::elevation::is_elevated().unwrap() {
        // Hosted Windows runners use an elevated token whose default file
        // owner is Administrators. Give only this new fixture directory the
        // actual user's owner SID, as an unelevated launcher would. The IPC
        // ownership/reparse-point checks and the stack budget stay unchanged.
        let identity = std::process::Command::new("whoami.exe")
            .args(["/user", "/fo", "csv", "/nh"])
            .output()
            .unwrap();
        assert!(identity.status.success());
        let identity = String::from_utf8_lossy(&identity.stdout);
        let sid = identity
            .trim()
            .rsplit(',')
            .next()
            .unwrap()
            .trim_matches('"');
        assert!(sid.starts_with("S-1-"), "Expected the current user's SID");
        let owner = std::process::Command::new("icacls.exe")
            .arg(&fixture.0)
            .args(["/setowner", &format!("*{sid}"), "/L", "/Q"])
            .output()
            .unwrap();
        assert!(
            owner.status.success(),
            "Cannot assign the test directory to its user: {}\n{}",
            String::from_utf8_lossy(&owner.stdout),
            String::from_utf8_lossy(&owner.stderr),
        );
    }
    astra_platform::platform::set_private(&fixture.0, 0o700).unwrap();
    let socket = fixture.0.join("management.sock");
    let listener = astra_platform::platform::LocalListener::bind(&socket).unwrap();
    let server = tokio::spawn(async move {
        let prepared = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut peers = tokio::task::JoinSet::new();
        loop {
            // Execute and its progress query use separate connections; serve
            // both concurrently, as the real management service does.
            let (mut stream, _) = tokio::select! {
                peer = listener.accept() => peer.unwrap(),
                completed = peers.join_next(), if !peers.is_empty() => {
                    completed.unwrap().unwrap();
                    continue;
                }
            };
            let prepared = prepared.clone();
            peers.spawn(async move {
                let request: v::Request = astra_management::client::receive(&mut stream)
                    .await
                    .unwrap();
                let response = match &request.body {
                    v::RequestBody::Execute {
                        command: v::Command::PrepareConfig { .. },
                        ..
                    } => {
                        prepared.store(true, std::sync::atomic::Ordering::SeqCst);
                        v::ResponseBody::Done {
                            message: String::new(),
                        }
                    }
                    v::RequestBody::Execute {
                        command: v::Command::Start { .. },
                        ..
                    } => {
                        assert!(
                            prepared.load(std::sync::atomic::Ordering::SeqCst),
                            "Connect must check configuration before starting"
                        );
                        v::ResponseBody::Done {
                            message: "Started isolated instance".into(),
                        }
                    }
                    v::RequestBody::Query(v::Query::Events { .. }) => {
                        v::ResponseBody::Events(v::EventBatch {
                            events: vec![],
                            cursor: "0".into(),
                            resync_required: false,
                        })
                    }
                    v::RequestBody::Query(v::Query::LogLevel { .. }) => {
                        v::ResponseBody::LogLevel(LogLevel::Debug)
                    }
                    v::RequestBody::Query(v::Query::LogSessions { .. }) => {
                        v::ResponseBody::LogSessions(vec![])
                    }
                    v::RequestBody::Query(v::Query::LogChunk { cursor: None, .. }) => {
                        v::ResponseBody::LogChunk(v::LogChunk::Data {
                            bytes: b"INFO stack regression\n".to_vec(),
                            cursor: v::LogCursor {
                                device: "1".into(),
                                inode: "2".into(),
                                offset: "22".into(),
                            },
                            reset: true,
                            skip_prefix: false,
                            more: false,
                        })
                    }
                    v::RequestBody::Query(v::Query::LogChunk { .. }) => {
                        v::ResponseBody::LogChunk(v::LogChunk::Waiting)
                    }
                    other => panic!("Unexpected management request: {other:?}"),
                };
                test_rpc::reply(stream, request, response).await;
            });
        }
    });
    let mut ui = Ui::new(Session::default());
    let mut jobs = background::Jobs::new(&socket);
    let action = match case.as_str() {
        "connect" => Action::Start("isolated".into()),
        "logs" => Action::Logs("isolated".into()),
        other => panic!("Unexpected case: {other}"),
    };
    jobs.start(&mut ui, action, None);
    assert!(ui.tasks.running());
    tokio::time::timeout(Duration::from_secs(5), async {
        while ui.tasks.running() {
            jobs.poll(&mut ui).await;
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        assert!(ui.recovery.is_none(), "{}", ui.message);
        if case == "connect" {
            assert_eq!(ui.message, "Started isolated instance");
        } else {
            let viewer = ui.viewer.as_mut().expect("Logs must open a viewer");
            assert_eq!(viewer.capture_level, LogLevel::Debug);
            loop {
                assert!(viewer.poll().is_none());
                if let Some(cursor) = &viewer.cursor {
                    assert_eq!((cursor.device, cursor.inode, cursor.offset), (1, 2, 22));
                    break;
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            viewer.stop();
        }
    })
    .await
    .expect("Background request did not finish");
    drop(jobs);
    server.abort();
    let _ = server.await;
    astra_platform::platform::remove_socket(&socket);
}
