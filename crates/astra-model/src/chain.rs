//! Pure chain expansion, shared by validation, connection planning and previews.
use crate::{
    config::protocol::OutboundConfig,
    groups::{Group, Mode},
};
use anyhow::{bail, ensure, Context, Result};
use std::collections::{BTreeMap, HashMap, HashSet};

pub const MAX_HOPS: usize = 32;

#[derive(Default, Debug, Clone)]
pub struct Path {
    pub nodes: Vec<String>,
    pub groups: Vec<String>,
    pub blocked: bool,
}

/// Return dependency-first groups, including dependencies through node dialers.
pub fn validate(
    groups: &BTreeMap<String, Group>,
    nodes: &HashMap<String, OutboundConfig>,
) -> Result<Vec<String>> {
    let groups = crate::groups::expand(groups, &nodes.keys().cloned().collect())
        .map_err(anyhow::Error::msg)?;
    fn visit(
        name: &str,
        groups: &BTreeMap<String, Group>,
        nodes: &HashMap<String, OutboundConfig>,
        active: &mut HashSet<String>,
        done: &mut HashSet<String>,
        order: &mut Vec<String>,
    ) -> Result<()> {
        if done.contains(name) {
            return Ok(());
        }
        ensure!(
            active.len() < MAX_HOPS && active.insert(name.into()),
            "proxy chain cycle or excessive depth at {name}"
        );
        if let Some(group) = groups.get(name) {
            for member in &group.members {
                if member == name && crate::groups::builtin(name) {
                    continue;
                }
                visit(member, groups, nodes, active, done, order)?;
            }
            order.push(name.into());
        } else if let Some(node) = nodes.get(name) {
            if let Some(front) = node.dialer_proxy() {
                ensure!(!front.trim().is_empty(), "{name}: empty dialer_proxy");
                ensure!(
                    !node.requires_direct_entry(),
                    "{name}: QUIC nodes must be the first hop"
                );
                visit(front, groups, nodes, active, done, order)?;
            }
        } else {
            ensure!(
                crate::groups::builtin(name),
                "undefined chain node or group: {name}"
            );
        }
        active.remove(name);
        done.insert(name.into());
        Ok(())
    }
    let mut done = HashSet::new();
    let mut order = Vec::new();
    for name in groups.keys().chain(nodes.keys()) {
        visit(
            name,
            &groups,
            nodes,
            &mut HashSet::new(),
            &mut done,
            &mut order,
        )?;
    }
    // Validate all selectable shapes without enumerating their combinations.
    // The DAG has at most four contexts per name: prior hop × apply dialer.
    fn shape(
        name: &str,
        prior: bool,
        apply: bool,
        groups: &BTreeMap<String, Group>,
        nodes: &HashMap<String, OutboundConfig>,
        memo: &mut HashMap<(String, bool, bool), usize>,
    ) -> Result<usize> {
        let key = (name.to_owned(), prior, apply);
        if let Some(count) = memo.get(&key) {
            return Ok(*count);
        }
        let mut count = 0;
        if let Some(group) = groups.get(name) {
            for member in &group.members {
                if member == name && crate::groups::builtin(name) {
                    continue;
                }
                if group.mode == Mode::Relay {
                    count += shape(
                        member,
                        prior || count > 0,
                        apply && count == 0,
                        groups,
                        nodes,
                        memo,
                    )?;
                } else {
                    count = count.max(shape(member, prior, apply, groups, nodes, memo)?);
                }
                ensure!(count <= MAX_HOPS, "{name}: chain exceeds {MAX_HOPS} hops");
            }
        } else if let Some(node) = nodes.get(name) {
            if apply {
                if let Some(front) = node.dialer_proxy() {
                    count = shape(front, prior, true, groups, nodes, memo)?;
                }
            }
            ensure!(
                !(prior || count > 0) || !node.requires_direct_entry(),
                "{name}: QUIC nodes must be the first hop"
            );
            count += 1;
        }
        ensure!(count <= MAX_HOPS, "{name}: chain exceeds {MAX_HOPS} hops");
        memo.insert(key, count);
        Ok(count)
    }
    let mut memo = HashMap::new();
    for name in groups.keys().chain(nodes.keys()) {
        shape(name, false, true, &groups, nodes, &mut memo)?;
    }
    Ok(order)
}

