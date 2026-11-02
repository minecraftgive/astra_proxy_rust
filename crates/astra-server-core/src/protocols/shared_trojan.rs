use crate::net::{App, Hub, Stream};
use anyhow::Result;
use astra_protocols::wire;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
pub async fn trojan_udp(stream: Stream, app: Arc<App>) -> Result<()> {
    let mut hub = Hub::new(app.clone());
    let (mut rd, mut wr) = tokio::io::split(stream);
    let send = hub.send.clone();
    let input = async {
        loop {
            let target = wire::address(&mut rd, 0, false).await?;
            let n = rd.read_u16().await? as usize;
            wire::crlf(&mut rd).await?;
            let mut payload = vec![0; n];
            rd.read_exact(&mut payload).await?;
            send.send(app.packet(0, target, payload)?).await?;
        }
        #[allow(unreachable_code)]
        Ok::<(), anyhow::Error>(())
    };
    let output = async {
        while let Some(p) = hub.receive.recv().await {
            wr.write_all(&wire::encode(&p.target, 0)?).await?;
            wr.write_u16(p.payload.len() as u16).await?;
            wr.write_all(b"\r\n").await?;
            wr.write_all(&p.payload).await?;
            wr.flush().await?;
        }
        Ok::<(), anyhow::Error>(())
    };
    tokio::select! {r=input=>r,r=output=>r}
}
