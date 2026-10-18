//! VMess client construction and reusable authenticated server implementation.
pub use meow_proxy::vmess::server::{Accepted, Server};
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
        let security = match cfg.cipher.as_deref().unwrap_or("auto") {
            "auto" => meow_proxy::vmess::header::auto_security(),
            "aes-128-gcm" => meow_proxy::vmess::Security::Aes128Gcm,
            "chacha20-poly1305" => meow_proxy::vmess::Security::ChaCha20Poly1305,
            _ => bail!("unsupported VMess cipher"),
        };
        let mut adapter = meow_proxy::VmessAdapter::new(
            tag,
            &cfg.server,
            cfg.port,
            *id.as_bytes(),
            security,
            true,
            transport(cfg, base)?,
            dialer,
        );
        if let Some(mux) = mux {
            adapter = adapter.with_mux(mux);
        }
        Ok(Arc::new(adapter))
    }
}
pub(crate) use adapter::build;
