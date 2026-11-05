use super::*;

#[tokio::test]
async fn varied_frames_preserve_padding_and_directional_boundaries() {
    for end in [1, 2] {
        let uuid = [7; 16];
        let raw = RawSwitch::default();
        let (mut client, server) = tokio::io::duplex(1024);
        let mut stream = wrap(Box::new(server), uuid, Some(raw.clone()));
        let mut wire = uuid.to_vec();
        let mut expected = Vec::new();
        for (command, size, padding) in [
            (0, 65535usize, 65535usize),
            (0, 1, 0),
            (0, 0, 517),
            (end, 33, 1),
        ] {
            let content: Vec<_> = (0..size).map(|i| (i % 251) as u8).collect();
            wire.push(command);
            wire.extend((size as u16).to_be_bytes());
            wire.extend((padding as u16).to_be_bytes());
            wire.extend(&content);
            wire.resize(wire.len() + padding, 0);
            expected.extend(content);
        }
        wire.extend(b"raw-tail");
        expected.extend(b"raw-tail");
        let mut received = Vec::new();
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            tokio::try_join!(
                async {
                    for chunk in wire.chunks(997) {
                        client.write_all(chunk).await?;
                    }
                    client.shutdown().await
                },
                async { stream.read_to_end(&mut received).await.map(|_| ()) },
            )
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(received, expected);
        assert_eq!(raw.read.load(Ordering::Acquire), end == 2);
        assert!(!raw.write.load(Ordering::Acquire));
    }
}
