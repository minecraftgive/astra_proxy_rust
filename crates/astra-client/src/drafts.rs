use crate::{editor::Editor, protocol::Kind};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
pub const KEYMAP_VERSION: u32 = 2;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Draft {
    pub path: PathBuf,
    pub kind: Kind,
    pub editor: Editor,
    #[serde(default)]
    pub conflict: bool,
    /// Disk contents observed on restore, kept only when an external change conflicts.
    #[serde(default)]
    pub observed_disk: Option<String>,
}
