//! Server-side Vision padding and directional DIRECT boundaries.
use crate::{net::Stream, security::RawSwitch};
use anyhow::ensure;
use bytes::BytesMut;
use std::sync::atomic::Ordering;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub fn wrap(stream: Stream, uuid: [u8; 16], raw: Option<RawSwitch>) -> Stream {
    let (client, bridge) = tokio::io::duplex(65536);
    let (mut rd, mut wr) = tokio::io::split(stream);
    let (mut incoming, mut outgoing) = tokio::io::split(bridge);
    let raw_read = raw.clone();
    let read = tokio::spawn(async move {
        let result = async {
            let mut first = true;
            let mut content = BytesMut::new();
            let mut discard = BytesMut::new();
            loop {
                if first {
                    let mut id = [0; 16];
                    rd.read_exact(&mut id).await?;
                    ensure!(crate::protocol::secret(&uuid, &id), "Vision UUID");
                    first = false
                }
                let command = rd.read_u8().await?;
                ensure!(command <= 2, "Vision command");
                let size = rd.read_u16().await? as usize;
                let padding = rd.read_u16().await? as usize;
                content.resize(size, 0);
                rd.read_exact(&mut content).await?;
                discard.resize(padding, 0);
                rd.read_exact(&mut discard).await?;
                if !content.is_empty() {
                    outgoing.write_all(&content).await?;
                }
                if command != 0 {
                    if command == 2 {
                        let raw = raw_read.as_ref().ok_or_else(|| {
                            anyhow::anyhow!("DIRECT unavailable on this transport")
                        })?;
                        raw.read.store(true, Ordering::Release);
                        if std::env::var_os("ASTRA_SERVER_DEBUG").is_some() {
                            eprintln!("Server Vision: inbound DIRECT");
                        }
                    }
                    tokio::io::copy(&mut rd, &mut outgoing).await?;
                    break;
                }
            }
            Ok::<_, anyhow::Error>(())
        }
        .await;
        let _ = outgoing.shutdown().await;
        result
    });
    let write = tokio::spawn(async move {
        let result = async {
            let mut first = true;
            let mut framed = true;
            let mut seen_tls = false;
            let mut tls13 = false;
            let mut filter = vec![];
            let mut records = 0;
            let mut buf = vec![0; 16384];
            loop {
                let n = incoming.read(&mut buf).await?;
                if n == 0 {
                    break;
                }
                let body = &buf[..n];
                if framed {
                    records += 1;
                    if body.starts_with(&[22, 3]) {
                        seen_tls = true;
                    }
                    if filter.len() < 65536 && seen_tls && !tls13 {
                        filter.extend_from_slice(body);
                        tls13 = server_tls13(&filter);
                    }
                    let application = body.starts_with(&[23, 3]);
                    let command = if application && tls13 && raw.is_some() {
                        2
                    } else if application || !seen_tls || records >= 8 {
                        1
                    } else {
                        0
                    };
                    let padding = usize::from(rand::random::<u8>()) + 32;
                    let mut frame = vec![];
                    if first {
                        frame.extend(uuid);
                        first = false
                    }
                    frame.push(command);
                    frame.extend((n as u16).to_be_bytes());
                    frame.extend((padding as u16).to_be_bytes());
                    frame.extend(body);
                    frame.resize(frame.len() + padding, 0);
                    wr.write_all(&frame).await?;
                    wr.flush().await?;
                    if command != 0 {
                        framed = false;
                        if command == 2 {
                            raw.as_ref().unwrap().write.store(true, Ordering::Release);
                            if std::env::var_os("ASTRA_SERVER_DEBUG").is_some() {
                                eprintln!("Server Vision: outbound DIRECT");
                            }
                        }
                    }
                } else {
                    wr.write_all(body).await?;
                    wr.flush().await?;
                }
            }
            wr.shutdown().await?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        result
    });
    crate::transport::owned(client, vec![read.abort_handle(), write.abort_handle()])
}
fn server_tls13(bytes: &[u8]) -> bool {
    if bytes.len() < 44 || !bytes.starts_with(&[22, 3]) || bytes[5] != 2 {
        return false;
    }
    let n = u16::from_be_bytes([bytes[3], bytes[4]]) as usize;
    if bytes.len() < 5 + n {
        return false;
    }
    let sid = bytes[43] as usize;
    let at = 44 + sid;
    if at + 5 > 5 + n {
        return false;
    }
    let cipher = u16::from_be_bytes([bytes[at], bytes[at + 1]]);
    (0x1301..=0x1303).contains(&cipher)
        && bytes[at + 3..5 + n]
            .windows(6)
            .any(|v| v == [0, 43, 0, 2, 3, 4])
}

#[cfg(test)]
mod tests {
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
}
