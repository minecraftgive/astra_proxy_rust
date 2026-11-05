use super::*;
use astra_model::nettool::Egress;
use astra_model::nettool::Probe;

#[tokio::test]
async fn force_start_is_explicit_and_preserves_existing_wire_actions() {
    use tokio::io::AsyncReadExt;
    let (mut client, mut server) = astra_platform::platform::stream_pair().await.unwrap();
    for (action, expected) in [
        (Action::Start("proxy".into()), 3u32),
        (
            Action::Monitor {
                name: "proxy".into(),
            },
            44u32,
        ),
        (Action::ForceStart("proxy".into()), 45u32),
        (Action::StartSilent("proxy".into()), 46u32),
    ] {
        astra_ipc::send_management(&mut client, &action)
            .await
            .unwrap();
        let size = server.read_u32().await.unwrap() as usize;
        let mut frame = vec![0; size];
        server.read_exact(&mut frame).await.unwrap();
        assert_eq!(&frame[..4], &expected.to_le_bytes());
        astra_ipc::send_management(&mut client, &action)
            .await
            .unwrap();
        let received: Action = astra_ipc::receive_management(&mut server).await.unwrap();
        assert_eq!(
            serde_json::to_value(received).unwrap(),
            serde_json::to_value(action).unwrap()
        );
    }
    let normal: crate::v2::Command = serde_json::from_value(serde_json::json!({
        "type": "start", "data": { "instance": "proxy" }
    }))
    .unwrap();
    assert!(matches!(normal, crate::v2::Command::Start { .. }));
    let silent: crate::v2::Command = serde_json::from_value(serde_json::json!({
        "type": "start_silent", "data": { "instance": "proxy" }
    }))
    .unwrap();
    assert!(matches!(silent, crate::v2::Command::StartSilent { instance } if instance == "proxy"));
    let force = crate::v2::Command::ForceStart {
        instance: "proxy".into(),
    };
    let encoded = serde_json::to_value(force).unwrap();
    assert_eq!(encoded["type"], "force_start");
    assert!(
        matches!(serde_json::from_value::<crate::v2::Command>(encoded).unwrap(),
            crate::v2::Command::ForceStart { instance } if instance == "proxy")
    );
}

#[tokio::test]
async fn update_actions_preserve_existing_discriminants_and_json_request_identity() {
    use tokio::io::AsyncReadExt;
    let (mut client, mut server) = astra_platform::platform::stream_pair().await.unwrap();
    for (action, expected) in [
        (Action::List, 0),
        (Action::UpdateStatus, 38),
        (Action::Rollback, 39),
        (Action::WatchUpdates, 42),
    ] {
        astra_ipc::send_management(&mut client, &action)
            .await
            .unwrap();
        assert_eq!(server.read_u32().await.unwrap(), 4);
        assert_eq!(server.read_u32_le().await.unwrap(), expected);
    }
    let request = updates::UpdateRequest::new(updates::UpdateIntent::Rollback {
        target_directory: PathBuf::from("fixed-release"),
    });
    astra_ipc::send_management(
        &mut client,
        &Action::UpdateTransaction {
            request: request.clone(),
        },
    )
    .await
    .unwrap();
    let received: Action = astra_ipc::receive_management(&mut server).await.unwrap();
    assert!(matches!(received,Action::UpdateTransaction {request:actual} if actual == request));
}

#[tokio::test]
async fn network_tool_requests_survive_management_frames() {
    let (mut client, mut server) = astra_platform::platform::stream_pair().await.unwrap();
    let egresses = [
        Egress::System,
        Egress::Interface("以太网 2".into()),
        Egress::Outbound("HK 01".into()),
    ];
    let probes = [
        Probe::Dns {
            name: "example.test".into(),
            record: "AAAA".into(),
            server: "tls://1.1.1.1".into(),
        },
        Probe::Tcp {
            host: "2001:db8::1".into(),
            port: 443,
        },
        Probe::Icmp {
            host: "192.0.2.1".into(),
        },
    ];
    for (egress, probe) in egresses.into_iter().zip(probes) {
        let request = astra_model::nettool::Request {
            config: "standalone=true\n".into(),
            egress: egress.clone(),
            probe: probe.clone(),
            count: 3,
            interval_ms: 200,
            timeout_ms: 3000,
        };
        astra_ipc::send_management(
            &mut client,
            &Request {
                version: astra_ipc::API_VERSION,
                action: Action::NetTool {
                    name: "proxy".into(),
                    request,
                },
            },
        )
        .await
        .unwrap();
        let received: Request = astra_ipc::receive_management(&mut server).await.unwrap();
        let (name, request) = match received.action {
            Action::NetTool { name, request } => (name, request),
            other => panic!("{other:?}"),
        };
        assert_eq!(name, "proxy");
        assert_eq!((request.egress, request.probe), (egress, probe));
        assert_eq!(
            (request.count, request.interval_ms, request.timeout_ms),
            (3, 200, 3000)
        );
        assert_eq!(request.config, "standalone=true\n");
    }
}
