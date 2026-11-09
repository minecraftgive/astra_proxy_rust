use super::*;
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Debug, Serialize, Deserialize)]
struct OldIdentity {
    version: u16,
    boot_id: String,
    process: ProcessIdentity,
    applied: Option<Applied>,
}
fn identity_value(capabilities: Option<Vec<String>>) -> Identity {
    let mut value = astra_platform::lifecycle::identity(None).unwrap();
    value.capabilities = capabilities;
    value
}
fn path() -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    std::env::temp_dir().join(format!(
        "acap-{}-{}.sock",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    ))
}

#[test]
fn identity_json_is_optional_but_binary_layout_is_frozen() {
    let value = identity_value(Some(vec![FAKE_IP_LOOKUP_CAPABILITY.into()]));
    let old = OldIdentity {
        version: value.version,
        boot_id: value.boot_id.clone(),
        process: value.process.clone(),
        applied: value.applied.clone(),
    };
    assert_eq!(
        bincode::serialize(&value).unwrap(),
        bincode::serialize(&old).unwrap()
    );
    let plain = bincode::serialize(&old).unwrap();
    let decoded = identity_wire::decode(&plain).unwrap();
    assert_eq!(decoded.version, 1);
    assert_eq!(decoded.capabilities, None);
    let with_extension = identity_wire::encode(&value).unwrap();
    // Exact old struct and old decoder tolerate the framed tail unchanged.
    let legacy: OldIdentity = bincode::deserialize(&with_extension).unwrap();
    assert_eq!(legacy.boot_id, value.boot_id);
    assert_eq!(legacy.version, 1);
    assert_eq!(
        identity_wire::decode(&with_extension).unwrap().capabilities,
        value.capabilities
    );
    let new_json = serde_json::to_vec(&value).unwrap();
    let legacy: OldIdentity = serde_json::from_slice(&new_json).unwrap();
    assert_eq!(legacy.version, 1);
    assert_eq!(
        serde_json::from_slice::<Identity>(&serde_json::to_vec(&old).unwrap())
            .unwrap()
            .capabilities,
        None
    );
    assert_eq!(
        serde_json::from_slice::<Identity>(&new_json)
            .unwrap()
            .capabilities,
        value.capabilities
    );
    // Prior binary enum tags do not move when the query is appended.
    assert_eq!(
        &bincode::serialize(&Command::Identity).unwrap()[..4],
        &10u32.to_le_bytes()
    );
    assert_eq!(
        &bincode::serialize(&Command::Monitor).unwrap()[..4],
        &13u32.to_le_bytes()
    );
    assert_eq!(
        &bincode::serialize(&Response::Identity(value)).unwrap()[..4],
        &4u32.to_le_bytes()
    );
}

