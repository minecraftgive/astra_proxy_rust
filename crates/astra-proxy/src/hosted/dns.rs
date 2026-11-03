//! Workers own resolver policy/caches; the resident instance owns address identity.
use super::*;
use astra_dns::{Config, MappingStore};
use std::{collections::HashSet, net::IpAddr};

pub use astra_model::host_control::MappingOperation;
impl Host {
    pub(super) async fn mapping(
        &self,
        token: &str,
        operation: MappingOperation,
    ) -> Result<serde_json::Value> {
        self.authenticate(token).await?;
        let store = &self.mappings;
        match operation {
            MappingOperation::Configure { config, reserved } => {
                store.configure(&config, &reserved)?;
                Ok(().into())
            }
            MappingOperation::Allocate {
                config,
                domain,
                ipv6,
                reserved,
            } => Ok(serde_json::to_value(
                store.allocate(&config, &domain, ipv6, &reserved)?,
            )?),
            MappingOperation::Restore { config, ip } => {
                Ok(serde_json::to_value(store.restore(&config, ip)?)?)
            }
            MappingOperation::Observe {
                config,
                domain,
                ip,
                ttl,
            } => {
                store.observe(&config, &domain, ip, ttl)?;
                Ok(().into())
            }
        }
    }
    pub(crate) async fn dns_answer(&self, raw: &[u8]) -> Result<Vec<u8>> {
        let lease = self.lease().await?;
        let duration = Duration::from_millis(
            self.app()?
                .dns
                .as_ref()
                .context("DNS disabled")?
                .config
                .query_timeout_ms
                .saturating_mul(2)
                .saturating_add(1000),
        );
        let result: std::result::Result<Vec<u8>, String> = tokio::time::timeout(duration, async {
            let mut stream = astra_platform::platform::connect_trusted(&lease.0.endpoint).await?;
            send(
                &mut stream,
                Auth {
                    token: lease.0.token.clone(),
                    command: WorkerRequest::Dns(raw.to_vec()),
                },
            )
            .await?;
            receive(&mut stream).await
        })
        .await??;
        result.map_err(anyhow::Error::msg)
    }
    pub(crate) async fn dns_tcp<S: AsyncRead + AsyncWrite + Unpin>(
        &self,
        mut client: S,
    ) -> Result<()> {
        let lease = self.lease().await?;
        let mut stream = astra_platform::platform::connect_trusted(&lease.0.endpoint).await?;
        send(
            &mut stream,
            Auth {
                token: lease.0.token.clone(),
                command: WorkerRequest::DnsTcp,
            },
        )
        .await?;
        let result: std::result::Result<(), String> = receive(&mut stream).await?;
        result.map_err(anyhow::Error::msg)?;
        let mut stream = astra_ipc::host::HostStream::new(stream);
        tokio::io::copy_bidirectional(&mut client, &mut stream).await?;
        Ok(())
    }
}
impl WorkerHost {
    fn mapping<T: serde::de::DeserializeOwned>(&self, operation: MappingOperation) -> Result<T> {
        tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                Ok(serde_json::from_value(
                    request(
                        &self.endpoint,
                        Command::Mapping {
                            token: self.token.clone(),
                            operation: Box::new(operation),
                        },
                    )
                    .await?,
                )?)
            })
        })
    }
}
impl MappingStore for WorkerHost {
    fn configure(&self, config: &Config, reserved: &HashSet<IpAddr>) -> Result<()> {
        self.mapping(MappingOperation::Configure {
            config: config.clone(),
            reserved: reserved.clone(),
        })
    }
    fn allocate(
        &self,
        config: &Config,
        domain: &str,
        ipv6: bool,
        reserved: &HashSet<IpAddr>,
    ) -> Result<Option<IpAddr>> {
        self.mapping(MappingOperation::Allocate {
            config: config.clone(),
            domain: domain.into(),
            ipv6,
            reserved: reserved.clone(),
        })
    }
    fn restore(&self, config: &Config, ip: IpAddr) -> Result<Option<String>> {
        self.mapping(MappingOperation::Restore {
            config: config.clone(),
            ip,
        })
    }
    fn observe(&self, config: &Config, domain: &str, ip: IpAddr, ttl: u32) -> Result<()> {
        self.mapping(MappingOperation::Observe {
            config: config.clone(),
            domain: domain.into(),
            ip,
            ttl,
        })
    }
}
