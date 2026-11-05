use super::*;
use crate::snell::v4::is_zero_chunk;
use sha2::{Digest, Sha256};
use std::future::Future;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};

const MODES: [SnellV6Mode; 3] = [
    SnellV6Mode::Default,
    SnellV6Mode::Unshaped,
    SnellV6Mode::UnsafeRaw,
];

async fn within<T>(fut: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), fut)
        .await
        .expect("test future timed out")
}

fn pattern(n: usize, mul: usize, add: usize) -> Vec<u8> {
    (0..n).map(|i| (i * mul + add) as u8).collect()
}

/// Two ends of one v6 stream. The framing is symmetric — each direction
/// opens with its own salt — so a `V6Conn` also stands in for the server.
fn pair_with(
    client_mode: SnellV6Mode,
    server_mode: SnellV6Mode,
) -> (V6Conn<DuplexStream>, V6Conn<DuplexStream>) {
    let psk: Arc<[u8]> = Arc::from(b"v6-unit-psk".as_slice());
    let (a, b) = tokio::io::duplex(1 << 16);
    (
        V6Conn::new(a, Arc::clone(&psk), V6Codec::new(client_mode, &psk)),
        V6Conn::new(b, Arc::clone(&psk), V6Codec::new(server_mode, &psk)),
    )
}

fn pair(mode: SnellV6Mode) -> (V6Conn<DuplexStream>, V6Conn<DuplexStream>) {
    pair_with(mode, mode)
}

async fn send_zero_chunk(conn: &mut V6Conn<DuplexStream>) {
    while std::future::poll_fn(|cx| Pin::new(&mut *conn).poll_write(cx, &[]))
        .await
        .unwrap()
        != 0
    {}
    conn.flush().await.unwrap();
}

/// Everything up to the peer's zero chunk.
async fn read_session(conn: &mut V6Conn<DuplexStream>) -> Vec<u8> {
    let mut got = Vec::new();
    let mut buf = vec![0u8; 8192];
    loop {
        match conn.read(&mut buf).await {
            Ok(0) => panic!("EOF before zero chunk"),
            Ok(n) => got.extend_from_slice(&buf[..n]),
            Err(e) if is_zero_chunk(&e) => return got,
            Err(e) => panic!("read failed: {e}"),
        }
    }
}

#[tokio::test]
async fn request_and_bulk_data_round_trip_in_every_mode() {
    for mode in MODES {
        let (mut client, mut server) = pair(mode);
        let data = pattern(300_000, 13, 7);
        client.defer_request(b"REQUEST".to_vec());
        let writer = async {
            client.write_all(&data).await.unwrap();
            send_zero_chunk(&mut client).await;
            client
        };
        let (_client, got) =
            within(async { tokio::join!(writer, read_session(&mut server)) }).await;
        assert_eq!(&got[..7], b"REQUEST", "{mode:?}");
        assert!(got[7..] == data[..], "{mode:?}: payload mismatch");
    }
}

#[tokio::test]
async fn request_shares_the_first_record_with_data() {
    for mode in MODES {
        let (mut client, mut server) = pair(mode);
        client.defer_request(b"REQ".to_vec());
        within(client.write_all(b"hello")).await.unwrap();
        let mut buf = [0u8; 64];
        let n = within(server.read(&mut buf)).await.unwrap();
        assert_eq!(&buf[..n], b"REQhello", "{mode:?}");
    }
}

#[tokio::test]
async fn reading_first_sends_the_request_alone() {
    for mode in MODES {
        let (mut client, mut server) = pair(mode);
        client.defer_request(b"REQ".to_vec());
        let server_side = async {
            let mut buf = [0u8; 64];
            let n = server.read(&mut buf).await.unwrap();
            assert_eq!(&buf[..n], b"REQ", "{mode:?}");
            server.write_all(b"banner").await.unwrap();
            server.flush().await.unwrap();
        };
        let client_side = async {
            let mut buf = [0u8; 6];
            client.read_exact(&mut buf).await.unwrap();
            buf
        };
        let ((), got) = within(async { tokio::join!(server_side, client_side) }).await;
        assert_eq!(&got, b"banner", "{mode:?}");
    }
}

