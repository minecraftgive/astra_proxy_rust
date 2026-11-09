use super::*;

#[derive(serde::Deserialize)]
struct EligibilityCase {
    name: String,
    request: v::NetworkDiagnosticsRequest,
    legacy: bool,
    resolution: bool,
}

#[test]
fn native_and_web_share_legacy_and_resolution_admission_cases() {
    let cases: Vec<EligibilityCase> =
        serde_json::from_str(include_str!("network_eligibility_cases.json")).unwrap();
    for case in cases {
        assert_eq!(
            validate_legacy(&case.request).is_ok(),
            case.legacy,
            "{}",
            case.name
        );
        assert_eq!(
            eligibility::needs_resolution(&case.request),
            case.resolution,
            "{}",
            case.name
        );
    }
}

#[test]
fn only_structured_existing_rejection_codes_are_definitive() {
    for (code, rejected) in [
        (v::ErrorCode::Unsupported, true),
        (v::ErrorCode::InvalidRequest, true),
        (v::ErrorCode::Conflict, true),
        (v::ErrorCode::OutcomeUnknown, false),
        (v::ErrorCode::Offline, false),
        (v::ErrorCode::Internal, false),
        (v::ErrorCode::VersionMismatch, false),
        (v::ErrorCode::NotFound, false),
        (v::ErrorCode::RequiresLocalAction, false),
        (v::ErrorCode::Unauthorized, false),
    ] {
        let error = anyhow::Error::new(ApiFailure(v::ApiError {
            code,
            message: "Unsupported: misleading human-readable text".into(),
            details: None,
        }))
        .context("outer request context");
        assert_eq!(submission_rejected(&error), rejected, "{error:#}");
    }
    assert!(!submission_rejected(&anyhow::anyhow!(
        "Unsupported: receipt lost"
    )));
}

#[tokio::test]
async fn request_preserves_the_typed_api_error() {
    let fixture = crate::import::tests::Fixture::new();
    let socket = fixture.0.join("typed-error.sock");
    let listener = astra_platform::platform::LocalListener::bind(&socket).unwrap();
    let client = Client::for_target(socket, None);
    let server = tokio::spawn(async move {
        let (mut stream, request) = next(&listener).await;
        astra_management::client::send(
            &mut stream,
            &v::Response::error(
                request.id,
                v::ErrorCode::Conflict,
                "arbitrary message without a code prefix",
            ),
        )
        .await
        .unwrap();
    });
    let error = client.availability().await.unwrap_err();
    let api = &error.downcast_ref::<ApiFailure>().unwrap().0;
    assert!(matches!(api.code, v::ErrorCode::Conflict));
    assert_eq!(api.message, "arbitrary message without a code prefix");
    assert!(submission_rejected(&error));
    server.await.unwrap();
}

#[test]
fn report_pages_reject_stalls_duplicate_items_and_oversized_evidence() {
    let mut page = report(v::NetworkState::Running);
    assert!(validate_page(&page, None).is_ok());
    page.has_more = true;
    assert!(validate_page(&page, None).is_err());
    page.cursor = "1".into();
    assert!(validate_page(&page, None).is_err());
    page.items.push(v::NetworkResult {
        id: "1".into(),
        sampled_at_ms: "2".into(),
        data: serde_json::json!({"event":"done"}),
    });
    assert!(validate_page(&page, None).is_ok());
    assert!(validate_page(&page, Some("1")).is_err());
    page.items.push(page.items[0].clone());
    assert!(validate_page(&page, None).is_err());
    page.items.pop();
    page.items[0].data["payload"] = serde_json::json!("x".repeat(v::NETWORK_PAGE_BYTES));
    assert!(validate_page(&page, None).is_err());
    page.items.clear();
    page.has_more = false;
    assert!(
        validate_page(&page, None).is_err(),
        "an empty unexpired page cannot skip evidence"
    );
    page.expired = true;
    page.state = v::NetworkState::Complete;
    assert!(
        validate_page(&page, None).is_ok(),
        "expired empty terminal reports retain their original cursor"
    );
}