pub fn resolve(
    root: &str,
    groups: &BTreeMap<String, Group>,
    nodes: &HashMap<String, OutboundConfig>,
    mut select: impl FnMut(&str, &Group) -> Result<String>,
) -> Result<Path> {
    fn walk(
        name: &str,
        parent: Option<&str>,
        apply_dialer: bool,
        depth: usize,
        groups: &BTreeMap<String, Group>,
        nodes: &HashMap<String, OutboundConfig>,
        select: &mut dyn FnMut(&str, &Group) -> Result<String>,
        path: &mut Path,
    ) -> Result<()> {
        ensure!(depth < MAX_HOPS, "proxy chain exceeds {MAX_HOPS} levels");
        if crate::groups::builtin(name) && (parent == Some(name) || !groups.contains_key(name)) {
            path.blocked |= name == "REJECT";
            return Ok(());
        }
        if let Some(group) = groups.get(name) {
            path.groups.push(name.into());
            if group.mode == Mode::Relay {
                for member in &group.members {
                    walk(
                        member,
                        Some(name),
                        apply_dialer && path.nodes.is_empty(),
                        depth + 1,
                        groups,
                        nodes,
                        select,
                        path,
                    )?;
                }
            } else {
                let chosen = select(name, group)?;
                let membership = crate::groups::member_path(groups, name, &chosen)
                    .context("selected member is not reachable")?;
                path.groups
                    .extend(membership[..membership.len() - 1].iter().cloned());
                walk(
                    &chosen,
                    Some(name),
                    apply_dialer,
                    depth + 1,
                    groups,
                    nodes,
                    select,
                    path,
                )?;
            }
        } else {
            let node = nodes
                .get(name)
                .with_context(|| format!("undefined chain node {name}"))?;
            if apply_dialer {
                if let Some(front) = node.dialer_proxy() {
                    walk(front, None, true, depth + 1, groups, nodes, select, path)?;
                }
            }
            if !path.nodes.is_empty() && node.requires_direct_entry() {
                bail!("{name}: QUIC nodes must be the first hop");
            }
            ensure!(
                path.nodes.len() < MAX_HOPS,
                "proxy chain exceeds {MAX_HOPS} hops"
            );
            path.nodes.push(name.into());
        }
        Ok(())
    }
    let mut path = Path::default();
    walk(root, None, true, 0, groups, nodes, &mut select, &mut path)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn node(tag: &str, front: Option<&str>) -> OutboundConfig {
        serde_json::from_value(serde_json::json!({"kind":"socks5", "tag":tag, "server":"127.0.0.1:1080", "dialer_proxy":front})).unwrap()
    }
    #[test]
    fn relay_overrides_later_dialers_and_expands_first_hop() {
        let nodes = [
            node("entry", None),
            node("middle", Some("entry")),
            node("exit", Some("entry")),
        ]
        .into_iter()
        .map(|n| (n.tag().into(), n))
        .collect();
        let groups = BTreeMap::from([(
            "chain".into(),
            Group {
                mode: Mode::Relay,
                members: vec!["middle".into(), "exit".into()],
                ..Default::default()
            },
        )]);
        validate(&groups, &nodes).unwrap();
        let path = resolve("chain", &groups, &nodes, |_, _| unreachable!()).unwrap();
        assert_eq!(path.nodes, ["entry", "middle", "exit"]);
        assert!(crate::groups::member_path(&groups, "chain", "exit").is_none());
    }
    #[test]
    fn detects_cycles_through_groups_and_missing_fronts() {
        let groups = BTreeMap::from([(
            "front".into(),
            Group {
                members: vec!["exit".into()],
                ..Default::default()
            },
        )]);
        let nodes = HashMap::from([("exit".into(), node("exit", Some("front")))]);
        assert!(validate(&groups, &nodes).is_err());
        assert!(validate(&BTreeMap::new(), &nodes).is_err());
    }
    #[test]
    fn group_selection_is_not_a_network_hop_and_reject_is_terminal() {
        let groups = BTreeMap::from([(
            "front".into(),
            Group {
                members: vec!["entry".into(), "REJECT".into()],
                ..Default::default()
            },
        )]);
        let nodes = [node("entry", None), node("exit", Some("front"))]
            .into_iter()
            .map(|n| (n.tag().into(), n))
            .collect();
        let path = resolve("exit", &groups, &nodes, |_, _| Ok("entry".into())).unwrap();
        assert_eq!(path.nodes, ["entry", "exit"]);
        assert_eq!(path.groups, ["front"]);
        assert!(
            resolve("exit", &groups, &nodes, |_, _| Ok("REJECT".into()))
                .unwrap()
                .blocked
        );
    }
}
