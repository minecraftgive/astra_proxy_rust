//! Native AEAD VMess inbound used by Astra's independent server.
use super::{
    body::BodyCipher,
    header::{cmd_key, response_body_keys, Security},
    kdf::{kdf12, kdf16},
};
use crate::{aead::Aead, tasked_duplex::TaskedDuplex};
use aes::{
    cipher::{BlockDecrypt, KeyInit},
    Aes128,
};
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub struct Server {
    keys: Vec<[u8; 16]>,
    replay: Arc<Mutex<HashMap<[u8; 16], Instant>>>,
}
pub struct Accepted {
    pub stream: Box<dyn meow_transport::Stream>,
    pub host: String,
    pub port: u16,
    pub udp: bool,
    pub user: usize,
}
fn invalid(message: &str) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message)
}
impl Server {
    pub fn new(users: impl IntoIterator<Item = [u8; 16]>) -> Self {
        Self {
            keys: users.into_iter().map(|id| cmd_key(&id)).collect(),
            replay: Arc::new(Mutex::new(HashMap::new())),
        }
    }
    pub fn with_replay_from(mut self, previous: &Self) -> Self {
        self.replay = previous.replay.clone();
        self
    }
    pub async fn accept(
        &self,
        mut stream: Box<dyn meow_transport::Stream>,
    ) -> std::io::Result<Accepted> {
        let mut prefix = [0u8; 42];
        stream.read_exact(&mut prefix).await?;
        let auth: &[u8; 16] = prefix[..16].try_into().unwrap();
        let nonce = &prefix[34..42];
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|_| invalid("clock before epoch"))?
            .as_secs();
        let mut selected = None;
        for (index, key) in self.keys.iter().enumerate() {
            let mut plain = *auth;
            let idkey = kdf16(key, &[b"AES Auth ID Encryption"]);
            Aes128::new_from_slice(&idkey)
                .unwrap()
                .decrypt_block(aes::Block::from_mut_slice(&mut plain));
            let stamp = u64::from_be_bytes(plain[..8].try_into().unwrap());
            if stamp.abs_diff(now) <= 120
                && crc32fast::hash(&plain[..12])
                    == u32::from_be_bytes(plain[12..].try_into().unwrap())
            {
                selected = Some((index, key));
                break;
            }
        }
        let (user, key) = selected.ok_or_else(|| invalid("VMess authentication failed"))?;
        {
            let mut replay = self.replay.lock().unwrap();
            replay.retain(|_, at| at.elapsed() < Duration::from_secs(240));
            if replay.contains_key(auth) || replay.len() >= 65536 {
                return Err(invalid("VMess replay or authentication capacity exceeded"));
            }
            replay.insert(*auth, Instant::now());
        }
        let mut length = prefix[16..34].to_vec();
        Aead::aes_128_gcm(&kdf16(key, &[b"VMess Header AEAD Key_Length", auth, nonce]))
            .open_trailing(
                &kdf12(key, &[b"VMess Header AEAD Nonce_Length", auth, nonce]),
                auth,
                &mut length,
            )
            .map_err(|_| invalid("VMess header length authentication failed"))?;
        let n = u16::from_be_bytes(length[..2].try_into().unwrap()) as usize;
        if !(45..=1024).contains(&n) {
            return Err(invalid("VMess header length"));
        }
        let mut header = vec![0; n + 16];
        stream.read_exact(&mut header).await?;
        Aead::aes_128_gcm(&kdf16(key, &[b"VMess Header AEAD Key", auth, nonce]))
            .open_trailing(
                &kdf12(key, &[b"VMess Header AEAD Nonce", auth, nonce]),
                auth,
                &mut header,
            )
            .map_err(|_| invalid("VMess header authentication failed"))?;
        let expected = u32::from_be_bytes(header[n - 4..].try_into().unwrap());
        let hash = header[..n - 4].iter().fold(0x811c9dc5u32, |hash, b| {
            (hash ^ u32::from(*b)).wrapping_mul(0x01000193)
        });
        if hash != expected || header[0] != 1 || header[34] != 1 || header[36] != 0 {
            return Err(invalid("unsupported VMess version/options/checksum"));
        }
        let security = match header[35] & 15 {
            3 => Security::Aes128Gcm,
            4 => Security::ChaCha20Poly1305,
            5 => Security::None,
            _ => return Err(invalid("unsupported VMess security")),
        };
        let udp = match header[37] {
            1 => false,
            2 => true,
            _ => return Err(invalid("unsupported VMess command")),
        };
        let port = u16::from_be_bytes([header[38], header[39]]);
        if port == 0 {
            return Err(invalid("zero destination port"));
        }
        let mut cursor = std::io::Cursor::new(&header[41..n - 4]);
        use std::io::Read;
        let host = match header[40] {
            1 => {
                let mut ip = [0; 4];
                Read::read_exact(&mut cursor, &mut ip)?;
                std::net::Ipv4Addr::from(ip).to_string()
            }
            3 => {
                let mut ip = [0; 16];
                Read::read_exact(&mut cursor, &mut ip)?;
                std::net::Ipv6Addr::from(ip).to_string()
            }
            2 => {
                let mut len = [0];
                Read::read_exact(&mut cursor, &mut len)?;
                if len[0] == 0 {
                    return Err(invalid("empty VMess domain"));
                }
                let mut name = vec![0; len[0] as usize];
                Read::read_exact(&mut cursor, &mut name)?;
                String::from_utf8(name).map_err(|_| invalid("invalid VMess domain"))?
            }
            _ => return Err(invalid("VMess address type")),
        };
        if cursor.position() as usize + (header[35] >> 4) as usize != n - 45 {
            return Err(invalid("invalid VMess padding length"));
        }
        let req_iv: [u8; 16] = header[1..17].try_into().unwrap();
        let req_key: [u8; 16] = header[17..33].try_into().unwrap();
        let (resp_key, resp_iv) = response_body_keys(&req_key, &req_iv);
        let mut response = vec![header[33], 0, 0, 0];
        let mut length = (response.len() as u16).to_be_bytes().to_vec();
        Aead::aes_128_gcm(&kdf16(&resp_key, &[b"AEAD Resp Header Len Key"]))
            .seal_append(
                &kdf12(&resp_iv, &[b"AEAD Resp Header Len IV"]),
                &[],
                &mut length,
            )
            .map_err(|_| invalid("VMess response length"))?;
        Aead::aes_128_gcm(&kdf16(&resp_key, &[b"AEAD Resp Header Key"]))
            .seal_append(
                &kdf12(&resp_iv, &[b"AEAD Resp Header IV"]),
                &[],
                &mut response,
            )
            .map_err(|_| invalid("VMess response header"))?;
        stream.write_all(&length).await?;
        stream.write_all(&response).await?;
        let (mut rd, mut wr) = tokio::io::split(stream);
        let (client, proxy) = tokio::io::duplex(32768);
        let (mut pr, mut pw) = tokio::io::split(proxy);
        let mut read = BodyCipher::from_response_keys(security, &req_key, &req_iv);
        let mut write = BodyCipher::new_writer(security, &resp_key, &resp_iv);
        let reader = tokio::spawn(async move {
            let mut buf = vec![];
            let clean = loop {
                match read.read_record_into(&mut rd, &mut buf).await {
                    Ok(Some(())) => {
                        if udp && pw.write_u16(buf.len() as u16).await.is_err() {
                            break false;
                        }
                        if pw.write_all(&buf).await.is_err() {
                            break false;
                        }
                    }
                    Ok(None) => break true,
                    Err(_) => break false,
                }
            };
            let _ = pw.shutdown().await;
            clean
        });
        let writer = tokio::spawn(async move {
            let mut buf = vec![0; BodyCipher::max_plaintext()];
            loop {
                let n = if udp {
                    let Ok(n) = pr.read_u16().await else { break };
                    let n = n as usize;
                    if n > buf.len() || pr.read_exact(&mut buf[..n]).await.is_err() {
                        break;
                    }
                    n
                } else {
                    match pr.read(&mut buf).await {
                        Ok(0) | Err(_) => break,
                        Ok(n) => n,
                    }
                };
                if write.write_record(&mut wr, &buf[..n]).await.is_err() {
                    break;
                }
            }
            let _ = wr.shutdown().await;
        });
        let abort_read = reader.abort_handle();
        let abort_write = writer.abort_handle();
        tokio::spawn(async move {
            if !reader.await.unwrap_or(false) {
                abort_write.abort();
            }
        });
        Ok(Accepted {
            stream: Box::new(TaskedDuplex::new(client, abort_read, writer)),
            host,
            port,
            udp,
            user,
        })
    }
}