fn hello(supported: bool) -> v::ResponseBody {
    v::ResponseBody::Hello(v::Hello {
        version: v::VERSION,
        role: "agent".into(),
        epoch: "boot-1".into(),
        build_id: "test".into(),
        capabilities: if supported {
            vec![v::NETWORK_DIAGNOSTICS_CAPABILITY.into()]
        } else {
            vec![]
        },
    })
}
fn task(state: v::TaskState) -> v::Task {
    v::Task {
        id: "job-1".into(),
        state,
        message: "Diagnostic queued".into(),
        started_ms: "1".into(),
        finished_ms: None,
    }
}
fn report(state: v::NetworkState) -> v::NetworkReport {
    v::NetworkReport {
        operation_id: "job-1".into(),
        schema_version: 1,
        revision: "1".into(),
        state,
        message: "test".into(),
        started_ms: "1".into(),
        finished_ms: state.terminal().then(|| "2".into()),
        truncated: false,
        expired: false,
        items: vec![],
        cursor: "0".into(),
        has_more: false,
    }
}
fn probe() -> v::NetToolRequest {
    v::NetToolRequest {
        egress: astra_model::nettool::Egress::System,
        probe: astra_model::nettool::Probe::Tcp {
            host: "127.0.0.1".into(),
            port: 80,
        },
        count: 1,
        interval_ms: "0".into(),
        timeout_ms: "100".into(),
    }
}
async fn next(
    listener: &astra_platform::platform::LocalListener,
) -> (astra_platform::platform::LocalStream, v::Request) {
    let (mut stream, _) = listener.accept().await.unwrap();
    let request = astra_management::client::receive(&mut stream)
        .await
        .unwrap();
    (stream, request)
}
async fn reply(
    mut stream: astra_platform::platform::LocalStream,
    request: v::Request,
    body: v::ResponseBody,
) {
    astra_management::client::send(&mut stream, &v::Response::success(request.id, body))
        .await
        .unwrap();
}

#[tokio::test]
async fn legacy_capability_prevents_new_command_and_protocol_errors_are_distinct() {
    let fixture = crate::import::tests::Fixture::new();
    let socket = fixture.0.join("network.sock");
    let listener = astra_platform::platform::LocalListener::bind(&socket).unwrap();
    let client = Client::for_target(socket, Some("remote-agent".into()));
    let server = tokio::spawn(async move {
        let (stream, request) = next(&listener).await;
        assert_eq!(request.agent_id.as_deref(), Some("remote-agent"));
        assert!(matches!(
            request.body,
            v::RequestBody::Query(v::Query::Hello)
        ));
        reply(stream, request, hello(false)).await;
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), listener.accept())
                .await
                .is_err()
        );
    });
    let error = client
        .submit("job-1".into(), "proxy".into(), probe())
        .await
        .unwrap_err();
    assert!(error.to_string().starts_with("Unsupported:"));
    server.await.unwrap();
    assert!(report_response(hello(true), "job-1")
        .unwrap_err()
        .to_string()
        .starts_with("Protocol error:"));
}

#[tokio::test]
async fn acceptance_retains_operation_and_target_and_is_not_completion() {
    let fixture = crate::import::tests::Fixture::new();
    let socket = fixture.0.join("network.sock");
    let listener = astra_platform::platform::LocalListener::bind(&socket).unwrap();
    let client = Client::for_target(socket, Some("remote-agent".into()));
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (stream, request) = next(&listener).await;
            assert_eq!(request.agent_id.as_deref(), Some("remote-agent"));
            assert!(matches!(
                request.body,
                v::RequestBody::Query(v::Query::Hello)
            ));
            reply(stream, request, hello(true)).await;
            let (stream, request) = next(&listener).await;
            assert_eq!(request.agent_id.as_deref(), Some("remote-agent"));
            assert!(matches!(&request.body, v::RequestBody::Execute {
                operation_id, command: v::Command::NetworkDiagnostics { instance, .. }
            } if operation_id == "job-1" && instance == "proxy"));
            reply(
                stream,
                request,
                v::ResponseBody::Task(task(v::TaskState::Running)),
            )
            .await;
        }
        let (stream, request) = next(&listener).await;
        assert!(
            matches!(&request.body, v::RequestBody::Query(v::Query::NetworkReport { operation_id, cursor })
            if operation_id == "job-1" && cursor.as_deref() == Some("7"))
        );
        let mut page = report(v::NetworkState::Running);
        page.cursor = "7".into();
        reply(stream, request, v::ResponseBody::NetworkReport(page)).await;
    });
    for _ in 0..2 {
        let accepted = client
            .submit("job-1".into(), "proxy".into(), probe())
            .await
            .unwrap();
        assert_eq!(accepted.id, "job-1");
        assert!(matches!(accepted.state, v::TaskState::Running));
    }
    let page = client.report("job-1", Some("7".into())).await.unwrap();
    assert!(!page.state.terminal());
    server.await.unwrap();
}

