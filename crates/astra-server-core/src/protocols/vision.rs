//! Server-side Vision padding and directional DIRECT boundaries.
use crate::{net::Stream, security::RawSwitch};
use anyhow::ensure;
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
                let mut content = vec![0; size];
                rd.read_exact(&mut content).await?;
                let mut discard = vec![0; padding];
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
