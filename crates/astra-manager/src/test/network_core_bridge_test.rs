use super::*;

#[test]
fn bridge_preserves_capability_protocol_and_transport_failures() {
    use astra_ipc::control::FakeIpLookupErrorKind as E;
    for (kind, expected) in [
        (E::Unsupported, CoreQueryStatus::Unsupported),
        (E::Unconfirmed, CoreQueryStatus::Unconfirmed),
        (E::Communication, CoreQueryStatus::CommunicationError),
        (E::Protocol, CoreQueryStatus::ProtocolError),
        (E::Core, CoreQueryStatus::CoreError),
        (E::InvalidRequest, CoreQueryStatus::InvalidRequest),
    ] {
        let (actual, message) = lookup_failure(astra_ipc::control::FakeIpLookupError {
            kind,
            message: "original evidence".into(),
        });
        assert_eq!(actual, expected);
        assert_eq!(message, "original evidence");
    }
    assert_eq!(
        io_failure(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "bad frame"
        ))
        .0,
        CoreQueryStatus::ProtocolError
    );
    assert_eq!(
        io_failure(std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "closed"
        ))
        .0,
        CoreQueryStatus::CommunicationError
    );
}

#[tokio::test]
async fn read_only_core_bridge_rejects_oversized_requests_before_contacting_a_core() {
    let root = tempfile::tempdir().unwrap();
    let agent = AgentService::new(ManagerConfig {
        state_dir: root.path().join("state"),
        config_dir: root.path().join("config"),
        bin_dir: root.path().join("absent"),
        ..Default::default()
    })
    .unwrap();
    let _mutation = agent.mutation.lock().await;
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        agent.handle(Request::new(
            "bounded",
            RequestBody::Query(Query::FakeIpLookup {
                instance: "unknown-instance".into(),
                addresses: vec!["198.18.0.2".parse().unwrap(); 65],
            }),
        )),
    )
    .await
    .unwrap();
    let Ok(ResponseBody::FakeIpLookup(reply)) = response.result else {
        panic!("Expected typed lookup reply")
    };
    assert_eq!(reply.status, CoreQueryStatus::InvalidRequest);
    assert!(reply.mappings.is_empty());
    assert!(reply.boot_id.is_none());
    assert!(agent.store.tasks(10).await.unwrap().is_empty());
    let response = agent
        .handle(Request::new(
            "core",
            RequestBody::Query(Query::NetworkCore {
                instance: "unknown-instance".into(),
            }),
        ))
        .await;
    let Ok(ResponseBody::NetworkCore(reply)) = response.result else {
        panic!("Expected typed core reply")
    };
    assert_eq!(reply.status, CoreQueryStatus::CommunicationError);
    assert!(reply.identity.is_none());
}

#[tokio::test]
async fn local_storage_failure_is_not_a_core_communication_error() {
    let root = tempfile::tempdir().unwrap();
    let agent = AgentService::new(ManagerConfig {
        state_dir: root.path().join("state"),
        config_dir: root.path().join("config"),
        bin_dir: root.path().join("bin"),
        ..Default::default()
    })
    .unwrap();
    let (client, actor) =
        crate::manager::NativeClient::with_test_runtime_actor(root.path().join("actor"), |_, _| {
            Ok(old::Reply::text("{}"))
        })
        .unwrap();
    assert!(agent.native.set(client).is_ok());
    let db = rusqlite::Connection::open(agent.config.state_dir.join("management.sqlite")).unwrap();
    db.execute("DROP TABLE servers", []).unwrap();
    for query in [
        Query::NetworkCore {
            instance: "proxy".into(),
        },
        Query::FakeIpLookup {
            instance: "proxy".into(),
            addresses: vec![],
        },
    ] {
        let response = agent
            .handle(Request::new("observe", RequestBody::Query(query)))
            .await;
        assert!(matches!(
            response.result,
            Err(ApiError {
                code: ErrorCode::Internal,
                ..
            })
        ));
    }
    actor.abort();
}
