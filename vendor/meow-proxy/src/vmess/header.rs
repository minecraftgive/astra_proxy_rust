use aes::cipher::{BlockEncrypt, KeyInit};
use aes::Aes128;
use md5::{Digest, Md5};
use rand::RngCore;
use sha2::Sha256;
use std::net::IpAddr;
use tokio::io::{AsyncRead, AsyncReadExt};

use meow_common::Metadata;

use super::kdf::{kdf12, kdf16};
use crate::aead::{Aead, TAG_LEN};

/// "c48619fe-8f02-49e0-b9e9-edf763e17e21" — historical v2ray constant
const VMESS_MAGIC: &[u8] = b"c48619fe-8f02-49e0-b9e9-edf763e17e21";

const CMD_TCP: u8 = 0x01;
#[allow(dead_code)]
const CMD_UDP: u8 = 0x02;

const ADDR_IPV4: u8 = 0x01;
const ADDR_DOMAIN: u8 = 0x02;
const ADDR_IPV6: u8 = 0x03;

const OPT_STANDARD: u8 = 0x01;

/// Derive the 16-byte cmd_key from a UUID.
///
/// upstream: transport/vmess/user.go — cmd_key = MD5(UUID || MAGIC)
pub fn cmd_key(uuid: &[u8; 16]) -> [u8; 16] {
    let mut hasher = Md5::new();
    hasher.update(uuid);
    hasher.update(VMESS_MAGIC);
    hasher.finalize().into()
}

/// Security cipher identifier in the VMess header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Security {
    Aes128Gcm,
    ChaCha20Poly1305,
    None,
}

impl Security {
    fn to_nibble(self) -> u8 {
        match self {
            Security::Aes128Gcm => 0x03,
            Security::ChaCha20Poly1305 => 0x04,
            Security::None => 0x05,
        }
    }
}

/// Parsed auto cipher: pick AES-GCM on hardware AES, ChaCha20 otherwise.
pub fn auto_security() -> Security {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    if std::arch::is_x86_feature_detected!("aes") {
        return Security::Aes128Gcm;
    }
    #[cfg(target_arch = "aarch64")]
    {
        return Security::Aes128Gcm;
    }
    #[allow(unreachable_code)]
    Security::ChaCha20Poly1305
}

pub struct SealedHeader {
    pub bytes: Vec<u8>,
    pub req_key: [u8; 16],
    pub req_iv: [u8; 16],
    pub resp_v: u8,
}

/// Build and encrypt the full VMess AEAD request header.
///
/// Returns (encrypted_header_bytes, req_key, req_iv, resp_v) for the caller
/// to derive body cipher keys.
fn seal_header(
    cmd_key: &[u8; 16],
    security: Security,
    cmd: HeaderCmd<'_>,
) -> Result<SealedHeader, String> {
    let mut rng = rand::rng();

    // Generate per-connection random values
    let mut req_key = [0u8; 16];
    let mut req_iv = [0u8; 16];
    rng.fill_bytes(&mut req_key);
    rng.fill_bytes(&mut req_iv);
    let resp_v: u8 = (rng.next_u32() & 0xFF) as u8;

    // Connection nonce (8 random bytes) used to derive header key/iv
    let mut conn_nonce = [0u8; 8];
    rng.fill_bytes(&mut conn_nonce);

    // 1) Auth ID (16 bytes, AES-128-ECB encrypted)
    let auth_id = build_auth_id(cmd_key, &mut rng);

    // 2) Build plaintext header
    let plaintext = build_header_plaintext(&req_key, &req_iv, resp_v, security, cmd, &mut rng)?;

    // 3) Derive header encryption keys
    let header_key = kdf16(cmd_key, &[b"VMess Header AEAD Key", &auth_id, &conn_nonce]);
    let header_iv = kdf12(
        cmd_key,
        &[b"VMess Header AEAD Nonce", &auth_id, &conn_nonce],
    );

    // 4) Derive length encryption keys. The salts use an underscore
    //    (`Key_Length` / `Nonce_Length`) in v2ray/xray/mihomo — a space here
    //    mis-derives the key and the server fails to open the header.
    let length_key = kdf16(
        cmd_key,
        &[b"VMess Header AEAD Key_Length", &auth_id, &conn_nonce],
    );
    let length_iv = kdf12(
        cmd_key,
        &[b"VMess Header AEAD Nonce_Length", &auth_id, &conn_nonce],
    );

    // 5) Encrypt the length. The length field is the PLAINTEXT header length
    //    (the server reads L then reads L+16 for the tag); using the
    //    ciphertext length here would make the server over-read by 16 bytes.
    let header_len = plaintext.len() as u16;
    let mut encrypted_length = [0u8; 2 + TAG_LEN];
    encrypted_length[..2].copy_from_slice(&header_len.to_be_bytes());
    let (len_data, len_tag) = encrypted_length.split_at_mut(2);
    Aead::aes_128_gcm(&length_key)
        .seal_detached(&length_iv, &auth_id, len_data, len_tag)
        .map_err(|_| "vmess: length encrypt failed".to_string())?;

    // 6) Encrypt the header
    let mut encrypted_header = plaintext;
    Aead::aes_128_gcm(&header_key)
        .seal_append(&header_iv, &auth_id, &mut encrypted_header)
        .map_err(|_| "vmess: header encrypt failed".to_string())?;

    // 7) Assemble in the exact upstream order:
    //    auth_id(16) || encrypted_length(2+16) || conn_nonce(8) || encrypted_header(N+16)
    //    (v2ray SealVMessAEADHeader). conn_nonce sits AFTER the length block.
    let mut out = Vec::with_capacity(16 + encrypted_length.len() + 8 + encrypted_header.len());
    out.extend_from_slice(&auth_id);
    out.extend_from_slice(&encrypted_length);
    out.extend_from_slice(&conn_nonce);
    out.extend_from_slice(&encrypted_header);

    Ok(SealedHeader {
        bytes: out,
        req_key,
        req_iv,
        resp_v,
    })
}