#[test]
fn corrupted_or_unknown_identity_extensions_are_protocol_errors() {
    let prefix = bincode::serialize(&identity_value(None)).unwrap();
    for tail in [
        b"unknown".as_slice(),
        b"ASTRA-CAPS\0{",
        b"ASTRA-CAPS\0{\"version\":2,\"capabilities\":[]}",
        b"ASTRA-CAPS\0{\"version\":1,\"capabilities\":null}",
    ] {
        let mut bytes = prefix.clone();
        bytes.extend_from_slice(tail);
        assert_eq!(
            identity_wire::decode(&bytes).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
    let mut oversized = prefix;
    oversized.extend_from_slice(b"ASTRA-CAPS\0");
    oversized.extend(vec![b' '; 4097]);
    assert_eq!(
        identity_wire::decode(&oversized).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
}

#[tokio::test]
async fn legacy_binary_receiver_reads_live_identity_with_extension() {
    let endpoint = path();
    let control = Control::bind(Some(endpoint.clone()))
        .unwrap()
        .with_capabilities(vec![FAKE_IP_LOOKUP_CAPABILITY.into()]);
    let server = tokio::spawn(async move { control.shutdown().await.unwrap() });
    let mut stream = astra_platform::platform::connect_trusted(&endpoint)
        .await
        .unwrap();
    crate::send(
        &mut stream,
        &Request {
            api_version: crate::API_VERSION,
            command: Command::Identity,
        },
    )
    .await
    .unwrap();
    let old: OldIdentity = crate::receive_management(&mut stream).await.unwrap();
    assert_eq!(old.version, 1);
    assert_eq!(old.process.pid, std::process::id());
    assert!(identity(&endpoint)
        .await
        .unwrap()
        .capabilities
        .unwrap()
        .contains(&FAKE_IP_LOOKUP_CAPABILITY.into()));
    request(&endpoint, Command::Stop).await.unwrap();
    server.await.unwrap();
}

#[tokio::test]
async fn missing_or_negative_capabilities_do_not_send_the_new_command() {
    for (capabilities, kind) in [
        (None, FakeIpLookupErrorKind::Unconfirmed),
        (Some(vec![]), FakeIpLookupErrorKind::Unsupported),
    ] {
        let endpoint = path();
        let listener = astra_platform::platform::bind_listener(&endpoint).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            assert!(matches!(
                crate::receive::<Request>(&mut stream)
                    .await
                    .unwrap()
                    .command,
                Command::Identity
            ));
            identity_wire::send(&mut stream, &identity_value(capabilities))
                .await
                .unwrap();
            drop(stream);
            assert!(
                tokio::time::timeout(Duration::from_millis(40), listener.accept())
                    .await
                    .is_err(),
                "must not send a query without positive capability evidence"
            );
        });
        let result = fake_ip_lookup(&endpoint, vec!["198.18.0.2".parse().unwrap()]).await;
        assert_eq!(result.unwrap_err().kind, kind);
        server.await.unwrap();
    }
}

#[tokio::test]
async fn lookup_is_bounded_and_bound_to_the_discovered_boot() {
    let endpoint = path();
    let calls = std::sync::Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    let control = Control::bind(Some(endpoint.clone()))
        .unwrap()
        .with_fake_ip_lookup(move |addresses| {
            observed.fetch_add(1, Ordering::SeqCst);
            async move {
                Ok(addresses
                    .into_iter()
                    .map(|address| FakeIpMapping {
                        address,
                        domain: None,
                    })
                    .collect())
            }
        });
    let server = tokio::spawn(async move { control.shutdown().await.unwrap() });
    let addresses = vec!["198.18.0.2".parse().unwrap(); 64];
    let response = fake_ip_lookup(&endpoint, addresses.clone()).await.unwrap();
    assert!(
        matches!(response.outcome, FakeIpLookupOutcome::Mappings(values) if values.len() == 64 && values.iter().all(|v| v.domain.is_none()))
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        fake_ip_lookup(&endpoint, vec![addresses[0]; 65])
            .await
            .unwrap_err()
            .kind,
        FakeIpLookupErrorKind::InvalidRequest
    );
    // Server independently checks the bound, bypassing client validation.
    let response = rpc(
        &endpoint,
        Command::FakeIpLookup {
            expected_boot_id: astra_platform::lifecycle::boot_id().into(),
            addresses: vec![addresses[0]; 65],
        },
        1,
    )
    .await
    .unwrap();
    assert!(matches!(
        response,
        Response::FakeIpLookup(FakeIpLookupResponse {
            outcome: FakeIpLookupOutcome::InvalidRequest(_),
            ..
        })
    ));
    let response = rpc(
        &endpoint,
        Command::FakeIpLookup {
            expected_boot_id: "previous-boot".into(),
            addresses,
        },
        1,
    )
    .await
    .unwrap();
    assert!(matches!(
        response,
        Response::FakeIpLookup(FakeIpLookupResponse {
            outcome: FakeIpLookupOutcome::CoreChanged,
            ..
        })
    ));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    request(&endpoint, Command::Stop).await.unwrap();
    server.await.unwrap();
}

#[test]
fn mismatched_responses_and_io_failures_are_not_unsupported() {
    let response = Response::Monitor(Err("unexpected".into()));
    assert_eq!(
        validate(response, "boot", &[]).unwrap_err().kind,
        FakeIpLookupErrorKind::Protocol
    );
    assert_eq!(
        io_error(io::Error::new(io::ErrorKind::InvalidData, "bad frame")).kind,
        FakeIpLookupErrorKind::Protocol
    );
    for kind in [
        io::ErrorKind::TimedOut,
        io::ErrorKind::ConnectionRefused,
        io::ErrorKind::UnexpectedEof,
    ] {
        assert_eq!(
            io_error(io::Error::new(kind, "I/O failure")).kind,
            FakeIpLookupErrorKind::Communication
        );
    }
    let response = Response::FakeIpLookup(FakeIpLookupResponse {
        version: 1,
        boot_id: "boot".into(),
        outcome: FakeIpLookupOutcome::Unsupported,
    });
    assert_eq!(
        validate(response, "boot", &[]).unwrap_err().kind,
        FakeIpLookupErrorKind::Unsupported
    );
    let response = Response::FakeIpLookup(FakeIpLookupResponse {
        version: 1,
        boot_id: "changed".into(),
        outcome: FakeIpLookupOutcome::Mappings(vec![]),
    });
    assert_eq!(
        validate(response, "boot", &[]).unwrap_err().kind,
        FakeIpLookupErrorKind::Protocol
    );
}

#[tokio::test]
async fn malformed_or_mismatched_live_replies_remain_protocol_errors() {
    use tokio::io::AsyncWriteExt;
    for malformed in [false, true] {
        let endpoint = path();
        let listener = astra_platform::platform::bind_listener(&endpoint).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let request: Request = crate::receive(&mut stream).await.unwrap();
            assert!(matches!(request.command, Command::Identity));
            identity_wire::send(
                &mut stream,
                &identity_value(Some(vec![FAKE_IP_LOOKUP_CAPABILITY.into()])),
            )
            .await
            .unwrap();
            drop(stream);
            let (mut stream, _) = listener.accept().await.unwrap();
            let request: Request = crate::receive(&mut stream).await.unwrap();
            assert!(matches!(request.command, Command::Session));
            crate::send_management(
                &mut stream,
                &crate::session::Event::<Response>::Ready(crate::API_VERSION),
            )
            .await
            .unwrap();
            let request: Request = crate::receive(&mut stream).await.unwrap();
            assert!(matches!(request.command, Command::FakeIpLookup { .. }));
            if malformed {
                stream.write_u32(4).await.unwrap();
                stream.write_all(&[255; 4]).await.unwrap();
            } else {
                crate::send_management(
                    &mut stream,
                    &crate::session::Event::Reply(Response::Monitor(Err("wrong response".into()))),
                )
                .await
                .unwrap();
            }
        });
        assert_eq!(
            fake_ip_lookup(&endpoint, vec![]).await.unwrap_err().kind,
            FakeIpLookupErrorKind::Protocol
        );
        server.await.unwrap();
    }
    // A connect failure must remain communication failure, never capability absence.
    assert_eq!(
        fake_ip_lookup(&path(), vec![]).await.unwrap_err().kind,
        FakeIpLookupErrorKind::Communication
    );
}
