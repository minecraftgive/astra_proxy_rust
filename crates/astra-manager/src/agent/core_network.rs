//! Read-only core observations. Capability absence and protocol failure never collapse together.
use super::*;

type Failure = (CoreQueryStatus, String);
enum TargetFailure {
    Observation(Failure),
    Internal(anyhow::Error),
}
impl From<Failure> for TargetFailure {
    fn from(failure: Failure) -> Self {
        Self::Observation(failure)
    }
}
fn io_failure(error: std::io::Error) -> Failure {
    (
        if error.kind() == std::io::ErrorKind::InvalidData {
            CoreQueryStatus::ProtocolError
        } else {
            CoreQueryStatus::CommunicationError
        },
        error.to_string(),
    )
}
fn lookup_failure(error: astra_ipc::control::FakeIpLookupError) -> Failure {
    use astra_ipc::control::FakeIpLookupErrorKind as E;
    (
        match error.kind {
            E::Unsupported => CoreQueryStatus::Unsupported,
            E::Unconfirmed => CoreQueryStatus::Unconfirmed,
            E::Communication => CoreQueryStatus::CommunicationError,
            E::Protocol => CoreQueryStatus::ProtocolError,
            E::Core => CoreQueryStatus::CoreError,
            E::InvalidRequest => CoreQueryStatus::InvalidRequest,
        },
        error.message,
    )
}

impl AgentService {
    async fn network_core_target(&self, instance: &str) -> Result<(PathBuf, u32), TargetFailure> {
        let instances = self.instances().await.map_err(|error| {
            if error
                .chain()
                .any(|cause| cause.is::<crate::error::AgentError>())
            {
                TargetFailure::Internal(error)
            } else {
                TargetFailure::Observation((
                    CoreQueryStatus::CommunicationError,
                    format!("Read target instance: {error:#}"),
                ))
            }
        })?;
        let row = instances
            .into_iter()
            .find(|row| row.name == instance || row.id == instance)
            .ok_or_else(|| {
                (
                    CoreQueryStatus::InvalidRequest,
                    "Target instance not found".into(),
                )
            })?;
        if row.kind != InstanceKind::Proxy {
            return Err((
                CoreQueryStatus::Unsupported,
                "Network core observation requires a proxy instance".into(),
            )
                .into());
        }
        let pid = row.pid.ok_or_else(|| {
            (
                CoreQueryStatus::CoreError,
                "Target proxy instance is not running".into(),
            )
        })?;
        Ok((
            self.config.state_dir.join(format!("{}.sock", row.name)),
            pid,
        ))
    }

    pub(super) async fn network_core(&self, instance: String) -> Result<NetworkCoreReply> {
        let mut reply = NetworkCoreReply {
            schema_version: 1,
            instance: instance.clone(),
            sampled_at_ms: store::now(),
            status: CoreQueryStatus::Ok,
            message: None,
            identity: None,
        };
        let result = async {
            let (endpoint, pid) = self.network_core_target(&instance).await?;
            let identity = astra_ipc::control::identity(&endpoint)
                .await
                .map_err(io_failure)?;
            if identity.process.pid != pid {
                return Err((
                    CoreQueryStatus::ProtocolError,
                    "Managed proxy process changed during identity observation; refresh instances"
                        .into(),
                )
                    .into());
            }
            Ok(identity)
        }
        .await;
        match result {
            Ok(identity) => {
                if identity.capabilities.is_none() {
                    reply.status = CoreQueryStatus::Unconfirmed;
                    reply.message = Some(
                        "Core capabilities cannot be confirmed; no feature query was sent".into(),
                    );
                }
                reply.identity = Some(NetworkCoreIdentity {
                    version: identity.version,
                    boot_id: identity.boot_id,
                    pid: identity.process.pid,
                    capabilities: identity.capabilities,
                    applied_digest: identity.applied.map(|applied| applied.digest.to_string()),
                });
            }
            Err(TargetFailure::Observation((status, message))) => {
                reply.status = status;
                reply.message = Some(message);
            }
            Err(TargetFailure::Internal(error)) => return Err(error),
        }
        Ok(reply)
    }

    pub(super) async fn fake_ip_lookup(
        &self,
        instance: String,
        addresses: Vec<std::net::IpAddr>,
    ) -> Result<FakeIpLookupReply> {
        let mut reply = FakeIpLookupReply {
            schema_version: 1,
            instance: instance.clone(),
            sampled_at_ms: store::now(),
            status: CoreQueryStatus::Ok,
            message: None,
            boot_id: None,
            mappings: Vec::new(),
        };
        let result = async {
            if addresses.len() > astra_model::control::MAX_FAKE_IP_LOOKUP {
                return Err((
                    CoreQueryStatus::InvalidRequest,
                    "Fake-IP lookup accepts at most 64 addresses".into(),
                )
                    .into());
            }
            let (endpoint, _) = self.network_core_target(&instance).await?;
            astra_ipc::control::fake_ip_lookup(&endpoint, addresses)
                .await
                .map_err(|error| TargetFailure::Observation(lookup_failure(error)))
        }
        .await;
        match result {
            Ok(result) => {
                reply.boot_id = Some(result.boot_id);
                if let astra_model::control::FakeIpLookupOutcome::Mappings(mappings) =
                    result.outcome
                {
                    reply.mappings = mappings
                        .into_iter()
                        .map(|mapping| FakeIpMapping {
                            address: mapping.address.to_string(),
                            domain: mapping.domain,
                        })
                        .collect();
                } else {
                    reply.status = CoreQueryStatus::ProtocolError;
                    reply.message = Some("Unexpected decoded Fake-IP outcome".into());
                }
            }
            Err(TargetFailure::Observation((status, message))) => {
                reply.status = status;
                reply.message = Some(message);
            }
            Err(TargetFailure::Internal(error)) => return Err(error),
        }
        Ok(reply)
    }
}

#[cfg(test)]
#[path = "../test/network_core_bridge_test.rs"]
mod tests;