/// Seal a VMess request header carrying the given destination and the TCP
/// or UDP command byte.
pub fn seal_request_header(
    cmd_key: &[u8; 16],
    security: Security,
    metadata: &Metadata,
    is_udp: bool,
) -> Result<SealedHeader, String> {
    let cmd = if is_udp {
        HeaderCmd::Udp(metadata)
    } else {
        HeaderCmd::Tcp(metadata)
    };
    seal_header(cmd_key, security, cmd)
}

/// Seal a VMess request header carrying CommandMux (0x03) and no
/// destination — the Mux.Cool-over-VMess session signaling (server:
/// sing-vmess service.go `case CommandMux: return HandleMuxConnection`).
pub fn seal_mux_request_header(
    cmd_key: &[u8; 16],
    security: Security,
) -> Result<SealedHeader, String> {
    seal_header(cmd_key, security, HeaderCmd::Mux)
}

/// Response body key/iv for AEAD VMess, derived from the request body
/// key/iv: `respBodyKey = SHA256(req_key)[..16]`, `respBodyIV = SHA256(req_iv)[..16]`.
///
/// upstream: `transport/vmess/conn.go` (`sendRequest`, AEAD branch)
pub fn response_body_keys(req_key: &[u8; 16], req_iv: &[u8; 16]) -> ([u8; 16], [u8; 16]) {
    let bk: [u8; 32] = Sha256::digest(req_key).into();
    let bi: [u8; 32] = Sha256::digest(req_iv).into();
    let mut resp_key = [0u8; 16];
    let mut resp_iv = [0u8; 16];
    resp_key.copy_from_slice(&bk[..16]);
    resp_iv.copy_from_slice(&bi[..16]);
    (resp_key, resp_iv)
}

/// Read and validate the AEAD-sealed VMess response header from `rd`, leaving
/// the reader positioned at the first response body record.
///
/// Wire layout (all AES-128-GCM, single-shot): `encrypted_length(2+16)` sealed
/// with (`AEAD Resp Header Len Key`/`IV`), then `encrypted_header(L+16)` sealed
/// with (`AEAD Resp Header Key`/`IV`). The header's first byte must equal the
/// per-connection response-verification byte (`resp_v`).
///
/// upstream: `transport/vmess/conn.go` (`recvResponse`)
pub async fn read_aead_response_header<R: AsyncRead + Unpin>(
    rd: &mut R,
    resp_body_key: &[u8; 16],
    resp_body_iv: &[u8; 16],
    resp_v: u8,
) -> std::io::Result<()> {
    let invalid = |msg: &'static str| std::io::Error::new(std::io::ErrorKind::InvalidData, msg);

    // Length block.
    let len_key = kdf16(resp_body_key, &[b"AEAD Resp Header Len Key"]);
    let len_iv = kdf12(resp_body_iv, &[b"AEAD Resp Header Len IV"]);
    let mut len_ct = [0u8; 2 + TAG_LEN];
    rd.read_exact(&mut len_ct).await?;
    let (len_pt, len_tag) = len_ct.split_at_mut(2);
    Aead::aes_128_gcm(&len_key)
        .open_detached(&len_iv, &[], len_pt, len_tag)
        .map_err(|_| invalid("vmess: response length AEAD open failed"))?;
    let hdr_len = u16::from_be_bytes([len_pt[0], len_pt[1]]) as usize;

    // Header block (L plaintext + 16 tag).
    let hdr_key = kdf16(resp_body_key, &[b"AEAD Resp Header Key"]);
    let hdr_iv = kdf12(resp_body_iv, &[b"AEAD Resp Header IV"]);
    let mut hdr = vec![0u8; hdr_len + TAG_LEN];
    rd.read_exact(&mut hdr).await?;
    Aead::aes_128_gcm(&hdr_key)
        .open_trailing(&hdr_iv, &[], &mut hdr)
        .map_err(|_| invalid("vmess: response header AEAD open failed"))?;
    if hdr.first() != Some(&resp_v) {
        return Err(invalid("vmess: response header verification byte mismatch"));
    }
    Ok(())
}

