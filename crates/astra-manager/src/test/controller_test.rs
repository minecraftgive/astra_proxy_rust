use super::*;
use astra_management::config::{AgentConfig, ManagerConfig, Mode};
use sha2::Digest;

#[tokio::test]
async fn authenticated_agent_routes_requests_and_offline_mutations_are_not_queued() {
    let _ = tokio_rustls::rustls::crypto::aws_lc_rs::default_provider().install_default();
    let dir = tempfile::tempdir().unwrap();
    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()]).unwrap();
    let cert_path = dir.path().join("cert.pem");
    let key_path = dir.path().join("key.pem");
    std::fs::write(&cert_path, cert.cert.pem()).unwrap();
    std::fs::write(&key_path, cert.signing_key.serialize_pem()).unwrap();
    let token = "test-token-with-at-least-thirty-two-bytes";
    let token_path = dir.path().join("token");
    std::fs::write(&token_path, token).unwrap();
    let reservation = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = reservation.local_addr().unwrap();
    drop(reservation);
    let controller = Controller::new(ControllerConfig {
        listen: address,
        tls_cert: cert_path.clone(),
        tls_key: key_path,
        agent_token_hashes: [(
            "agent-a".into(),
            format!("{:x}", sha2::Sha256::digest(token)),
        )]
        .into(),
    });
    let listening = tokio::spawn(controller.clone().listen());
    let agent = crate::agent::AgentService::new(ManagerConfig {
        mode: Mode::Agent,
        state_dir: dir.path().join("state"),
        config_dir: dir.path().join("config"),
        bin_dir: dir.path().into(),
        controller: None,
        agent: AgentConfig {
            id: "agent-a".into(),
            name: "Test agent".into(),
            controller_url: Some(format!("wss://localhost:{}/agents/v2", address.port())),
            token_file: Some(token_path),
            ca_certificate: Some(cert_path),
        },
        ..ManagerConfig::default()
    })
    .unwrap();
    let uplink = tokio::spawn(super::uplink(agent));
    tokio::time::timeout(Duration::from_secs(10), async {
        while !controller.peers.lock().unwrap().contains_key("agent-a") {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let mut request = Request::new(
        "front-1",
        RequestBody::Query(Query::Events {
            after: None,
            limit: None,
        }),
    );
    request.agent_id = Some("agent-a".into());
    let result = controller.handle(request).await;
    assert_eq!(result.id, "front-1");
    assert!(matches!(result.result, Ok(ResponseBody::Events(_))));
    controller.replace_credentials(Default::default());
    tokio::time::timeout(Duration::from_secs(5), async {
        while controller.peers.lock().unwrap().contains_key("agent-a") {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    uplink.abort();
    let _ = uplink.await;
    tokio::time::timeout(Duration::from_secs(5), async {
        while controller.peers.lock().unwrap().contains_key("agent-a") {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    let mut request = Request::new(
        "front-2",
        RequestBody::Execute {
            operation_id: "never-execute".into(),
            command: Command::Start {
                instance: "proxy".into(),
            },
        },
    );
    request.agent_id = Some("agent-a".into());
    assert!(matches!(
        controller.handle(request).await.result,
        Err(ApiError {
            code: ErrorCode::Offline,
            ..
        })
    ));
    listening.abort();
}

#[tokio::test]
async fn controller_restarts_with_offline_identity_and_new_epoch() {
    let root = tempfile::tempdir().unwrap();
    let config = ControllerConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        tls_cert: "unused".into(),
        tls_key: "unused".into(),
        agent_token_hashes: Default::default(),
    };
    let first = Controller::open(config.clone(), root.path(), 128)
        .await
        .unwrap();
    let epoch = first.epoch.clone();
    first
        .index
        .as_ref()
        .unwrap()
        .remember_agent(Agent {
            id: "a".into(),
            name: "Known node".into(),
            boot_id: "boot".into(),
            online: true,
            last_seen_ms: "123".into(),
        })
        .await
        .unwrap();
    drop(first);
    let second = Controller::open(config, root.path(), 128).await.unwrap();
    assert_ne!(epoch, second.epoch);
    assert!(!second.known.lock().unwrap()["a"].online);
}

#[tokio::test]
async fn configured_but_disconnected_agent_is_visible_and_requires_explicit_target() {
    let controller = Controller::new(ControllerConfig {
        listen: "127.0.0.1:0".parse().unwrap(),
        tls_cert: "unused".into(),
        tls_key: "unused".into(),
        agent_token_hashes: [("offline".into(), "a".repeat(64))].into(),
    });
    let response = controller
        .handle(Request::new("1", RequestBody::Query(Query::Agents)))
        .await;
    let Ok(ResponseBody::Agents(agents)) = response.result else {
        panic!("expected agents")
    };
    assert_eq!(agents.len(), 1);
    assert!(!agents[0].online);
    let response = controller
        .handle(Request::new("2", RequestBody::Query(Query::Instances)))
        .await;
    assert!(matches!(
        response.result,
        Err(ApiError {
            code: ErrorCode::InvalidRequest,
            ..
        })
    ));
}
