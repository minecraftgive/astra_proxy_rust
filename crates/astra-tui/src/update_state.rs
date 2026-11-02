//! Versioned JSON carried inside the existing management Reply, never bincode fields.
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, path::PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UpdateIntent {
    Stage {
        package: PathBuf,
    },
    Activate {
        release_id: String,
    },
    /// The caller freezes this directory from a status snapshot before submitting.
    Rollback {
        target_directory: PathBuf,
    },
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdateRequest {
    pub operation_id: String,
    pub intent: UpdateIntent,
}
impl UpdateRequest {
    pub fn new(intent: UpdateIntent) -> Self {
        static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        Self {
            operation_id: format!(
                "{}-{}-{}",
                std::process::id(),
                astra_ipc::progress::now_ms(),
                SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ),
            intent,
        }
    }
}
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum UpdatePhase {
    #[default]
    Validating,
    Staged,
    Preparing,
    Activating,
    Resuming,
    SavingSelection,
    Complete,
    Failed,
    NeedsRecovery,
    Uncertain,
    RestoringPrevious,
}
impl UpdatePhase {
    pub fn terminal(self) -> bool {
        matches!(self, Self::Staged | Self::Complete | Self::Failed)
    }
    pub fn label(self) -> &'static str {
        match self {
            Self::Validating => "Validating package",
            Self::Staged => "Staged; running version unchanged",
            Self::Preparing => "Preparing workers",
            Self::Activating => "Switching new connections",
            Self::Resuming => "Resuming ingress",
            Self::SavingSelection => "Saving next-launch version",
            Self::Complete => "New connections switched; old connections drain naturally",
            Self::Failed => "Not applied",
            Self::NeedsRecovery => "Partly applied; recovery required",
            Self::Uncertain => "Runtime state must be reconciled",
            Self::RestoringPrevious => "Restoring previous workers",
        }
    }
}
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdateParticipant {
    pub endpoint: PathBuf,
    pub host_boot_id: String,
    pub previous_generation: u64,
    pub candidate_generation: u64,
    pub resume_needed: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdatePreparation {
    pub endpoint: PathBuf,
    pub host_boot_id: String,
    pub previous_generation: u64,
    pub known_generations: Vec<u64>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpdateOperation {
    pub request: UpdateRequest,
    pub phase: UpdatePhase,
    pub before_directory: PathBuf,
    pub target_directory: Option<PathBuf>,
    pub release_id: Option<String>,
    pub runtime_committed: bool,
    pub next_launch_saved: bool,
    pub pending_record_saved: bool,
    pub participants: Vec<UpdateParticipant>,
    /// A Prepare response was not durably recorded; never guess its generation ID.
    #[serde(default)]
    pub uncertain_preparations: Vec<UpdatePreparation>,
    pub error: Option<String>,
    pub updated_at_ms: u64,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdateCapabilities {
    pub experimental_enabled: bool,
    pub ordinary_user: bool,
    pub operation_running: bool,
    pub activation_allowed: bool,
    pub activation_blocked_reason: Option<String>,
    pub rollback_target: Option<PathBuf>,
    pub recovery_operation_id: Option<String>,
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdateInstance {
    pub pid: Option<u32>,
    pub reachable: bool,
    pub status: Option<serde_json::Value>,
    pub error: Option<String>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdateStatus {
    pub schema_version: u16,
    pub host_version: String,
    pub manager_binary: Option<PathBuf>,
    pub runtime_directory: PathBuf,
    pub next_launch: Option<astra_update::CurrentRelease>,
    pub staged_release_id: Option<String>,
    pub staged_compatible: Option<bool>,
    pub capabilities: UpdateCapabilities,
    pub operation: Option<UpdateOperation>,
    pub instances: BTreeMap<String, UpdateInstance>,
    pub observed_at_ms: u64,
    pub rule_sync_error: Option<String>,
    pub source_rule_revisions: BTreeMap<String, u64>,
    /// Source revisions incorporated into the active worker generation.
    pub effective_rule_revisions: BTreeMap<String, u64>,
}
impl Default for UpdateStatus {
    fn default() -> Self {
        Self {
            schema_version: 1,
            host_version: env!("CARGO_PKG_VERSION").into(),
            manager_binary: std::env::current_exe().ok(),
            runtime_directory: Default::default(),
            next_launch: None,
            staged_release_id: None,
            staged_compatible: None,
            capabilities: Default::default(),
            operation: None,
            instances: Default::default(),
            observed_at_ms: 0,
            rule_sync_error: None,
            source_rule_revisions: Default::default(),
            effective_rule_revisions: Default::default(),
        }
    }
}