fn build_auth_id(cmd_key: &[u8; 16], rng: &mut impl RngCore) -> [u8; 16] {
    let auth_id_key = kdf16(cmd_key, &[b"AES Auth ID Encryption"]);

    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();

    let mut block = [0u8; 16];
    block[..8].copy_from_slice(&now.to_be_bytes());
    rng.fill_bytes(&mut block[8..12]);
    let crc = crc32fast::hash(&block[..12]);
    block[12..16].copy_from_slice(&crc.to_be_bytes());

    let aes = Aes128::new_from_slice(&auth_id_key).expect("AES-128 key is 16 bytes");
    aes.encrypt_block(aes::Block::from_mut_slice(&mut block));

    block
}

/// Request command byte (xray `RequestCommand*`).  Mux.Cool-over-VMess
/// signals the session with CommandMux (0x03) and no destination — the
/// server (sing-vmess `service.go`, present since 2022) routes it straight
/// into `HandleMuxConnection`.
const CMD_MUX: u8 = 0x03;

/// The header destination: `None` for CommandMux (no port/address follows
/// the command byte — mirrors sing-vmess `if command != CommandMux`).
#[derive(Clone, Copy)]
enum HeaderCmd<'a> {
    Tcp(&'a Metadata),
    Udp(&'a Metadata),
    Mux,
}

fn build_header_plaintext(
    req_key: &[u8; 16],
    req_iv: &[u8; 16],
    resp_v: u8,
    security: Security,
    cmd: HeaderCmd<'_>,
    rng: &mut impl RngCore,
) -> Result<Vec<u8>, String> {
    let padding_len = (rng.next_u32() % 16) as u8;
    let (cmd_byte, metadata) = match cmd {
        HeaderCmd::Tcp(m) => (CMD_TCP, Some(m)),
        HeaderCmd::Udp(m) => (CMD_UDP, Some(m)),
        HeaderCmd::Mux => (CMD_MUX, None),
    };

    let mut buf = Vec::with_capacity(64);
    buf.push(0x01); // version
    buf.extend_from_slice(req_iv);
    buf.extend_from_slice(req_key);
    buf.push(resp_v);
    buf.push(OPT_STANDARD); // opts: S=1
    buf.push((padding_len << 4) | security.to_nibble()); // p(4) || sec(4)
    buf.push(0x00); // reserved
    buf.push(cmd_byte);

    if let Some(metadata) = metadata {
        // Port (big-endian, BEFORE addr_type)
        buf.extend_from_slice(&metadata.dst_port.to_be_bytes());

        // Address encoding
        encode_address(&mut buf, metadata)?;
    }

    // Padding
    if padding_len > 0 {
        let mut pad = [0u8; 15];
        rng.fill_bytes(&mut pad[..padding_len as usize]);
        buf.extend_from_slice(&pad[..padding_len as usize]);
    }

    // FNV-1a hash of everything so far
    let hash = fnv1a32(&buf);
    buf.extend_from_slice(&hash.to_be_bytes());

    Ok(buf)
}

fn encode_address(buf: &mut Vec<u8>, metadata: &Metadata) -> Result<(), String> {
    if !metadata.host.is_empty() {
        let host = metadata.host.as_bytes();
        if host.len() > 255 {
            return Err(format!(
                "vmess: domain too long ({} bytes, max 255)",
                host.len()
            ));
        }
        buf.push(ADDR_DOMAIN);
        buf.push(host.len() as u8);
        buf.extend_from_slice(host);
    } else if let Some(ip) = metadata.dst_ip {
        match ip {
            IpAddr::V4(v4) => {
                buf.push(ADDR_IPV4);
                buf.extend_from_slice(&v4.octets());
            }
            IpAddr::V6(v6) => {
                buf.push(ADDR_IPV6);
                buf.extend_from_slice(&v6.octets());
            }
        }
    } else {
        return Err("vmess: no destination address".into());
    }
    Ok(())
}

fn fnv1a32(data: &[u8]) -> u32 {
    let mut hash = 0x811c_9dc5u32;
    for &byte in data {
        hash ^= byte as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

#[cfg(test)]
#[path = "../test/vmess_header_test.rs"]
pub(crate) mod tests;
