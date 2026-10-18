mod adapter {
    use crate::native::*;
    use anyhow::{bail, Result};
    use meow_common::ProxyAdapter;
    use std::{path::Path, sync::Arc};
    pub fn build(
        tag: &str,
        cfg: &NodeConfig,
        _base: &Path,
        _dialer: Arc<meow_proxy::dialer::DirectDialer>,
        mux: Option<meow_proxy::mux::MuxOptions>,
    ) -> Result<Arc<dyn ProxyAdapter>> {
        if cfg.transport.kind != TransportKind::Tcp || mux.is_some() {
            bail!("Hysteria2 uses its own QUIC transport");
        }
        if cfg.tls.as_ref().is_some_and(|t| {
            t.ca_file.is_some() || t.reality_public_key.is_some() || t.cert_pin_sha256.is_some()
        }) {
            bail!("Hysteria2 uses system roots or pin_sha256, not ca_file/REALITY");
        }
        let options = meow_proxy::Hy2Options {
            name: tag.into(),
            server: cfg.server.clone(),
            port: cfg.port,
            password: required(&cfg.password, "password")?.into(),
            sni: cfg.tls.as_ref().map(|t| t.server_name.clone()),
            skip_cert_verify: false,
            udp: true,
            up_bps: 0,
            down_bps: 0,
            obfs: cfg
                .obfs_password
                .as_ref()
                .map(|_| meow_proxy::Hy2Obfs::Salamander),
            obfs_password: cfg.obfs_password.clone(),
            ports: None,
            hop_interval: None,
            fingerprint: cfg.pin_sha256.clone(),
            fast_open: false,
        };
        Ok(Arc::new(
            meow_proxy::Hy2Adapter::new(options).map_err(anyhow::Error::msg)?,
        ))
    }
}
pub(crate) use adapter::build;

use crate::{accept::secret, wire};
use anyhow::{ensure, Result};
pub async fn authenticate(
    conn: &quinn::Connection,
    users: &[crate::accept::User],
    udp: bool,
) -> Result<quinn::SendStream> {
    use quiche::h3::{qpack, Header, NameValue};
    let (mut send, mut recv) = conn.accept_bi().await?;
    ensure!(
        wire::varint(&mut recv).await? == 1,
        "Hysteria HTTP/3 headers required"
    );
    let block = wire::sized(&mut recv, 16384).await?;
    ensure!(block.starts_with(&[0, 0]), "dynamic QPACK is unsupported");
    let headers = qpack::Decoder::new()
        .decode(&block, 16384)
        .map_err(|e| anyhow::anyhow!("QPACK: {e}"))?;
    let values = |key: &[u8]| {
        headers
            .iter()
            .filter(move |h| h.name() == key)
            .map(|h| h.value())
            .collect::<Vec<_>>()
    };
    ensure!(
        values(b":method") == vec![b"POST".as_slice()]
            && values(b":path") == vec![b"/auth".as_slice()],
        "Hysteria auth request"
    );
    let passwords = values(b"hysteria-auth");
    ensure!(
        passwords.len() == 1
            && users
                .iter()
                .any(|u| secret(u.password.as_ref().unwrap().as_bytes(), passwords[0])),
        "Hysteria authentication failed"
    );
    let headers = [
        Header::new(b":status", b"233"),
        Header::new(b"hysteria-udp", if udp { b"true" } else { b"false" }),
        Header::new(b"hysteria-cc-rx", b"0"),
    ];
    let mut block = vec![0; 1024];
    let n = qpack::Encoder::new()
        .encode(&headers, &mut block)
        .map_err(|e| anyhow::anyhow!("QPACK: {e}"))?;
    let mut reply = vec![1];
    wire::put_varint(n as u64, &mut reply)?;
    reply.extend(&block[..n]);
    send.write_all(&reply).await?;
    send.finish()?;
    let mut stream = conn.open_uni().await?;
    stream.write_all(&[0, 4, 0]).await?;
    return Ok(stream);
}

pub async fn accept_tcp<R: tokio::io::AsyncRead + Unpin>(
    recv: &mut R,
) -> Result<astra_ipc::Target> {
    use crate::http::authority_target;

    ensure!(wire::varint(recv).await? == 0x401, "Hysteria TCP request");
    let address = wire::sized(recv, 512).await?;
    let _padding = wire::sized(recv, 4096).await?;
    authority_target(std::str::from_utf8(&address)?)
}

pub(crate) async fn decode_datagram(
    bytes: &[u8],
    fragments: &mut crate::datagram::Fragments,
) -> Result<Option<(u32, astra_ipc::Target, Vec<u8>)>> {
    use crate::{http::authority_target, wire};
    use tokio::io::AsyncReadExt;
    let mut r = bytes;

    let id = r.read_u32().await?;
    let packet = r.read_u16().await?;
    let index = r.read_u8().await?;
    let count = r.read_u8().await?;
    let address = wire::sized(&mut r, 512).await?;
    let target = authority_target(std::str::from_utf8(&address)?)?;
    fragments.insert(id, packet, count, index, Some(target), r)
}
