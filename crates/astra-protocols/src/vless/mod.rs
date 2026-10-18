use crate::outbound::{tls_connect, Stream};
use anyhow::{bail, Context, Result};
use astra_ipc::Target;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use uuid::Uuid;
mod stream;
use stream::VlessStream;
pub async fn vless_tls(server: &str, sni: &str, uuid: &str, target: &Target) -> Result<Stream> {
    let mut tls = tls_connect(server, sni).await?;
    let user = Uuid::parse_str(uuid).context("invalid VLESS UUID")?;
    let mut header = Vec::with_capacity(320);
    header.push(0); // protocol version
    header.extend_from_slice(user.as_bytes());
    header.push(0); // addons length: no Vision flow
    header.push(1); // TCP
    header.extend_from_slice(&target.port.to_be_bytes());
    let address = crate::wire::encode(target, 1)?;
    header.extend_from_slice(&address[..address.len() - 2]);
    tls.write_all(&header).await?;
    tls.flush().await?;
    Ok(Box::new(VlessStream::new(tls)))
}

use crate::{
    accept::{secret, Request as Route},
    wire,
};
use anyhow::ensure;
pub async fn accept<S: tokio::io::AsyncRead + Unpin>(
    stream: &mut S,
    users: &[crate::accept::User],
    multiplex: bool,
    expected_flow: Option<&str>,
) -> Result<(crate::accept::Request, Option<[u8; 16]>)> {
    let mut vision = None;

    ensure!(stream.read_u8().await? == 0, "VLESS version");
    let mut id = [0; 16];
    stream.read_exact(&mut id).await?;
    ensure!(
        users
            .iter()
            .any(|u| u.uuid.is_some_and(|v| secret(v.as_bytes(), &id))),
        "authentication failed"
    );
    let n = stream.read_u8().await? as usize;
    let mut addons = vec![0; n];
    stream.read_exact(&mut addons).await?;
    let flow = if n == 0 {
        None
    } else {
        ensure!(
            n >= 2 && addons[0] == 10 && addons[1] as usize == n - 2,
            "invalid VLESS addons"
        );
        Some(std::str::from_utf8(&addons[2..])?)
    };
    let command = stream.read_u8().await?;
    if command == 3 {
        ensure!(multiplex && flow.is_none(), "Mux.Cool disabled");
        return Ok((Route::MuxCool, None));
    }
    if command == 1 {
        ensure!(flow == expected_flow, "VLESS flow mismatch");
        if flow.is_some() {
            vision = Some(id);
        }
    } else {
        ensure!(flow.is_none(), "Vision UDP requires ordinary VLESS UDP");
    }
    let target = wire::address(stream, 1, true).await?;
    let route = match command {
        1 => Route::Tcp(target),
        2 => Route::Udp(target),
        _ => bail!("VLESS command unsupported"),
    };
    Ok((route, vision))
}

mod adapter {
    use crate::native::*;
    use anyhow::{bail, Result};
    use meow_common::ProxyAdapter;
    use std::{path::Path, sync::Arc};
    pub fn build(
        tag: &str,
        cfg: &NodeConfig,
        base: &Path,
        dialer: Arc<meow_proxy::dialer::DirectDialer>,
        mux: Option<meow_proxy::mux::MuxOptions>,
    ) -> Result<Arc<dyn ProxyAdapter>> {
        let id = uuid::Uuid::parse_str(required(&cfg.uuid, "uuid")?)?;
        let flow = match cfg.flow.as_deref() {
            None | Some("") => None,
            Some("xtls-rprx-vision") => {
                if cfg.tls.is_none() {
                    bail!("Vision requires TLS/REALITY");
                }
                if cfg.transport.kind != TransportKind::Tcp
                    && !cfg.encryption.as_deref().is_some_and(|v| {
                        v.starts_with("mlkem768x25519plus.native.")
                            || v.starts_with("mlkem768x25519plus.xorpub.")
                    })
                {
                    bail!("Vision over an HTTP transport requires native/xorpub VLESS Encryption");
                }
                if mux.is_some() {
                    bail!("Vision with mux is not supported in this release");
                }
                Some(meow_proxy::VlessFlow::XtlsRprxVision)
            }
            Some(_) => bail!("unsupported VLESS flow"),
        };
        let mut adapter = meow_proxy::VlessAdapter::new(
            tag,
            &cfg.server,
            cfg.port,
            *id.as_bytes(),
            flow,
            true,
            transport(cfg, base)?,
            dialer,
        );
        let encryption =
            meow_proxy::parse_client_encryption(cfg.encryption.as_deref().unwrap_or("none"))
                .map_err(anyhow::Error::msg)?;
        adapter.set_encryption(encryption.map(Arc::new));
        if let Some(mux) = mux {
            adapter = adapter.with_mux(mux);
        }
        Ok(Arc::new(adapter))
    }
}
pub(crate) use adapter::build;
