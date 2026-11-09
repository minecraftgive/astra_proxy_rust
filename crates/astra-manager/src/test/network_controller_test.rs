use super::*;

fn diagnostic() -> RequestBody {
    RequestBody::Execute {
        operation_id: "diagnostic-once".into(),
        command: Command::NetworkDiagnostics {
            instance: "proxy".into(),
            request: NetToolRequest {
                egress: astra_model::nettool::Egress::System,
                probe: astra_model::nettool::Probe::Tcp {
                    host: "127.0.0.1".into(),
                    port: 9,
                },
                count: 1,
                interval_ms: "0".into(),
                timeout_ms: "100".into(),
            }
            .into(),
        },
    }
}

fn task(state: TaskState) -> ResponseBody {
    ResponseBody::Task(Task {
        id: "diagnostic-once".into(),
        finished_ms: (!matches!(state, TaskState::Running)).then(|| "200".into()),
        state,
        message: "Fixture outcome".into(),
        started_ms: "100".into(),
    })
}

#[tokio::test]
async fn controller_distinguishes_acceptance_observed_completion_and_synchronous_replies() {
    let root = tempfile::tempdir().unwrap();
    let controller = Controller::open(
        ControllerConfig {
            listen: "127.0.0.1:0".parse().unwrap(),
            tls_cert: "unused".into(),
            tls_key: "unused".into(),
            agent_token_hashes: Default::default(),
        },
        root.path(),
        128,
    )
    .await
    .unwrap();
    let (sender, mut requests) = mpsc::channel::<Request>(4);
    let pending: Pending = Default::default();
    controller.peers.lock().unwrap().insert(
        "agent-a".into(),
        Peer {
            sender,
            pending: pending.clone(),
            connection: 1,
            revoked: Default::default(),
        },
    );
    let completed = ResponseBody::NetworkReport(NetworkReport {
        operation_id: "diagnostic-once".into(),
        schema_version: 1,
        revision: "3".into(),
        state: NetworkState::Complete,
        message: "Fixture complete".into(),
        started_ms: "100".into(),
        finished_ms: Some("200".into()),
        truncated: false,
        expired: false,
        items: vec![],
        cursor: "0".into(),
        has_more: false,
    });
    let observe_task = RequestBody::Query(Query::Task {
        operation_id: "diagnostic-once".into(),
    });
    let cases = vec![
        (
            diagnostic(),
            Ok(task(TaskState::Running)),
            "accepted",
            "diagnostic-once",
        ),
        (
            observe_task.clone(),
            Ok(task(TaskState::Running)),
            "accepted",
            "diagnostic-once",
        ),
        (
            RequestBody::Query(Query::NetworkReport {
                operation_id: "diagnostic-once".into(),
                cursor: None,
            }),
            Ok(completed),
            "completed",
            "diagnostic-once",
        ),
        (
            observe_task.clone(),
            Ok(task(TaskState::Complete)),
            "completed",
            "diagnostic-once",
        ),
        (
            diagnostic(),
            Ok(task(TaskState::Complete)),
            "completed",
            "diagnostic-once",
        ),
        // An observation error cannot erase the last durable execution outcome.
        (
            observe_task,
            Err(ApiError {
                code: ErrorCode::NotFound,
                message: "fixture read failure".into(),
                details: None,
            }),
            "completed",
            "diagnostic-once",
        ),
        (
            RequestBody::Execute {
                operation_id: "ordinary".into(),
                command: Command::Start {
                    instance: "proxy".into(),
                },
            },
            Ok(ResponseBody::Done {
                message: "Started".into(),
            }),
            "responded",
            "ordinary",
        ),
    ];
    let replies: Vec<_> = cases.iter().map(|(_, reply, _, _)| reply.clone()).collect();
    let responder = tokio::spawn(async move {
        for result in replies {
            let request = requests.recv().await.unwrap();
            pending
                .lock()
                .unwrap()
                .remove(&request.id)
                .unwrap()
                .send(Response {
                    version: VERSION,
                    id: request.id.clone(),
                    result,
                })
                .unwrap();
        }
    });
    for (number, (body, expected_reply, expected, operation)) in cases.into_iter().enumerate() {
        let id = format!("front-{number}");
        let mut request = Request::new(id.clone(), body);
        request.agent_id = Some("agent-a".into());
        let response = tokio::time::timeout(Duration::from_secs(2), controller.handle(request))
            .await
            .unwrap();
        assert_eq!(response.id, id);
        assert_eq!(
            serde_json::to_value(response.result).unwrap(),
            serde_json::to_value(expected_reply).unwrap()
        );
        let db = rusqlite::Connection::open_with_flags(
            root.path().join("controller.sqlite"),
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let (state, owner): (String, String) = db
            .query_row(
                "SELECT state,agent_id FROM controller_routes WHERE operation_id=?1",
                [operation],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(state, expected, "case {number}");
        assert_eq!(owner, "agent-a");
    }
    responder.await.unwrap();
}
