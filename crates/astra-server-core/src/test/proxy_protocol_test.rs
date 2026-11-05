use super::*;
#[tokio::test]
async fn vmess_replay_is_rejected_across_configuration_generations() {
    use meow_proxy::vmess::{
        header::{cmd_key, seal_request_header},
        server::Server,
        Security,
    };
    let id = [7u8; 16];
    let metadata = meow_common::Metadata {
        host: "example.test".into(),
        dst_port: 443,
        ..Default::default()
    };
    let header = seal_request_header(&cmd_key(&id), Security::Aes128Gcm, &metadata, false).unwrap();
    let original = Server::new([id]);
    let (mut client, stream) = tokio::io::duplex(4096);
    client.write_all(&header.bytes).await.unwrap();
    let accepted = original.accept(Box::new(stream)).await.unwrap();
    assert_eq!(accepted.host, "example.test");
    assert_eq!(accepted.port, 443);
    drop(accepted);
    drop(client);
    let reloaded = Server::new([id]).with_replay_from(&original);
    let (mut client, stream) = tokio::io::duplex(4096);
    client.write_all(&header.bytes).await.unwrap();
    let error = reloaded
        .accept(Box::new(stream))
        .await
        .err()
        .expect("replayed VMess request accepted");
    assert!(error.to_string().contains("replay"));
}
