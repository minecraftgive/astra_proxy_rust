use md5::{Digest, Md5};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[cfg(test)]
use super::header::response_body_keys;
use super::header::Security;
use crate::aead::{Aead, TAG_LEN};

/// Maximum plaintext per body record (matching upstream 16 KiB - 16 tag).
const MAX_PLAINTEXT: usize = 16384 - 16;

/// Body keys/IVs derived from the per-connection req_key and req_iv. The IVs
/// are the full 16-byte seeds; each record nonce is `count(2 BE) || iv[2..12]`.
/// Both directions' material at once — test-only; production builds each
/// direction separately (`new_writer`/`from_response_keys`) so no key
/// schedule is paid for twice (issue #533).
#[cfg(test)]
struct DerivedKeys {
    write_key: Vec<u8>,
    write_iv: [u8; 16],
    read_key: Vec<u8>,
    read_iv: [u8; 16],
}

/// Expand a 16-byte AEAD seed key into the actual cipher key for `security`.
///
/// - AES-128-GCM: the 16-byte key is used directly.
/// - ChaCha20-Poly1305: 32-byte key `MD5(k) || MD5(MD5(k))`.
///
/// upstream: `transport/vmess/conn.go` (`sendRequest`, per-security branch).
fn expand_body_key(security: Security, key16: &[u8; 16]) -> Vec<u8> {
    match security {
        Security::Aes128Gcm => key16.to_vec(),
        Security::ChaCha20Poly1305 => {
            let md5_1: [u8; 16] = Md5::digest(key16).into();
            let md5_2: [u8; 16] = Md5::digest(md5_1).into();
            let mut k = Vec::with_capacity(32);
            k.extend_from_slice(&md5_1);
            k.extend_from_slice(&md5_2);
            k
        }
        Security::None => Vec::new(),
    }
}

#[cfg(test)]
fn derive_keys(security: Security, req_key: &[u8; 16], req_iv: &[u8; 16]) -> DerivedKeys {
    // Request (write) direction uses the raw per-connection key/iv directly —
    // there is NO "VMess Body AEAD Key" KDF in the wire protocol.
    let write_key = expand_body_key(security, req_key);
    let write_iv = *req_iv;

    // Response (read) direction keys come from SHA-256 of the request key/iv.
    let (resp_key, resp_iv) = response_body_keys(req_key, req_iv);
    let read_key = expand_body_key(security, &resp_key);

    DerivedKeys {
        write_key,
        write_iv,
        read_key,
        read_iv: resp_iv,
    }
}

/// One direction's AEAD state. The cipher object (the expanded key schedule)
/// is built once per connection — only the nonce changes per record.
enum RecordCipher {
    None,
    /// Direction never constructed — a `BodyCipher` built via
    /// `new_writer`/`new_reader` carries only its own side's key schedule
    /// (issue #533). Distinct from `None` (the plaintext `security: none`
    /// codec): sealing/opening through it is a bug and must error, never
    /// silently emit plaintext records.
    Unbuilt,
    /// AES-128-GCM or ChaCha20-Poly1305, per the negotiated security.
    Aead(Aead),
}

impl RecordCipher {
    fn new(security: Security, key: &[u8]) -> Self {
        match security {
            Security::None => Self::None,
            Security::Aes128Gcm => Self::Aead(Aead::aes_128_gcm(key)),
            Security::ChaCha20Poly1305 => Self::Aead(Aead::chacha20_poly1305(key)),
        }
    }

    /// Encrypt `data` in place, writing the tag to `tag`.
    fn seal(&self, nonce: &[u8; 12], data: &mut [u8], tag: &mut [u8]) -> std::io::Result<()> {
        match self {
            Self::Unbuilt => Err(std::io::Error::other(
                "seal called on an unconstructed cipher direction",
            )),
            Self::None => Err(std::io::Error::other("seal called with Security::None")),
            Self::Aead(c) => c
                .seal_detached(nonce, &[], data, tag)
                .map_err(|_| std::io::Error::other("vmess body: record encrypt failed")),
        }
    }

