#[test]
fn runtime_telemetry_json_is_additive_to_legacy_snapshot() {
    let snapshot = astra_model::runtime::Snapshot {
        pid: 42,
        upload: 123,
        ..Default::default()
    };
    let legacy = serde_json::to_string(&snapshot).unwrap();
    let view: super::RuntimeView = serde_json::from_str(&legacy).unwrap();
    assert_eq!(view.snapshot.pid, 42);
    assert!(view.telemetry.is_none());
    let enhanced = super::RuntimeView {
        snapshot,
        telemetry: Some(super::RuntimeTelemetry {
            host_boot_id: "boot".into(),
            sample_sequence: 7,
            ..Default::default()
        }),
    };
    let json = serde_json::to_string(&enhanced).unwrap();
    let old_reader: astra_model::runtime::Snapshot = serde_json::from_str(&json).unwrap();
    assert_eq!(old_reader.upload, 123);
    let new_reader: super::RuntimeView = serde_json::from_str(&json).unwrap();
    assert_eq!(new_reader.telemetry.unwrap().sample_sequence, 7);
}
#[test]
fn worker_negotiation_accepts_additions_but_rejects_missing_semantics() {
    let mut hello = super::Hello::worker("future-version");
    hello.capabilities.push("optional_future_feature".into());
    hello.validate_worker().unwrap();
    hello.capabilities.remove(0);
    assert_eq!(
        hello.validate_worker().unwrap_err().kind(),
        std::io::ErrorKind::Unsupported
    );
    let mut manager = super::Hello::worker("1");
    manager.role = super::PeerRole::Manager;
    assert!(manager.validate_worker().is_err());
}
use super::*;
use std::time::Duration;
use tokio::time::timeout;

#[tokio::test]
async fn control_json_roundtrip_and_version_rejection() {
    let (mut sender, mut receiver) = tokio::io::duplex(4096);
    let message = ControlEnvelope::new(17, ControlRequest::Drain(Drain { generation: 3 }));
    send_control(&mut sender, &message).await.unwrap();
    assert_eq!(
        receive_control::<_, ControlRequest>(&mut receiver)
            .await
            .unwrap(),
        message
    );
    let mut invalid = message;
    invalid.protocol_version += 1;
    assert_eq!(
        send_control(&mut sender, &invalid)
            .await
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidData
    );
    let bytes = serde_json::to_vec(&invalid).unwrap();
    sender.write_u32(bytes.len() as u32).await.unwrap();
    sender.write_all(&bytes).await.unwrap();
    assert_eq!(
        receive_control::<_, ControlRequest>(&mut receiver)
            .await
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidData
    );
}

#[tokio::test]
async fn oversized_control_rejected_before_payload_read() {
    let (mut sender, mut receiver) = tokio::io::duplex(8);
    sender
        .write_u32(MAX_CONTROL_BYTES as u32 + 1)
        .await
        .unwrap();
    assert_eq!(
        receive_control::<_, ControlRequest>(&mut receiver)
            .await
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidData
    );
}

async fn half_close<S: AsyncRead + AsyncWrite + Unpin + Send + 'static>(left: S, right: S) {
    let mut client = HostStream::new(left);
    let mut server = HostStream::new(right);
    let task = tokio::spawn(async move {
        let mut request = Vec::new();
        server.read_to_end(&mut request).await.unwrap();
        assert_eq!(request, vec![7; MAX_STREAM_DATA * 3 + 123]);
        server
            .write_all(b"response after upload FIN")
            .await
            .unwrap();
        server.shutdown().await.unwrap();
    });
    client
        .write_all(&vec![7; MAX_STREAM_DATA * 3 + 123])
        .await
        .unwrap();
    client.shutdown().await.unwrap();
    client.shutdown().await.unwrap();
    assert_eq!(
        client.write(b"late").await.unwrap_err().kind(),
        io::ErrorKind::BrokenPipe
    );
    let mut response = Vec::new();
    client.read_to_end(&mut response).await.unwrap();
    assert_eq!(response, b"response after upload FIN");
    task.await.unwrap();
}

