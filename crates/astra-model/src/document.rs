use serde::{Deserialize, Serialize};
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum PathPart {
    Key(String),
    Index(usize),
}
pub type DocumentPath = Vec<PathPart>;
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Platform {
    Windows,
    Macos,
    Linux,
    Other,
}
impl Platform {
    pub const fn current() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else if cfg!(target_os = "macos") {
            Self::Macos
        } else if cfg!(target_os = "linux") {
            Self::Linux
        } else {
            Self::Other
        }
    }
    pub fn option(self, parent: Option<&str>, key: &str) -> bool {
        match (parent, key) {
            (None, "tproxy" | "redirect") => self == Self::Linux,
            (None, "tun" | "system_proxy") => self != Self::Other,
            (Some("system_proxy"), "services") => self == Self::Macos,
            _ => true,
        }
    }
}
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum FieldType {
    Text,
    Number {
        #[cfg_attr(feature = "bindings", ts(type = "number"))]
        min: i64,
        #[cfg_attr(feature = "bindings", ts(type = "number"))]
        max: i64,
    },
    Choice {
        values: Vec<String>,
    },
    Boolean,
    StringList,
}
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Field {
    pub key: String,
    pub label: String,
    pub field_type: FieldType,
    pub default: String,
    pub value: Option<String>,
    pub required: bool,
    pub secret: bool,
    pub help: String,
}
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SchemaRow {
    Field { field: Field },
    Group { key: String, label: String },
    Entry { index: usize },
}
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Schema {
    pub core: crate::CoreKind,
    pub path: DocumentPath,
    pub collection: bool,
    pub rows: Vec<SchemaRow>,
}
#[cfg_attr(feature = "bindings", derive(ts_rs::TS))]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "command", rename_all = "snake_case", deny_unknown_fields)]
pub enum EditCommand {
    SetField {
        path: DocumentPath,
        key: String,
        input: String,
    },
    RemoveField {
        path: DocumentPath,
        key: String,
    },
    RenameProxy {
        previous: String,
        next: String,
    },
    RenameRuleSet {
        previous: String,
        next: String,
    },
    AppendOutbound {
        kind: String,
        tag: String,
    },
    AppendEntry {
        path: DocumentPath,
    },
    AppendRule {
        kind: String,
        value: String,
        action: String,
    },
    RemoveEntry {
        path: DocumentPath,
        index: usize,
    },
    MoveEntry {
        path: DocumentPath,
        from: usize,
        to: usize,
    },
    ReplaceText {
        text: String,
    },
}

#[cfg(test)]
#[path = "test/document_test.rs"]
mod tests;
