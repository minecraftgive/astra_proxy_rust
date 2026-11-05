use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[test]
fn increment_nonce_wraps() {
    let mut n = [0xFFu8; V4_NONCE_SIZE];
    increment_nonce(&mut n);
    assert_eq!(n, [0u8; V4_NONCE_SIZE]);
}

#[test]
fn swap_padding_round_trips() {
    let mut p = vec![1u8, 2, 3, 4, 5, 6];
    let mut c = vec![10u8, 20, 30, 40, 50, 60];
    let (orig_p, orig_c) = (p.clone(), c.clone());
    swap_padding(&mut p, &mut c);
    swap_padding(&mut p, &mut c);
    assert_eq!(p, orig_p);
    assert_eq!(c, orig_c);
}

#[test]
fn bit_count_padding_has_requested_ones() {
    let pad = make_v4_bit_count_padding(16, 40).unwrap();
    let ones: u32 = pad.iter().map(|b| b.count_ones()).sum();
    assert_eq!(ones, 40);
}

/// End-to-end: two V4Conn instances over a duplex stream should round-trip
/// arbitrary payloads, including bursts that overflow the first-frame limit.
#[tokio::test]
async fn v4_round_trip_through_duplex() {
    let (a, b) = tokio::io::duplex(1 << 18);
    let psk: Arc<[u8]> = Arc::from(b"shared-secret".as_slice());
    let mut alice = V4Conn::new(a, Arc::clone(&psk));
    let mut bob = V4Conn::new(b, psk);

    // Alice writes a small payload, then a large burst.
    let small = b"hello-world";
    let large: Vec<u8> = (0..32_000u32).map(|i| (i % 251) as u8).collect();

    let writer = tokio::spawn(async move {
        alice.write_all(small).await.unwrap();
        alice.write_all(&large).await.unwrap();
        alice.flush().await.unwrap();
        // Keep alice alive for the read side to consume everything.
        alice
    });

    let mut got_small = vec![0u8; small.len()];
    bob.read_exact(&mut got_small).await.unwrap();
    assert_eq!(got_small, small);

    let mut got_large = vec![0u8; 32_000];
    bob.read_exact(&mut got_large).await.unwrap();
    let expected: Vec<u8> = (0..32_000u32).map(|i| (i % 251) as u8).collect();
    assert_eq!(got_large, expected);

    drop(writer.await.unwrap());
}

#[tokio::test]
async fn v4_zero_chunk_surfaces_as_tagged_error() {
    let (a, b) = tokio::io::duplex(8192);
    let psk: Arc<[u8]> = Arc::from(b"k".as_slice());
    let mut alice = V4Conn::new(a, Arc::clone(&psk));
    let mut bob = V4Conn::new(b, psk);

    // Alice sends one real frame, then a zero chunk. `write_all(&[])`
    // is a no-op in tokio because the future short-circuits on empty
    // input, so we drive `poll_write(&[])` by hand to actually emit
    // the zero-chunk frame.
    alice.write_all(b"abc").await.unwrap();
    alice.flush().await.unwrap();
    std::future::poll_fn(|cx| Pin::new(&mut alice).poll_write(cx, &[]))
        .await
        .unwrap();
    alice.flush().await.unwrap();

    let mut buf = [0u8; 3];
    bob.read_exact(&mut buf).await.unwrap();
    assert_eq!(&buf, b"abc");

    let mut more = [0u8; 1];
    let err = bob.read_exact(&mut more).await.unwrap_err();
    assert!(is_zero_chunk(&err), "expected zero-chunk tag, got {err:?}");
}
