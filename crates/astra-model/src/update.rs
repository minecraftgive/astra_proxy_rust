use serde::{Deserialize, Serialize};
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CurrentRelease {
    pub release_id: String,
    pub previous_release_id: Option<String>,
}
