use astra_management::v2::{SaveOutcome, SaveState};
#[derive(Debug)]
pub(crate) struct SaveFailure {
    pub outcome: SaveOutcome,
}
impl std::fmt::Display for SaveFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.outcome.message)
    }
}
impl std::error::Error for SaveFailure {}
pub(crate) fn outcome(
    state: SaveState,
    disk_saved: Option<bool>,
    runtime_applied: Option<bool>,
    message: impl Into<String>,
) -> SaveOutcome {
    SaveOutcome {
        state,
        disk_saved,
        runtime_applied,
        recovery: None,
        operation_id: String::new(),
        message: message.into(),
    }
}
pub(crate) fn failed(
    state: SaveState,
    disk_saved: Option<bool>,
    runtime_applied: Option<bool>,
    message: impl Into<String>,
) -> anyhow::Error {
    SaveFailure {
        outcome: outcome(state, disk_saved, runtime_applied, message),
    }
    .into()
}
