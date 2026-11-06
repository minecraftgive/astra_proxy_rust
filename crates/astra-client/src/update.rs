use crate::protocol::updates::UpdateStatus;
use anyhow::Result;
pub struct View {
    pub status: UpdateStatus,
}
impl View {
    pub fn new(status: UpdateStatus) -> Self {
        Self { status }
    }
    pub fn decode(text: &str) -> Result<Self> {
        let value: serde_json::Value = serde_json::from_str(text)?;
        anyhow::ensure!(
            value
                .get("schema_version")
                .and_then(|version| version.as_u64())
                == Some(1),
            "This manager does not support the update transaction view"
        );
        Ok(Self::new(serde_json::from_value(value)?))
    }
}
