use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[test]
fn increment_nonce_wraps() {
    let mut n = [0xFFu8; V3_NONCE_SIZE];
    increment_nonce(&mut n);
    assert_eq!(n, [0u8; V3_NONCE_SIZE]);
}

#[tokio::test]
async fn v3_round_trip_through_duplex() {
    let (a, b) = tokio::io::duplex(1 << 18);
    let psk: Arc<[u8]> = Arc::from(b"shared-secret".as_slice());
    let mut alice = V3Conn::new(a, Arc::clone(&psk));
    let mut bob = V3Conn::new(b, psk);

    let small = b"hello-v3";
    let large: Vec<u8> = (0..32_000u32).map(|i| (i % 251) as u8).collect();
    let large_to_write = large.clone();

    let writer = tokio::spawn(async move {
        alice.write_all(small).await.unwrap();
        alice.write_all(&large_to_write).await.unwrap();
        alice.flush().await.unwrap();
        alice
    });

    let mut got_small = vec![0u8; small.len()];
    bob.read_exact(&mut got_small).await.unwrap();
    assert_eq!(got_small, small);

    let mut got_large = vec![0u8; 32_000];
    bob.read_exact(&mut got_large).await.unwrap();
    assert_eq!(got_large, large);

    drop(writer.await.unwrap());
}

#[tokio::test]
async fn v3_zero_chunk_surfaces_as_tagged_error() {
    let (a, b) = tokio::io::duplex(8192);
    let psk: Arc<[u8]> = Arc::from(b"k".as_slice());
    let mut alice = V3Conn::new(a, Arc::clone(&psk));
    let mut bob = V3Conn::new(b, psk);

    alice.write_all(b"abc").await.unwrap();
    alice.flush().await.unwrap();
    std::future::poll_fn(|cx| Pin::new(&mut alice).poll_write(cx, &[]))
        .await
        .unwrap();
    alice.flush().await.unwrap();

    let mut got = [0u8; 3];
    bob.read_exact(&mut got).await.unwrap();
    assert_eq!(&got, b"abc");

    let mut scratch = [0u8; 1];
    let err = bob.read(&mut scratch).await.unwrap_err();
    assert!(is_zero_chunk(&err), "expected zero-chunk tag, got {err:?}");
}
