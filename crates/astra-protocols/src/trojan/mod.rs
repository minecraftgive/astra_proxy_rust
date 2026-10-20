use crate::outbound::{append_socks_address, tls_connect, Stream};
use anyhow::{bail, Result};
use astra_ipc::Target;
use sha2::{Digest, Sha224};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
pub async fn trojan_tls(
    server: &str,
    sni: &str,
    password: &str,
    target: &Target,
) -> Result<Stream> {
    let mut tls = tls_connect(server, sni).await?;
    let hash = hex::encode(Sha224::digest(password.as_bytes()));
    let header = request(&hash, 1, target)?;
    tls.write_all(&header).await?;
    // Trojan has no successful TCP CONNECT response header.
    Ok(Box::new(tls))
}

use crate::{
    accept::{secret, Request as Route},
    wire,
};
use anyhow::ensure;
pub async fn accept<S: tokio::io::AsyncRead + Unpin>(
    stream: &mut S,
    users: &[crate::accept::User],
) -> Result<crate::accept::Request> {
    let mut hash = [0; 56];
    stream.read_exact(&mut hash).await?;
    ensure!(
        users.iter().any(|u| secret(
            hex::encode(Sha224::digest(u.password.as_ref().unwrap().as_bytes())).as_bytes(),
            &hash
        )),
        "authentication failed"
    );
    wire::crlf(stream).await?;
    let command = stream.read_u8().await?;
    let target = wire::address(stream, 0, false).await?;
    wire::crlf(stream).await?;
    let route = match command {
        1 => Route::Tcp(target),
        3 => Route::TrojanUdp,
        _ => bail!("Trojan command unsupported"),
    };
    Ok(route)
}

use crate::native::NativeOutbound;
pub(crate) async fn trojan_stream(
    outbound: &NativeOutbound,
    target: &astra_ipc::Target,
    command: u8,
) -> Result<Box<dyn meow_transport::Stream>> {
    let NativeOutbound::Trojan {
        server,
        port,
        password_hash,
        transport,
    } = outbound
    else {
        bail!("not a Trojan outbound");
    };
    use tokio::io::AsyncWriteExt;
    let tcp = crate::egress::connect((server.as_str(), *port)).await?;
    tcp.set_nodelay(true)?;
    let mut stream = transport.connect(Box::new(tcp)).await?;
    let header = request(password_hash, command, target)?;
    stream.write_all(&header).await?;
    stream.flush().await?;
    Ok(stream)
}

pub fn request(password_hash: &str, command: u8, target: &Target) -> Result<Vec<u8>> {
    let mut header = password_hash.as_bytes().to_vec();
    header.extend_from_slice(b"\r\n");
    header.push(command);
    append_socks_address(&mut header, target)?;
    header.extend_from_slice(b"\r\n");
    Ok(header)
}