#[tokio::test]
async fn closing_without_data_sends_request_then_zero_chunk() {
    for mode in MODES {
        let (mut client, mut server) = pair(mode);
        client.defer_request(b"REQ".to_vec());
        within(send_zero_chunk(&mut client)).await;
        assert_eq!(within(read_session(&mut server)).await, b"REQ", "{mode:?}");
    }
}

#[tokio::test]
async fn sessions_continue_on_one_record_stream() {
    for mode in MODES {
        let (mut client, mut server) = pair(mode);
        for session in 0..4u8 {
            let request = vec![b'R', session];
            let upload = pattern(5000 + usize::from(session) * 900, 3, session.into());
            let download = pattern(7000 - usize::from(session) * 500, 5, session.into());
            client.defer_request(request.clone());
            let client_side = async {
                client.write_all(&upload).await.unwrap();
                send_zero_chunk(&mut client).await;
                read_session(&mut client).await
            };
            let server_side = async {
                let got = read_session(&mut server).await;
                server.write_all(&download).await.unwrap();
                send_zero_chunk(&mut server).await;
                got
            };
            let (downloaded, uploaded) =
                within(async { tokio::join!(client_side, server_side) }).await;
            assert_eq!(uploaded[..2], request[..], "{mode:?} #{session}");
            assert!(uploaded[2..] == upload[..], "{mode:?} #{session}");
            assert!(downloaded == download, "{mode:?} #{session}");
        }
    }
}