#[tokio::test]
async fn lost_submission_reply_can_be_queried_without_replaying_the_command() {
    let fixture = crate::import::tests::Fixture::new();
    let socket = fixture.0.join("network.sock");
    let listener = astra_platform::platform::LocalListener::bind(&socket).unwrap();
    let client = Client::for_target(socket, Some("remote-agent".into()));
    let server = tokio::spawn(async move {
        let (stream, request) = next(&listener).await;
        reply(stream, request, hello(true)).await;
        let (stream, request) = next(&listener).await;
        assert!(
            matches!(&request.body, v::RequestBody::Execute { operation_id, .. } if operation_id == "job-1")
        );
        // The Agent accepted the task, but the acceptance receipt was lost.
        drop(stream);
        let (stream, request) = next(&listener).await;
        assert!(
            matches!(&request.body, v::RequestBody::Query(v::Query::Task { operation_id }) if operation_id == "job-1")
        );
        reply(
            stream,
            request,
            v::ResponseBody::Task(task(v::TaskState::Running)),
        )
        .await;
    });
    assert!(client
        .submit("job-1".into(), "proxy".into(), probe())
        .await
        .is_err());
    assert!(matches!(
        client.task("job-1").await.unwrap().state,
        v::TaskState::Running
    ));
    server.await.unwrap();
}

#[tokio::test]
async fn unexpected_hello_is_protocol_error_and_never_triggers_legacy_fallback() {
    let fixture = crate::import::tests::Fixture::new();
    let socket = fixture.0.join("network.sock");
    let listener = astra_platform::platform::LocalListener::bind(&socket).unwrap();
    let client = Client::for_target(socket, Some("remote-agent".into()));
    let server = tokio::spawn(async move {
        let (stream, request) = next(&listener).await;
        reply(
            stream,
            request,
            v::ResponseBody::Task(task(v::TaskState::Complete)),
        )
        .await;
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(50), listener.accept())
                .await
                .is_err()
        );
    });
    let error = client
        .submit("job-1".into(), "proxy".into(), probe())
        .await
        .unwrap_err();
    assert!(error.to_string().starts_with("Protocol error:"));
    server.await.unwrap();
}

#[tokio::test]
async fn cancellation_requires_terminal_report_and_keeps_completion_race_outcome() {
    for (state, expected_ok, already_finished) in [
        (v::NetworkState::Running, false, false),
        (v::NetworkState::Cancelled, true, false),
        (v::NetworkState::Complete, true, true),
    ] {
        let fixture = crate::import::tests::Fixture::new();
        let socket = fixture.0.join("network.sock");
        let listener = astra_platform::platform::LocalListener::bind(&socket).unwrap();
        let client = Client::for_target(socket, Some("remote-agent".into()));
        let server = tokio::spawn(async move {
            let (stream, request) = next(&listener).await;
            assert_eq!(request.agent_id.as_deref(), Some("remote-agent"));
            assert!(matches!(&request.body, v::RequestBody::Execute {
                operation_id, command: v::Command::CancelTask { operation_id: target }
            } if operation_id == "cancel-1" && target == "job-1"));
            if already_finished {
                reply(
                    stream,
                    request,
                    v::ResponseBody::Cancellation(v::Cancellation::AlreadyFinished(
                        "Completed".into(),
                    )),
                )
                .await;
                let (stream, request) = next(&listener).await;
                assert!(
                    matches!(&request.body, v::RequestBody::Query(v::Query::NetworkReport { operation_id, .. }) if operation_id == "job-1")
                );
                reply(
                    stream,
                    request,
                    v::ResponseBody::NetworkReport(report(state)),
                )
                .await;
            } else {
                reply(
                    stream,
                    request,
                    v::ResponseBody::NetworkReport(report(state)),
                )
                .await;
            }
        });
        let outcome = client.cancel("job-1", "cancel-1".into()).await;
        assert_eq!(outcome.is_ok(), expected_ok);
        if let Ok(report) = outcome {
            assert_eq!(report.state, state);
        }
        server.await.unwrap();
    }
}

#[test]
fn wrong_task_and_report_identities_are_protocol_errors() {
    assert!(task_response(
        v::ResponseBody::Task(task(v::TaskState::Complete)),
        "other-job"
    )
    .is_err());
    assert!(report_response(
        v::ResponseBody::NetworkReport(report(v::NetworkState::Complete)),
        "other-job"
    )
    .is_err());
    let mut future_schema = report(v::NetworkState::Complete);
    future_schema.schema_version = 2;
    assert!(report_response(v::ResponseBody::NetworkReport(future_schema), "job-1").is_err());
}