    /// Verify and strip the trailing tag of `buf`, decrypting the rest in
    /// place.
    fn open(&self, nonce: &[u8; 12], buf: &mut Vec<u8>) -> std::io::Result<()> {
        match self {
            Self::Unbuilt => Err(std::io::Error::other(
                "open called on an unconstructed cipher direction",
            )),
            Self::None => Err(std::io::Error::other("open called with Security::None")),
            Self::Aead(c) => c
                .open_trailing(nonce, &[], buf)
                .map_err(|_| std::io::Error::other("vmess body: record authentication failed")),
        }
    }
}

/// Build a 16-byte-seed record nonce: `count(2 BE) || iv[2..12]`. The first
/// two IV bytes are discarded (overwritten by the counter), matching mihomo
/// `aead.go` — a scheme that XORs the counter into the full IV only agrees
/// when `iv[0]==iv[1]==0`, so its records fail to authenticate on real servers.
fn record_nonce(iv: &[u8; 16], counter: u16) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce[..2].copy_from_slice(&counter.to_be_bytes());
    nonce[2..].copy_from_slice(&iv[2..12]);
    nonce
}

/// Per-connection body cipher state for one direction — the other half is
/// `RecordCipher::Unbuilt` on values built via `new_writer`/`new_reader`/
/// `from_response_keys` (issue #533).
///
/// `*_counter` is one past the last nonce value used. The wire format packs
/// it into a u16, so record `0xFFFF` is the last safe one — a 65537th record
/// would reuse nonce 0 under the same key. Reaching `NONCE_BUDGET` retires
/// the connection (issue #513): the error propagates up the stream and the
/// flow re-dials. mihomo shares this wrap (v2fly `aead.go`), so a >~1 GiB
/// transfer on one connection is a deliberate divergence from upstream.
pub struct BodyCipher {
    write: RecordCipher,
    write_iv: [u8; 16],
    read: RecordCipher,
    read_iv: [u8; 16],
    write_counter: u32,
    read_counter: u32,
    /// Reused by the write direction so every record does not allocate a
    /// fresh frame buffer.
    write_buf: Vec<u8>,
}

/// Number of distinct nonces a u16 counter can produce. Mux logical flows
/// share this budget because they share the physical connection's keys.
const NONCE_BUDGET: u32 = 1 << 16;

impl BodyCipher {
    /// Build both directions' key schedules — test-only. Production relays
    /// use [`Self::new_writer`]/[`Self::from_response_keys`]: the two halves
    /// of a spawned relay each touch only their own direction, so a pair of
    /// `new`s pays for two AEAD key schedules that are never used
    /// (issue #533). Composed from the directional constructors so the test
    /// fixture can never drift from production (issue #533 review).
    #[cfg(test)]
    pub(crate) fn new(
        security: Security,
        req_key: &[u8; 16],
        req_iv: &[u8; 16],
        resp_v: u8,
    ) -> Self {
        // resp_v gates the response *header* validation (in header.rs), not the
        // body IV; the parameter is kept for call-site signature stability.
        let _ = resp_v;
        let w = Self::new_writer(security, req_key, req_iv);
        let r = Self::new_reader(security, req_key, req_iv);
        Self {
            write: w.write,
            write_iv: w.write_iv,
            read: r.read,
            read_iv: r.read_iv,
            write_counter: 0,
            read_counter: 0,
            write_buf: w.write_buf,
        }
    }

    /// Build only the request (write) direction's key schedule — the raw
    /// per-connection `req_key`/`req_iv` directly (there is no
    /// "VMess Body AEAD Key" KDF on the wire). The read half is `Unbuilt` —
    /// `read_record` on this value is a bug and errors rather than emitting
    /// or accepting anything.
    pub fn new_writer(security: Security, req_key: &[u8; 16], req_iv: &[u8; 16]) -> Self {
        Self {
            write: RecordCipher::new(security, &expand_body_key(security, req_key)),
            write_iv: *req_iv,
            read: RecordCipher::Unbuilt,
            read_iv: [0; 16],
            write_counter: 0,
            read_counter: 0,
            write_buf: Vec::new(),
        }
    }