#[tokio::test]
async fn packet_frames_keep_datagram_boundaries() {
    for mode in MODES {
        let (mut client, mut server) = pair(mode);
        let packets = [
            pattern(1, 1, 9),
            pattern(1400, 7, 1),
            pattern(MAX_RECORD_PAYLOAD, 3, 2),
        ];
        let client_side = async {
            for packet in &packets {
                client.stage_packet_frame(packet).unwrap();
                client.flush().await.unwrap();
            }
        };
        let server_side = async {
            let mut buf = vec![0u8; MAX_RECORD_PAYLOAD];
            let mut got = Vec::new();
            for _ in 0..packets.len() {
                let n = server.read(&mut buf).await.unwrap();
                got.push(buf[..n].to_vec());
            }
            got
        };
        let ((), got) = within(async { tokio::join!(client_side, server_side) }).await;
        assert!(got == packets, "{mode:?}");
        assert_eq!(
            client
                .stage_packet_frame(&vec![0; MAX_RECORD_PAYLOAD + 1])
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }
}

#[tokio::test]
async fn plain_reader_rejects_padding_and_reserved_bytes() {
    for (record, reason) in [
        (
            &[4u8, 0, 0, 0, 1, 0, 1, 0xaa, 0xbb][..],
            "unexpected padding",
        ),
        (&[4u8, 1, 0, 0, 0, 0, 1, 0xbb][..], "reserved header bytes"),
        (&[5u8, 0, 0, 0, 0, 0, 1, 0xbb][..], "bad record version"),
    ] {
        let (mut raw, b) = tokio::io::duplex(1 << 10);
        let psk: Arc<[u8]> = Arc::from(b"k".as_slice());
        let mut conn = V6Conn::new(
            b,
            Arc::clone(&psk),
            V6Codec::new(SnellV6Mode::UnsafeRaw, &psk),
        );
        raw.write_all(record).await.unwrap();
        let err = within(conn.read(&mut [0u8; 16])).await.unwrap_err();
        assert!(err.to_string().contains(reason), "{err}");
    }
}

#[tokio::test]
async fn mode_mismatch_fails_instead_of_yielding_garbage() {
    use SnellV6Mode::{Default, UnsafeRaw, Unshaped};
    for (client_mode, server_mode) in [
        (Default, Unshaped),
        (Unshaped, Default),
        (Unshaped, UnsafeRaw),
        (UnsafeRaw, Unshaped),
        (Default, UnsafeRaw),
    ] {
        let (mut client, mut server) = pair_with(client_mode, server_mode);
        client.defer_request(b"REQ".to_vec());
        let writer = async {
            client.write_all(&pattern(4000, 1, 0)).await.unwrap();
            // Dropping the client closes the transport, so a reader
            // still waiting for bytes sees EOF instead of hanging.
            drop(client);
        };
        let reader = async {
            let mut buf = vec![0u8; 8192];
            loop {
                match server.read(&mut buf).await {
                    Ok(0) => break Err("clean EOF"),
                    Ok(_) => {}
                    Err(e) => break Ok(e),
                }
            }
        };
        let ((), result) = within(async { tokio::join!(writer, reader) }).await;
        assert!(
            result.is_ok(),
            "{client_mode:?} -> {server_mode:?} was not detected"
        );
    }
}

#[test]
fn mode_names_round_trip() {
    assert_eq!(SnellV6Mode::parse(""), Some(SnellV6Mode::Default));
    for mode in MODES {
        assert_eq!(SnellV6Mode::parse(mode.as_str()), Some(mode));
        let psk = b"k";
        assert_eq!(V6Codec::new(mode, psk).mode(), mode);
    }
    assert_eq!(SnellV6Mode::parse("shaped"), None);
}

/// `write(3000)`, zero chunk, 700-byte datagram on a fresh writer with
/// a fixed salt and clock.
fn scripted_wire(mode: SnellV6Mode, psk: &str) -> Vec<u8> {
    const NOW: i64 = 1_700_000_000;
    let codec = V6Codec::new(mode, psk.as_bytes());
    let salt = std::array::from_fn(|i| 0xa0 + i as u8);
    let mut send = SendHalf::with_salt(psk.as_bytes(), &codec.0, salt);
    send.push_stream(&codec.0, &pattern(3000, 7, 3), NOW)
        .unwrap();
    send.push_zero_chunk(&codec.0, NOW).unwrap();
    send.push_packet(&codec.0, &pattern(700, 11, 5), NOW)
        .unwrap();
    send.out
}

/// Regression pin on the exact bytes each framing writes. Recorded once
/// the writer interoperated with a real snell-server v6.
#[test]
fn scripted_wire_is_stable() {
    let mut h = Sha256::new();
    for i in 0..64 {
        h.update(scripted_wire(SnellV6Mode::Default, &format!("v6-wire-{i}")));
    }
    h.update(scripted_wire(SnellV6Mode::Unshaped, "v6-wire"));
    h.update(scripted_wire(SnellV6Mode::UnsafeRaw, "v6-wire"));
    assert_eq!(
        hex::encode(h.finalize()),
        "78d692abc31b7006e784f66a434233a0439dcfe177086dd8365296dd92499dfd"
    );
}

#[tokio::test]
async fn scripted_wire_decodes() {
    for mode in MODES {
        let psk = "v6-wire";
        let wire = scripted_wire(mode, psk);
        let (mut raw, b) = tokio::io::duplex(1 << 16);
        let psk: Arc<[u8]> = Arc::from(psk.as_bytes());
        let mut conn = V6Conn::new(b, Arc::clone(&psk), V6Codec::new(mode, &psk));
        let feed = async {
            raw.write_all(&wire).await.unwrap();
            raw
        };
        let read = async {
            let stream = read_session(&mut conn).await;
            let mut packet = vec![0u8; 1024];
            let n = conn.read(&mut packet).await.unwrap();
            (stream, packet[..n].to_vec())
        };
        let (_raw, (stream, packet)) = within(async { tokio::join!(feed, read) }).await;
        assert!(stream == pattern(3000, 7, 3), "{mode:?}");
        assert!(packet == pattern(700, 11, 5), "{mode:?}");
    }
}
