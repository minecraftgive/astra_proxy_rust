use super::*;
use std::{fmt, net::IpAddr, path::Path};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FakeIpLookupErrorKind {
    Unsupported,
    Communication,
    Protocol,
    Unconfirmed,
    /// A correctly decoded, supported query failed inside the core.
    Core,
    InvalidRequest,
}
#[derive(Debug)]
pub struct FakeIpLookupError {
    pub kind: FakeIpLookupErrorKind,
    pub message: String,
}
impl fmt::Display for FakeIpLookupError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}
impl std::error::Error for FakeIpLookupError {}
fn error(kind: FakeIpLookupErrorKind, message: impl Into<String>) -> FakeIpLookupError {
    FakeIpLookupError {
        kind,
        message: message.into(),
    }
}
fn io_error(value: io::Error) -> FakeIpLookupError {
    error(
        if value.kind() == io::ErrorKind::InvalidData {
            FakeIpLookupErrorKind::Protocol
        } else {
            FakeIpLookupErrorKind::Communication
        },
        value.to_string(),
    )
}

/// Read capabilities afresh from the target core's identity on every call.
/// The request is bound to that boot; no capability cache can outlive a restart.
pub async fn fake_ip_lookup(
    path: &Path,
    addresses: Vec<IpAddr>,
) -> Result<FakeIpLookupResponse, FakeIpLookupError> {
    if addresses.len() > MAX_FAKE_IP_LOOKUP {
        return Err(error(
            FakeIpLookupErrorKind::InvalidRequest,
            "Fake-IP lookup accepts at most 64 addresses",
        ));
    }
    let identity = identity(path).await.map_err(io_error)?;
    match &identity.capabilities {
        None => {
            return Err(error(
                FakeIpLookupErrorKind::Unconfirmed,
                "Core capabilities cannot be confirmed; no Fake-IP query was sent",
            ))
        }
        Some(capabilities) if !capabilities.iter().any(|c| c == FAKE_IP_LOOKUP_CAPABILITY) => {
            return Err(error(
                FakeIpLookupErrorKind::Unsupported,
                "Core explicitly does not support Fake-IP lookup",
            ))
        }
        _ => {}
    }
    let response = rpc(
        path,
        Command::FakeIpLookup {
            expected_boot_id: identity.boot_id.clone(),
            addresses: addresses.clone(),
        },
        4,
    )
    .await
    .map_err(io_error)?;
    validate(response, &identity.boot_id, &addresses)
}
fn validate(
    response: Response,
    boot_id: &str,
    addresses: &[IpAddr],
) -> Result<FakeIpLookupResponse, FakeIpLookupError> {
    let Response::FakeIpLookup(response) = response else {
        return Err(error(
            FakeIpLookupErrorKind::Protocol,
            "Unexpected Fake-IP response type",
        ));
    };
    if response.version != 1 || response.boot_id != boot_id {
        return Err(error(FakeIpLookupErrorKind::Protocol, "Fake-IP response version or core identity changed; read identity again before retrying"));
    }
    match &response.outcome {
        FakeIpLookupOutcome::Unsupported => {
            return Err(error(
                FakeIpLookupErrorKind::Unsupported,
                "Core explicitly rejected Fake-IP capability",
            ))
        }
        FakeIpLookupOutcome::InvalidRequest(message) => {
            return Err(error(
                FakeIpLookupErrorKind::InvalidRequest,
                message.clone(),
            ))
        }
        FakeIpLookupOutcome::CoreChanged => {
            return Err(error(
                FakeIpLookupErrorKind::Protocol,
                "Core changed between capability discovery and query",
            ))
        }
        FakeIpLookupOutcome::Failed(message) => {
            return Err(error(FakeIpLookupErrorKind::Core, message.clone()))
        }
        FakeIpLookupOutcome::Mappings(mappings) => {
            if mappings.len() != addresses.len()
                || mappings.iter().zip(addresses).any(|(m, ip)| {
                    m.address != *ip
                        || m.domain.as_ref().is_some_and(|name| {
                            name.is_empty()
                                || name.len() > 253
                                || name.chars().any(char::is_control)
                        })
                })
            {
                return Err(error(
                    FakeIpLookupErrorKind::Protocol,
                    "Invalid Fake-IP mapping response",
                ));
            }
        }
    }
    Ok(response)
}

#[cfg(test)]
#[path = "../test/control_fake_ip_test.rs"]
mod tests;
