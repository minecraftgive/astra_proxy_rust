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
    Fallback,
    #[serde(alias = "load-balance")]
    LoadBalance,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Group {
    /// Explicit per-group socket binding; absent means inherit the outer/default egress.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interface: Option<String>,
    /// Subscription source whose nodes form this group's managed membership.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subscription: Option<String>,
    pub include_in_proxy: bool,
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
            interface: None,
            subscription: None,
            include_in_proxy: true,
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
/// DIRECT and REJECT are both terminal outbounds and built-in groups of the same name.
pub fn builtin(name: &str) -> bool {
    matches!(name, "DIRECT" | "REJECT")
}
/// Groups every configuration has; they cannot be renamed or removed.
pub fn reserved(name: &str) -> bool {
    name == "PROXY" || builtin(name)
}
/// Whether `member` of `parent` leads into another group. Inside its own built-in
/// group, DIRECT/REJECT names the terminal outbound; everywhere else, the group.
pub fn group_edge(groups: &BTreeMap<String, Group>, parent: &str, member: &str) -> bool {
    member != parent && groups.contains_key(member)
}
pub fn ensure_default(
    groups: &mut BTreeMap<String, Group>,
    nodes: &HashSet<String>,
) -> Result<(), String> {
    if let Some(name) = nodes.iter().find(|name| reserved(name)) {
        return Err(format!(
            "{name} is reserved for a built-in proxy group; rename the outbound node"
        ));
    }
    groups.entry("PROXY".into()).or_insert_with(|| Group {
        include_all: true,
        ..Default::default()
    });
    for name in ["DIRECT", "REJECT"] {
        groups.entry(name.into()).or_insert_with(|| Group {
            members: vec![name.into()],
            ..Default::default()
        });
    }
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
    let subscription_nodes: HashSet<_> = groups
        .values()
        .filter(|g| g.subscription.is_some())
        .flat_map(|g| g.members.iter().cloned())
        .collect();
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
            .filter(|node| {
                source.include_all && !(name == "PROXY" && subscription_nodes.contains(*node))
            })
            .chain(groups.keys().filter(|g| {
                // Built-in DIRECT/REJECT join only when listed explicitly.
                *g != name
                    && !builtin(g)
                    && (source.include_all_groups
                        || (name == "PROXY" && groups[*g].subscription.is_some()))
                    && !(name == "PROXY"
                        && groups[*g].subscription.is_some()
                        && !groups[*g].include_in_proxy)
            }))
            .filter(|m| {
                filter.as_ref().is_none_or(|r| r.is_match(m))
                    && !exclude.as_ref().is_some_and(|r| r.is_match(m))
            })
            .cloned()
            .collect();
        dynamic.sort();
        let target = result.get_mut(name).unwrap();
        if name == "PROXY" {
            let excluded = |member: &String| {
                subscription_nodes.contains(member)
                    || groups
                        .get(member)
                        .is_some_and(|g| g.subscription.is_some() && !g.include_in_proxy)
            };
            target.members.retain(|m| !excluded(m));
            seen.retain(|m| !excluded(m));
        }
        target
            .members
            .extend(dynamic.into_iter().filter(|m| seen.insert(m.clone())));
    }
    if let Some(selected) = result
        .get("PROXY")
        .and_then(|g| g.selected.as_ref())
        .cloned()
    {
        if (subscription_nodes.contains(&selected)
            || groups
                .get(&selected)
                .is_some_and(|g| g.subscription.is_some() && !g.include_in_proxy))
            && member_path(&result, "PROXY", &selected).is_none()
        {
            result.get_mut("PROXY").unwrap().selected = None;
        }
    }
    Ok(result)
}

