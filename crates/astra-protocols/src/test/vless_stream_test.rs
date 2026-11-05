use super::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test]
async fn client_first_and_fragmented_response_with_addons() {
    let (client, mut server) = tokio::io::duplex(1);
    let task = tokio::spawn(async move {
        let mut request = [0; 4];
        server.read_exact(&mut request).await.unwrap();
        assert_eq!(&request, b"ping");
        server.write_all(b"\x00\x03abcdata").await.unwrap();
        server.shutdown().await.unwrap();
    });
    let mut stream = VlessStream::new(client);
    stream.write_all(b"ping").await.unwrap();
    let mut payload = Vec::new();
    stream.read_to_end(&mut payload).await.unwrap();
    assert_eq!(payload, b"data");
    task.await.unwrap();
}

#[tokio::test]
async fn rejects_truncated_and_invalid_headers() {
    for bytes in [&b"\x00"[..], &b"\x01\x00"[..], &b"\x00\x02a"[..]] {
        let mut stream = VlessStream::new(bytes);
        assert!(stream.read_u8().await.is_err());
    }
}
