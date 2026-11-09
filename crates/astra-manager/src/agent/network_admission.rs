use anyhow::{ensure, Result};
use astra_management::v2::ErrorCode;
use astra_model::network_state::{Interface, InterfaceKind, Snapshot};

pub(super) fn physical_interface(
    request: &astra_model::network_nat::Request,
    snapshot: &Snapshot,
    agent_id: &str,
) -> Result<Interface> {
    ensure!(
        request.agent_id == agent_id && snapshot.agent_id == agent_id,
        crate::error::message(
            ErrorCode::InvalidRequest,
            "NAT interface belongs to a different Agent"
        )
    );
    ensure!(
        !request.interface_id.is_empty(),
        crate::error::message(ErrorCode::InvalidRequest, "请选择物理网卡")
    );
    ensure!(
        !snapshot.truncated,
        crate::error::message(
            ErrorCode::InvalidRequest,
            "Cannot confirm physical interface from truncated network state"
        )
    );
    let interface = snapshot
        .interfaces
        .iter()
        .find(|interface| interface.id == request.interface_id)
        .ok_or_else(|| {
            crate::error::message(
                ErrorCode::InvalidRequest,
                "Selected NAT interface no longer exists",
            )
        })?;
    ensure!(
        interface.connected && interface.kind == InterfaceKind::Physical,
        crate::error::message(
            ErrorCode::InvalidRequest,
            "NAT requires a connected, positively identified physical interface"
        )
    );
    ensure!(
        interface
            .addresses
            .iter()
            .filter_map(|address| address.address.parse().ok())
            .any(|address| request.family.matches(address)),
        crate::error::message(
            ErrorCode::InvalidRequest,
            "Selected NAT interface does not support the requested address family"
        )
    );
    Ok(interface.clone())
}

pub(super) fn same_interface(before: &Interface, after: &Interface) -> bool {
    before.id == after.id
        && before.name == after.name
        && before.index == after.index
        && before.kind == after.kind
        && before.connected == after.connected
        && before.addresses == after.addresses
        && before.source == after.source
}
