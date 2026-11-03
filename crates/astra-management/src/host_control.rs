//! Resident-host management client. The model owns the wire data contract.
pub use astra_model::host_control::{
    endpoint, Command, GenerationStatus, MappingOperation, Status,
};
pub async fn request(
    path: &std::path::Path,
    command: Command,
) -> anyhow::Result<serde_json::Value> {
    astra_ipc::host::request_json_control(path, command).await
}
