use super::*;

/// The legacy binary layout is frozen. Capability extensions are carried only
/// in human-readable JSON, or the explicitly framed Identity transport trailer.
#[derive(Serialize, Deserialize)]
struct Legacy {
    version: u16,
    boot_id: String,
    process: ProcessIdentity,
    applied: Option<Applied>,
}
#[derive(Serialize, Deserialize)]
struct Json {
    version: u16,
    boot_id: String,
    process: ProcessIdentity,
    applied: Option<Applied>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    capabilities: Option<Vec<String>>,
}
impl Serialize for Identity {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        if serializer.is_human_readable() {
            Json {
                version: self.version,
                boot_id: self.boot_id.clone(),
                process: self.process.clone(),
                applied: self.applied.clone(),
                capabilities: self.capabilities.clone(),
            }
            .serialize(serializer)
        } else {
            Legacy {
                version: self.version,
                boot_id: self.boot_id.clone(),
                process: self.process.clone(),
                applied: self.applied.clone(),
            }
            .serialize(serializer)
        }
    }
}
impl<'de> Deserialize<'de> for Identity {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        if deserializer.is_human_readable() {
            let value = Json::deserialize(deserializer)?;
            Ok(Self {
                version: value.version,
                boot_id: value.boot_id,
                process: value.process,
                applied: value.applied,
                capabilities: value.capabilities,
            })
        } else {
            let value = Legacy::deserialize(deserializer)?;
            Ok(Self {
                version: value.version,
                boot_id: value.boot_id,
                process: value.process,
                applied: value.applied,
                capabilities: None,
            })
        }
    }
}