    /// Build only the response (read) direction's key schedule; the write
    /// half is `Unbuilt`. Takes the request material — the SHA-256 hop to
    /// the response key/IV happens inside. See [`Self::new_writer`].
    ///
    /// `spawn_vmess_relay` calls [`Self::from_response_keys`] instead: it
    /// needs the derived pair for the response header anyway, so one
    /// derivation feeds both (issue #533 review). Test-only — production
    /// never derives the pair without also needing it for the header.
    #[cfg(test)]
    pub fn new_reader(security: Security, req_key: &[u8; 16], req_iv: &[u8; 16]) -> Self {
        let (resp_key, resp_iv) = response_body_keys(req_key, req_iv);
        Self::from_response_keys(security, &resp_key, &resp_iv)
    }

    /// The read-direction constructor for callers that already derived the
    /// response key/IV via [`response_body_keys`] — `spawn_vmess_relay`
    /// shares one derivation between the response header AEAD and the body
    /// reader (issue #533 review).
    pub fn from_response_keys(security: Security, resp_key: &[u8; 16], resp_iv: &[u8; 16]) -> Self {
        Self {
            write: RecordCipher::Unbuilt,
            write_iv: [0; 16],
            read: RecordCipher::new(security, &expand_body_key(security, resp_key)),
            read_iv: *resp_iv,
            write_counter: 0,
            read_counter: 0,
            write_buf: Vec::new(),
        }
    }

    /// Test hook: [`Self::new`] with its write schedule on the read side
    /// too, so the codec opens what it seals (real connections derive read
    /// keys from SHA-256 of req material).
    #[cfg(test)]
    fn loopback(security: Security, req_key: &[u8; 16], req_iv: &[u8; 16]) -> Self {
        let mut c = Self::new(security, req_key, req_iv, 0);
        c.read = Self::new(security, req_key, req_iv, 0).write;
        c.read_iv = c.write_iv;
        c
    }

    /// Test hook: the server-side mirror of [`Self::new`] — reads
    /// request-keyed records and writes response-keyed ones, i.e. the
    /// client constructor's two directions swapped. Lets duplex tests play
    /// a conformant VMess server without re-deriving keys by hand.
    #[cfg(test)]
    pub(crate) fn server_mirror(security: Security, req_key: &[u8; 16], req_iv: &[u8; 16]) -> Self {
        let mut c = Self::new(security, req_key, req_iv, 0);
        std::mem::swap(&mut c.write, &mut c.read);
        std::mem::swap(&mut c.write_iv, &mut c.read_iv);
        c
    }

    fn write_nonce(&mut self) -> std::io::Result<[u8; 12]> {
        if self.write_counter >= NONCE_BUDGET {
            return Err(std::io::Error::other(
                "vmess body nonce budget exhausted; retiring connection",
            ));
        }
        let nonce = record_nonce(&self.write_iv, self.write_counter as u16);
        self.write_counter += 1;
        Ok(nonce)
    }

    fn read_nonce(&mut self) -> std::io::Result<[u8; 12]> {
        if self.read_counter >= NONCE_BUDGET {
            return Err(std::io::Error::other(
                "vmess body nonce budget exhausted; retiring connection",
            ));
        }
        let nonce = record_nonce(&self.read_iv, self.read_counter as u16);
        self.read_counter += 1;
        Ok(nonce)
    }

