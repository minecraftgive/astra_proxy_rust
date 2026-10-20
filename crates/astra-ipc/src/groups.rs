//! Proxy-group configuration shared by the converter, editor and core.
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    #[default]
    Select,
    #[serde(alias = "url-test", alias = "url_test")]
    Auto,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Group {
    pub mode: Mode,
    pub members: Vec<String>,
    pub include_all: bool,
    pub include_all_groups: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub filter: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclude_filter: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub selected: Option<String>,
    pub url: String,
    pub interval: u64,
    pub timeout_ms: u64,
    pub tolerance_ms: u64,
    pub disconnect_on_manual: bool,
    pub disconnect_on_auto: bool,
    pub disconnect_on_failure: bool,
    pub disconnect_on_recovery: bool,
    pub disconnect_on_config_change: bool,
}
impl Default for Group {
    fn default() -> Self {
        Self {
            mode: Mode::Select,
            members: vec![],
            include_all: false,
            include_all_groups: false,
            filter: None,
            exclude_filter: None,
            selected: None,
            url: "https://www.gstatic.com/generate_204".into(),
            interval: 300,
            timeout_ms: 5000,
            tolerance_ms: 50,
            disconnect_on_manual: false,
            disconnect_on_auto: false,
            disconnect_on_failure: false,
            disconnect_on_recovery: false,
            disconnect_on_config_change: false,
        }
    }
}
pub fn builtin(name: &str) -> bool {
    matches!(name, "DIRECT" | "REJECT")
}
pub fn ensure_default(
    groups: &mut BTreeMap<String, Group>,
    nodes: &HashSet<String>,
) -> Result<(), String> {
    if nodes.contains("PROXY") {
        return Err(
            "PROXY is reserved for the default proxy group; rename the outbound node".into(),
        );
    }
    groups.entry("PROXY".into()).or_insert_with(|| Group {
        include_all: true,
        ..Default::default()
    });
    Ok(())
}
/// Resolve dynamic membership afresh from saved node/group definitions, without
/// serializing the expanded snapshot back into the user's configuration.
pub fn expand(
    groups: &BTreeMap<String, Group>,
    nodes: &HashSet<String>,
) -> Result<BTreeMap<String, Group>, String> {
    fn pattern(value: &Option<String>) -> Result<Option<regex::Regex>, String> {
        value
            .as_ref()
            .map(|value| {
                if value.len() > 4096 {
                    return Err("group filter exceeds 4096 bytes".into());
                }
                regex::RegexBuilder::new(value)
                    .size_limit(2 * 1024 * 1024)
                    .build()
                    .map_err(|e| format!("invalid group filter: {e}"))
            })
            .transpose()
    }
    let mut result = groups.clone();
    for (name, source) in groups {
        let filter = pattern(&source.filter).map_err(|e| format!("proxy group {name}: {e}"))?;
        let exclude =
            pattern(&source.exclude_filter).map_err(|e| format!("proxy group {name}: {e}"))?;
        let mut seen = HashSet::new();
        for member in &source.members {
            if !seen.insert(member.clone()) {
                return Err(format!("proxy group {name}: duplicate member {member}"));
            }
        }
        let mut dynamic: Vec<_> = nodes
            .iter()
            .filter(|_| source.include_all)
            .chain(
                groups
                    .keys()
                    .filter(|g| source.include_all_groups && *g != name),
            )
            .filter(|m| {
                filter.as_ref().is_none_or(|r| r.is_match(m))
                    && !exclude.as_ref().is_some_and(|r| r.is_match(m))
            })
            .cloned()
            .collect();
        dynamic.sort();
        let target = result.get_mut(name).unwrap();
        target
            .members
            .extend(dynamic.into_iter().filter(|m| seen.insert(m.clone())));
    }
    Ok(result)
}
/// Child groups precede their parents; cycles and ambiguous references are rejected.
pub fn validate(
    groups: &BTreeMap<String, Group>,
    nodes: &HashSet<String>,
) -> Result<Vec<String>, String> {
    let groups = expand(groups, nodes)?;
    validate_expanded(&groups, nodes)
}
fn validate_expanded(
    groups: &BTreeMap<String, Group>,
    nodes: &HashSet<String>,
) -> Result<Vec<String>, String> {
    if groups.len() > 128 {
        return Err("at most 128 proxy groups are supported".into());
    }
    let mut total = 0;
    for (name, group) in groups {
        if name.trim().is_empty()
            || name.len() > 64
            || name.chars().any(char::is_control)
            || nodes.contains(name)
            || builtin(name)
        {
            return Err(format!(
                "invalid, reserved or duplicate proxy group name: {name}"
            ));
        }
        total += group.members.len();
        if (group.members.is_empty() && !group.include_all && !group.include_all_groups)
            || group.members.len() > 1024
            || total > 4096
        {
            return Err(format!(
                "proxy group {name}: expected 1..1024 members, at most 4096 in total"
            ));
        }
        let mut seen = HashSet::new();
        for member in &group.members {
            if !seen.insert(member)
                || (!builtin(member) && !nodes.contains(member) && !groups.contains_key(member))
            {
                return Err(format!(
                    "proxy group {name}: duplicate or undefined member {member}"
                ));
            }
        }
        if group
            .selected
            .as_ref()
            .is_some_and(|s| !group.members.contains(s))
        {
            return Err(format!(
                "proxy group {name}: selected must be one of its members"
            ));
        }
        if !(1..=86400).contains(&group.interval)
            || !(100..=30000).contains(&group.timeout_ms)
            || group.tolerance_ms > 10000
        {
            return Err(format!(
                "proxy group {name}: invalid interval, timeout or tolerance"
            ));
        }
        if group.mode == Mode::Auto
            && !(group.url.starts_with("http://") || group.url.starts_with("https://"))
        {
            return Err(format!("proxy group {name}: test URL must use HTTP(S)"));
        }
    }
    fn visit(
        name: &str,
        groups: &BTreeMap<String, Group>,
        active: &mut HashSet<String>,
        done: &mut HashSet<String>,
        order: &mut Vec<String>,
    ) -> Result<(), String> {
        if done.contains(name) {
            return Ok(());
        }
        if active.len() >= 32 || !active.insert(name.into()) {
            return Err(format!("proxy group cycle or excessive nesting: {name}"));
        }
        for member in &groups[name].members {
            if groups.contains_key(member) {
                visit(member, groups, active, done, order)?;
            }
        }
        active.remove(name);
        done.insert(name.into());
        order.push(name.into());
        Ok(())
    }
    let mut order = vec![];
    let mut done = HashSet::new();
    for name in groups.keys() {
        visit(name, groups, &mut HashSet::new(), &mut done, &mut order)?;
    }
    Ok(order)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dynamic_members_merge_filter_deduplicate_and_recompute_without_materializing() {
        let mut groups = BTreeMap::from([(
            "region".into(),
            Group {
                members: vec!["manual".into(), "JP-a".into()],
                include_all: true,
                filter: Some("^JP-".into()),
                exclude_filter: Some("expired".into()),
                ..Default::default()
            },
        )]);
        let nodes = HashSet::from([
            "manual".into(),
            "JP-a".into(),
            "JP-b".into(),
            "JP-expired".into(),
            "US-c".into(),
        ]);
        ensure_default(&mut groups, &nodes).unwrap();
        let expanded = expand(&groups, &nodes).unwrap();
        assert_eq!(expanded["region"].members, ["manual", "JP-a", "JP-b"]);
        assert_eq!(
            groups["region"].members,
            ["manual", "JP-a"],
            "do not persist expansion"
        );
        let mut updated = nodes.clone();
        updated.remove("JP-b");
        updated.insert("JP-new".into());
        let expanded = expand(&groups, &updated).unwrap();
        assert_eq!(expanded["region"].members, ["manual", "JP-a", "JP-new"]);
        assert!(validate(&groups, &updated).is_ok());
        groups.get_mut("region").unwrap().filter = Some("[".into());
        assert!(validate(&groups, &nodes).is_err());
    }
    #[test]
    fn dynamic_group_references_detect_cycles_and_empty_results_are_safe() {
        let nodes = HashSet::new();
        let mut groups = BTreeMap::new();
        ensure_default(&mut groups, &nodes).unwrap();
        groups.insert(
            "all-groups".into(),
            Group {
                include_all_groups: true,
                ..Default::default()
            },
        );
        assert_eq!(
            expand(&groups, &nodes).unwrap()["all-groups"].members,
            ["PROXY"]
        );
        assert!(validate(&groups, &nodes).is_ok());
        groups.insert(
            "cycle".into(),
            Group {
                members: vec!["all-groups".into()],
                ..Default::default()
            },
        );
        assert!(validate(&groups, &nodes).is_err());
        assert!(ensure_default(&mut BTreeMap::new(), &HashSet::from(["PROXY".into()])).is_err());
    }
    #[test]
    fn connection_policies_preserve_backward_compatible_defaults() {
        let group = Group::default();
        assert!(
            !group.disconnect_on_manual
                && !group.disconnect_on_auto
                && !group.disconnect_on_failure
                && !group.disconnect_on_recovery
                && !group.disconnect_on_config_change
        );
    }
    #[test]
    fn references_cycles_and_selection_are_validated() {
        let nodes = HashSet::from(["a".into(), "b".into()]);
        let mut groups = BTreeMap::from([
            (
                "auto".into(),
                Group {
                    mode: Mode::Auto,
                    members: vec!["a".into(), "b".into()],
                    ..Default::default()
                },
            ),
            (
                "main".into(),
                Group {
                    members: vec!["auto".into(), "DIRECT".into()],
                    selected: Some("auto".into()),
                    ..Default::default()
                },
            ),
        ]);
        assert_eq!(validate(&groups, &nodes).unwrap(), ["auto", "main"]);
        groups.get_mut("auto").unwrap().members.push("main".into());
        assert!(validate(&groups, &nodes).is_err());
        groups.get_mut("auto").unwrap().members = vec!["missing".into()];
        assert!(validate(&groups, &nodes).is_err());
        groups.get_mut("auto").unwrap().members = vec!["a".into()];
        groups.get_mut("main").unwrap().selected = Some("b".into());
        assert!(validate(&groups, &nodes).is_err());
    }
}