/// Find an actual membership path; a parent may pin any reachable descendant
/// without changing the selected member of the shared child group.
pub fn member_path(
    groups: &BTreeMap<String, Group>,
    root: &str,
    target: &str,
) -> Option<Vec<String>> {
    let mut pending = std::collections::VecDeque::from([(root.to_owned(), Vec::new())]);
    let mut seen = HashSet::new();
    while let Some((name, path)) = pending.pop_front() {
        if path.len() >= 32 || !seen.insert(name.clone()) {
            continue;
        }
        let Some(group) = groups.get(&name) else {
            continue;
        };
        if group.members.iter().any(|m| m == target) {
            let mut found = path;
            found.push(target.to_owned());
            return Some(found);
        }
        for member in &group.members {
            if group_edge(groups, &name, member) && member != root {
                let mut next = path.clone();
                next.push(member.clone());
                pending.push_back((member.clone(), next));
            }
        }
    }
    None
}

pub fn member_choices(groups: &BTreeMap<String, Group>, root: &str) -> Vec<String> {
    fn visit(
        groups: &BTreeMap<String, Group>,
        name: &str,
        depth: usize,
        seen: &mut HashSet<String>,
        output: &mut Vec<String>,
    ) {
        if depth >= 32 {
            return;
        }
        if let Some(group) = groups.get(name) {
            for member in &group.members {
                if seen.insert(member.clone()) {
                    output.push(member.clone());
                    if group_edge(groups, name, member) {
                        visit(groups, member, depth + 1, seen, output);
                    }
                }
            }
        }
    }
    let mut seen = HashSet::from([root.to_owned()]);
    let mut output = Vec::new();
    visit(groups, root, 0, &mut seen, &mut output);
    output
}

/// Saved configs need not contain cached subscription nodes yet. Such a pin is
/// kept pending and cannot route until a source supplies an actual member path.
pub fn pending_subscription_selection(groups: &BTreeMap<String, Group>, root: &str) -> bool {
    std::iter::once(root.to_owned())
        .chain(member_choices(groups, root))
        .any(|name| {
            groups
                .get(&name)
                .is_some_and(|g| g.subscription.is_some() && g.members.is_empty())
        })
}
fn subscription_descendant(groups: &BTreeMap<String, Group>, root: &str) -> bool {
    std::iter::once(root.to_owned())
        .chain(member_choices(groups, root))
        .any(|name| groups.get(&name).is_some_and(|g| g.subscription.is_some()))
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
        if group.interface.as_ref().is_some_and(|name| {
            name.trim().is_empty() || name.len() > 256 || name.chars().any(char::is_control)
        }) {
            return Err(format!("proxy group {name}: invalid egress interface"));
        }
        if name.trim().is_empty()
            || name.len() > 64
            || name.chars().any(char::is_control)
            || nodes.contains(name)
        {
            return Err(format!(
                "invalid, reserved or duplicate proxy group name: {name}"
            ));
        }
        total += group.members.len();
        if (group.members.is_empty()
            && !group.include_all
            && !group.include_all_groups
            && group.subscription.is_none())
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
                || (member == name && !builtin(name))
                || (!builtin(member) && !nodes.contains(member) && !groups.contains_key(member))
            {
                return Err(format!(
                    "proxy group {name}: duplicate or undefined member {member}"
                ));
            }
        }
        if group.selected.as_ref().is_some_and(|s| {
            member_path(groups, name, s).is_none()
                && !(!nodes.contains(s)
                    && !groups.contains_key(s)
                    && !builtin(s)
                    && subscription_descendant(groups, name))
        }) {
            return Err(format!(
                "proxy group {name}: selected must be a reachable member"
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
        if group.mode != Mode::Select
            && !(group.url.starts_with("http://") || group.url.starts_with("https://"))
        {
            return Err(format!("proxy group {name}: test URL must use HTTP(S)"));
        }
        if group.mode != Mode::Select {
            let url = crate::config::validate_remote_url(&group.url)
                .map_err(|_| format!("proxy group {name}: invalid test URL"))?;
            if group.url.contains(['#', '@']) || url.port_or_known_default() == Some(0) {
                return Err(format!("proxy group {name}: test URL must not contain credentials, fragments or zero ports"));
            }
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
            if group_edge(groups, name, member) {
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
#[path = "test/groups_test.rs"]
mod tests;
