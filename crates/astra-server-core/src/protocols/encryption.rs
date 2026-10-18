//! Inbound VLESS decryption; outbound `encryption` settings stay in the client.
use crate::{config::Decryption, net::Stream, protocol::Adapt, security::RawSwitch};
use anyhow::{ensure, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use meow_proxy::vless_server_decryption::{ServerInstance, ServerKey};
use std::{
    io,
    pin::Pin,
    sync::{atomic::Ordering, Arc},
    task::{Context as Cx, Poll},
};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
pub fn prepare(config: &Decryption) -> Result<Arc<ServerInstance>> {
    ensure!(
        !config.private_keys.is_empty() && config.private_keys.len() <= 8,
        "configure 1..8 decryption private keys"
    );
    ensure!(
        config.ticket_lifetime_secs <= 3600,
        "ticket lifetime exceeds 3600 seconds"
    );
    let mode = match config.mode.as_str() {
        "native" => 0,
        "xorpub" => 1,
        "random" => 2,
        _ => anyhow::bail!("decryption mode must be native/xorpub/random"),
    };
    let keys = config
        .private_keys
        .iter()
        .map(|key| {
            ServerKey::from_private(&URL_SAFE_NO_PAD.decode(key)?).map_err(anyhow::Error::msg)
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(Arc::new(
        ServerInstance::init(keys, mode, i64::from(config.ticket_lifetime_secs), 0, "")
            .map_err(anyhow::Error::msg)?,
    ))
}
pub async fn accept(stream: Stream, server: &ServerInstance) -> Result<(Stream, RawSwitch)> {
    let inner = server.handshake(Box::new(Adapt(stream))).await?;
    let raw = RawSwitch::default();
    Ok((
        Box::new(Decrypted {
            inner,
            raw: raw.clone(),
            read_switched: false,
            write_switched: false,
        }),
        raw,
    ))
}
struct Decrypted {
    inner: Box<dyn meow_transport::Stream>,
    raw: RawSwitch,
    read_switched: bool,
    write_switched: bool,
}
impl AsyncRead for Decrypted {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Cx<'_>,
        b: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if !self.read_switched && self.raw.read.load(Ordering::Acquire) {
            ensure_io(meow_proxy::vless_server_decryption::enable_server_raw_read(
                &mut *self.inner,
            ))?;
            self.read_switched = true;
        }
        Pin::new(&mut self.inner).poll_read(cx, b)
    }
}
fn ensure_io(valid: bool) -> io::Result<()> {
    if valid {
        Ok(())
    } else {
        Err(io::Error::other("decryption raw boundary unavailable"))
    }
}
impl AsyncWrite for Decrypted {
    fn poll_write(mut self: Pin<&mut Self>, cx: &mut Cx<'_>, b: &[u8]) -> Poll<io::Result<usize>> {
        if !self.write_switched && self.raw.write.load(Ordering::Acquire) {
            ensure_io(
                meow_proxy::vless_server_decryption::enable_server_raw_write(&mut *self.inner),
            )?;
            self.write_switched = true;
        }
        Pin::new(&mut self.inner).poll_write(cx, b)
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Cx<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Cx<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
