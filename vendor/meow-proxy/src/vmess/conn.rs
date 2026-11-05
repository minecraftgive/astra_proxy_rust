use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::body::BodyCipher;
use super::header::{read_aead_response_header, response_body_keys, Security};
use crate::tasked_duplex::TaskedDuplex;

/// Spawn a VMess relay task that handles AEAD body record framing.
///
/// Returns a `TaskedDuplex` that the caller reads/writes plain bytes on.
/// The background read task first consumes the AEAD-sealed response header
/// (validating the per-connection `resp_v` byte), then encrypts writes into
/// body records and decrypts reads from body records on the underlying stream.
///
/// The body ciphers are built here — each relay half touches only its own
/// direction, so the read task gets `from_response_keys` (response schedule) and
/// the write task a `new_writer` (request schedule): one AEAD key expansion
/// per direction, and the response SHA-256 hop is shared with the header
/// decode (issue #533). Constructing both inside also makes a swapped
/// pairing unrepresentable.
///
/// Dropping the returned endpoint aborts both relay tasks — the bounded
/// cleanup path for a silent upstream that never sends data, FIN, or RST
/// (issue #514). A write-side `shutdown()` alone does not cancel anything:
/// the read direction keeps delivering upstream data (half-close download).
pub fn spawn_vmess_relay(
    stream: Box<dyn meow_transport::Stream>,
    security: Security,
    req_key: [u8; 16],
    req_iv: [u8; 16],
    resp_v: u8,
) -> TaskedDuplex {
    let (resp_body_key, resp_body_iv) = response_body_keys(&req_key, &req_iv);
    let mut read_cipher = BodyCipher::from_response_keys(security, &resp_body_key, &resp_body_iv);
    let mut write_cipher = BodyCipher::new_writer(security, &req_key, &req_iv);
    let (client, proxy) = tokio::io::duplex(32768);
    let (mut rd, mut wr) = tokio::io::split(stream);
    let (mut proxy_rd, mut proxy_wr) = tokio::io::split(proxy);

    // Upstream: consume the response header, then stream → decrypt →
    // proxy_wr. This runs concurrently with the write side: a conformant
    // server commonly waits for request data before sending its response
    // header, so awaiting the header before forwarding the request would
    // deadlock every request/response protocol.
    //
    // Returns `true` when the exchange ended on a clean EOF (peer FIN at a
    // record boundary / terminator record) — the write direction may still
    // have data in flight, so the supervisor leaves it running. `false`
    // marks a fatal end (handshake failure, decode error, transport error,
    // or the client endpoint gone), where the write side is stopped too.
    let read_task = tokio::spawn(async move {
        let mut buf = Vec::with_capacity(BodyCipher::max_plaintext() + crate::aead::TAG_LEN);
        if let Err(e) =
            read_aead_response_header(&mut rd, &resp_body_key, &resp_body_iv, resp_v).await
        {
            tracing::warn!("vmess: response header decode failed: {e}");
            let _ = proxy_wr.shutdown().await;
            return false;
        }

        let clean_eof = loop {
            match read_cipher.read_record_into(&mut rd, &mut buf).await {
                Ok(Some(())) => {
                    if proxy_wr.write_all(&buf).await.is_err() {
                        break false;
                    }
                }
                // FIN at a record boundary or the explicit terminator
                // record — the clean half-close; not worth a warn.
                Ok(None) => break true,
                // Everything else is a corrupt session: FIN mid-record
                // (partial length prefix or short body), decrypt failure,
                // nonce-budget exhaustion (issue #513 — otherwise an
                // unexplained ~1 GiB disconnect), or a transport error.
                // Fatal: the write side is stopped too.
                Err(e) => {
                    tracing::warn!("vmess: read side closed: {e}");
                    break false;
                }
            }
        };
        let _ = proxy_wr.shutdown().await;
        clean_eof
    });

    // Downstream: proxy_rd → encrypt → stream
    let write_task = tokio::spawn(async move {
        let mut buf = vec![0u8; BodyCipher::max_plaintext()];
        loop {
            let n = match proxy_rd.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => n,
            };
            if let Err(e) = write_cipher.write_record(&mut wr, &buf[..n]).await {
                // Same visibility: seal failure or nonce-budget
                // exhaustion (issue #513).
                tracing::warn!("vmess: write side closed: {e}");
                break;
            }
        }
        let _ = wr.shutdown().await;
    });

    let read_abort = read_task.abort_handle();
    let write_abort = write_task.abort_handle();

    // When the read side ends on a FATAL path — decode failure, transport
    // error, nonce budget — the exchange is over: stop the write side too.
    // A clean EOF (peer FIN at a record boundary) only half-closes the
    // exchange: the client may still be uploading, so the write side keeps
    // running until the caller half-closes or drops the endpoint (issue
    // #514 review follow-up). If the client endpoint is dropped first,
    // `TaskedDuplex` aborts both tasks and this supervisor resolves
    // immediately.
    tokio::spawn(async move {
        let clean_eof = match read_task.await {
            Ok(clean) => clean,
            Err(e) => {
                if e.is_panic() {
                    tracing::error!("vmess: read task panicked: {e}");
                }
                false
            }
        };
        if !clean_eof {
            write_abort.abort();
        }
    });

    TaskedDuplex::new(client, read_abort, write_task)
}

#[cfg(test)]
#[path = "../test/vmess_conn_test.rs"]
mod tests;