    /// Encrypt and write one body record: [len(2 BE)][ciphertext + tag(16)].
    /// Length includes the tag.
    pub async fn write_record<W: AsyncWrite + Unpin>(
        &mut self,
        writer: &mut W,
        plaintext: &[u8],
    ) -> std::io::Result<()> {
        if matches!(self.write, RecordCipher::Unbuilt) {
            return Err(std::io::Error::other(
                "vmess body: write direction not built (reader cipher)",
            ));
        }
        if matches!(self.write, RecordCipher::None) {
            // OPT_STANDARD advertises chunk streaming even when the security
            // type is none, so the payload still carries the 2-byte size.
            let len = u16::try_from(plaintext.len())
                .map_err(|_| std::io::Error::other("vmess plaintext record too large"))?;
            writer.write_all(&len.to_be_bytes()).await?;
            writer.write_all(plaintext).await?;
            return writer.flush().await;
        }

        // Seal in place inside the finished frame, so the record leaves in
        // one write rather than a 2-byte length write and a body write.
        let len = u16::try_from(plaintext.len() + TAG_LEN)
            .map_err(|_| std::io::Error::other("vmess body record too large"))?;
        let nonce = self.write_nonce()?;
        let frame = &mut self.write_buf;
        frame.clear();
        frame.reserve(2 + usize::from(len));
        frame.extend_from_slice(&len.to_be_bytes());
        frame.extend_from_slice(plaintext);
        frame.resize(2 + usize::from(len), 0);
        let (data, tag) = frame[2..].split_at_mut(plaintext.len());
        self.write.seal(&nonce, data, tag)?;
        writer.write_all(frame).await?;
        writer.flush().await
    }

    /// Read and decrypt one body record.
    ///
    /// `Ok(None)` is the clean close: the peer's FIN landed exactly on a
    /// record boundary (zero bytes into the next length prefix), or it sent
    /// the protocol's zero-length terminator record. Every `Err` — a
    /// partial length prefix, a body shorter than the prefix promised,
    /// decrypt failure, nonce-budget exhaustion, transport error — means a
    /// corrupt session and is fatal for the exchange: a truncated AEAD
    /// record must not be mistaken for a half-close (issue #514 review).
    #[cfg(test)]
    pub async fn read_record<R: AsyncRead + Unpin>(
        &mut self,
        reader: &mut R,
    ) -> std::io::Result<Option<Vec<u8>>> {
        let mut buf = Vec::new();
        match self.read_record_into(reader, &mut buf).await? {
            Some(()) => Ok(Some(buf)),
            None => Ok(None),
        }
    }

    /// Read and decrypt one body record into a caller-owned buffer.
    ///
    /// The relay keeps this buffer across records, avoiding one allocation for
    /// every encrypted chunk. `Some(())` means `buf` contains a record;
    /// `None` is the clean close or explicit terminator.
    pub async fn read_record_into<R: AsyncRead + Unpin>(
        &mut self,
        reader: &mut R,
        buf: &mut Vec<u8>,
    ) -> std::io::Result<Option<()>> {
        // Two-phase length-prefix read: a bare `read` returning 0 is FIN
        // at a record boundary; `read_exact` alone cannot tell that apart
        // from a FIN arriving after part of the prefix already landed
        // (both surface as UnexpectedEof) — and mid-record EOF is fatal.
        // The Unbuilt check must precede the EOF paths too: a wrong-side
        // cipher returning `Ok(None)` would mask a wiring bug as a clean
        // half-close (issue #533 review).
        if matches!(self.read, RecordCipher::Unbuilt) {
            return Err(std::io::Error::other(
                "vmess body: read direction not built (writer cipher)",
            ));
        }
        buf.clear();
        let mut len_buf = [0u8; 2];
        match reader.read(&mut len_buf).await {
            Ok(0) => return Ok(None),
            Ok(n) => {
                reader.read_exact(&mut len_buf[n..]).await?;
            }
            Err(e) => return Err(e),
        }
        let len = u16::from_be_bytes(len_buf) as usize;
        if len == 0 {
            // Explicit chunk-streaming terminator record.
            return Ok(None);
        }

        if matches!(self.read, RecordCipher::None) {
            buf.resize(len, 0);
            reader.read_exact(buf).await?;
            return Ok(Some(()));
        }

        buf.resize(len, 0);
        reader.read_exact(buf).await?;
        let nonce = self.read_nonce()?;
        self.read.open(&nonce, buf)?;
        Ok(Some(()))
    }

    pub fn max_plaintext() -> usize {
        MAX_PLAINTEXT
    }
}

#[cfg(test)]
#[path = "../test/vmess_body_test.rs"]
mod tests;