#[tokio::test]
async fn half_close_over_fragmented_transport() {
    let (left, right) = tokio::io::duplex(3);
    timeout(Duration::from_secs(10), half_close(left, right))
        .await
        .unwrap();
}

#[tokio::test]
async fn half_close_over_platform_local_stream() {
    let (left, right) = astra_platform::platform::stream_pair().await.unwrap();
    timeout(Duration::from_secs(10), half_close(left, right))
        .await
        .unwrap();
}

#[tokio::test]
async fn reset_is_distinct_from_fin() {
    let (left, right) = tokio::io::duplex(128);
    let mut sender = HostStream::new(left);
    let mut receiver = HostStream::new(right);
    sender.reset().await.unwrap();
    let error = receiver.read(&mut [0; 1]).await.unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::ConnectionReset);
    assert_eq!(
        receiver.write(b"late").await.unwrap_err().kind(),
        io::ErrorKind::ConnectionReset
    );
}

#[tokio::test]
async fn malformed_and_truncated_streams_fail_closed() {
    for frame in [
        vec![DATA, 0, 1, 0, 0],
        vec![DATA, 0, 0, 0, 0],
        vec![FIN, 0, 0, 0, 1],
        vec![99, 0, 0, 0, 0],
    ] {
        let mut stream = HostStream::new(std::io::Cursor::new(frame));
        assert_eq!(
            stream.read(&mut [0; 1]).await.unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
    for frame in [vec![], vec![DATA, 0], vec![DATA, 0, 0, 0, 2, 42]] {
        let mut stream = HostStream::new(std::io::Cursor::new(frame));
        assert_eq!(
            stream
                .read_to_end(&mut Vec::new())
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::UnexpectedEof
        );
    }
}

#[tokio::test]
async fn output_backpressure_is_bounded_to_one_frame() {
    let (left, _right) = tokio::io::duplex(1);
    let mut stream = HostStream::new(left);
    assert!(timeout(
        Duration::from_millis(20),
        stream.write(&vec![0; MAX_STREAM_DATA * 2])
    )
    .await
    .is_err());
    assert_eq!(stream.output.len(), MAX_STREAM_DATA + HEADER_BYTES);
    assert_eq!(
        stream.write(b"different").await.unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    assert_eq!(stream.output.len(), MAX_STREAM_DATA + HEADER_BYTES);
}

#[tokio::test]
async fn write_then_read_handshake_does_not_need_flush() {
    timeout(Duration::from_secs(5), async {
        let (left, right) = tokio::io::duplex(3);
        let mut client = HostStream::new(left);
        let mut server = HostStream::new(right);
        let task = tokio::spawn(async move {
            let mut request = [0; 4];
            server.read_exact(&mut request).await.unwrap();
            assert_eq!(&request, b"ping");
            server.write_all(b"pong").await.unwrap();
        });
        client.write_all(b"ping").await.unwrap();
        let mut reply = [0; 4];
        client.read_exact(&mut reply).await.unwrap();
        assert_eq!(&reply, b"pong");
        task.await.unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn datagrams_preserve_empty_and_maximum_payloads() {
    let (mut sender, mut receiver) = tokio::io::duplex(128);
    let task = tokio::spawn(async move {
        send_datagram(&mut sender, &[]).await.unwrap();
        send_datagram(&mut sender, &vec![123; MAX_DATAGRAM_BYTES])
            .await
            .unwrap();
        assert_eq!(
            send_datagram(&mut sender, &vec![0; MAX_DATAGRAM_BYTES + 1])
                .await
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    });
    assert!(receive_datagram(&mut receiver).await.unwrap().is_empty());
    assert_eq!(
        receive_datagram(&mut receiver).await.unwrap(),
        vec![123; MAX_DATAGRAM_BYTES]
    );
    task.await.unwrap();
}

#[tokio::test]
async fn datagram_length_is_checked_before_allocation() {
    let (mut sender, mut receiver) = tokio::io::duplex(8);
    sender
        .write_u32(MAX_DATAGRAM_BYTES as u32 + 1)
        .await
        .unwrap();
    assert_eq!(
        receive_datagram(&mut receiver).await.unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
}
