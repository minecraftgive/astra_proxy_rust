//! The v2 boundary keeps 64-bit update counters lossless without changing persisted journals.
use serde::{Deserialize, Deserializer, Serializer};
use serde_json::Value;
fn counter(key: &str) -> bool {
    matches!(
        key,
        "previous_generation"
            | "candidate_generation"
            | "known_generations"
            | "updated_at_ms"
            | "observed_at_ms"
            | "generation"
            | "active"
            | "flows"
    )
}
fn transform(value: Value, key: &str, map: bool, encode: bool) -> Value {
    match value {
        Value::Number(n) if encode && (map || counter(key)) => Value::String(n.to_string()),
        Value::String(s) if !encode && (map || counter(key)) => s
            .parse::<u64>()
            .map(|n| Value::Number(n.into()))
            .unwrap_or(Value::String(s)),
        Value::Array(values) => Value::Array(
            values
                .into_iter()
                .map(|v| transform(v, key, map, encode))
                .collect(),
        ),
        Value::Object(values) => Value::Object(
            values
                .into_iter()
                .map(|(k, v)| {
                    let all = matches!(key, "source_rule_revisions" | "effective_rule_revisions");
                    let v = transform(v, &k, all, encode);
                    (k, v)
                })
                .collect(),
        ),
        other => other,
    }
}
pub fn serialize<S: Serializer>(
    value: &crate::updates::UpdateStatus,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    use serde::Serialize;
    transform(
        serde_json::to_value(value).map_err(serde::ser::Error::custom)?,
        "",
        false,
        true,
    )
    .serialize(serializer)
}
pub fn deserialize<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Box<crate::updates::UpdateStatus>, D::Error> {
    serde_json::from_value(transform(
        Value::deserialize(deserializer)?,
        "",
        false,
        false,
    ))
    .map_err(serde::de::Error::custom)
}
#[cfg(test)]
mod tests {
    #[test]
    fn v2_preserves_large_revisions_and_normal_version_numbers() {
        let mut status = crate::updates::UpdateStatus {
            observed_at_ms: u64::MAX,
            ..Default::default()
        };
        status
            .source_rule_revisions
            .insert("rules".into(), u64::MAX);
        let response = crate::v2::ResponseBody::UpdateStatus(Box::new(status));
        let json = serde_json::to_value(&response).unwrap();
        assert_eq!(json["data"]["observed_at_ms"], u64::MAX.to_string());
        assert_eq!(
            json["data"]["source_rule_revisions"]["rules"],
            u64::MAX.to_string()
        );
        assert_eq!(json["data"]["schema_version"], 1);
        let crate::v2::ResponseBody::UpdateStatus(decoded) = serde_json::from_value(json).unwrap()
        else {
            panic!()
        };
        assert_eq!(decoded.observed_at_ms, u64::MAX);
        assert_eq!(decoded.source_rule_revisions["rules"], u64::MAX);
    }
}
