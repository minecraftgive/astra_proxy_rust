//! Pure reconstruction of a diagnostic path. Never creates an App, reads a
//! subscription cache, tests a group, or chooses a fallback member.
use anyhow::{Context, Result};
use astra_model::{
    config::proxy::ProxyConfig,
    groups::{self, Mode},
    nettool::{PinnedEgress, SelectionHop},
    runtime::Snapshot,
};
use std::collections::{BTreeMap, HashSet};

pub(super) fn rebuild(
    text: &str,
    requested: &str,
    runtime: Option<&Snapshot>,
) -> Result<PinnedEgress> {
    // TOML decoder diagnostics may quote source lines containing credentials.
    let mut config: ProxyConfig = toml::from_str(text).map_err(|_| {
        anyhow::anyhow!(
            "Diagnostic configuration cannot be parsed; choose a confirmed concrete node"
        )
    })?;
    // Runtime switches can remove the saved TUN table or create a default TUN
    // table; the source document alone does not describe the active fallback.
    match runtime.map(|snapshot| snapshot.tun) {
        Some(false) => config.tun = None,
        Some(true) => {
            config.tun.get_or_insert_with(Default::default);
        }
        None => {}
    }
    let tags: HashSet<_> = config
        .outbounds
        .iter()
        .map(|node| node.tag().to_owned())
        .collect();
    anyhow::ensure!(
        tags.len() == config.outbounds.len(),
        "Ambiguous outbound names; choose a confirmed concrete node"
    );
    groups::ensure_default(&mut config.proxy_groups, &tags).map_err(anyhow::Error::msg)?;
    config.proxy_groups =
        groups::expand(&config.proxy_groups, &tags).map_err(anyhow::Error::msg)?;
    let needs_runtime = config.proxy_groups.get(requested).is_some_and(|group| {
        !(requested == "DIRECT" && group.mode == Mode::Select && group.members == ["DIRECT"])
    });
    let runtime = runtime
        .filter(|_| needs_runtime)
        .map(|runtime| {
            anyhow::ensure!(
                !runtime.truncated,
                "Runtime snapshot is truncated; choose a confirmed concrete node"
            );
            let by_name: BTreeMap<_, _> = runtime
                .groups
                .iter()
                .map(|group| (group.name.as_str(), group))
                .collect();
            anyhow::ensure!(
                by_name.len() == runtime.groups.len(),
                "Duplicate runtime groups; choose a confirmed concrete node"
            );
            Ok::<_, anyhow::Error>(by_name)
        })
        .transpose()?;
    let mut chain = vec![];
    let mut seen = HashSet::new();
    let mut name = requested.to_owned();
    loop {
        anyhow::ensure!(
            chain.len() <= 32,
            "Selection chain exceeds limit; choose a concrete node"
        );
        if name == "DIRECT"
            && chain
                .last()
                .is_some_and(|hop: &SelectionHop| hop.group == "DIRECT")
        {
            break;
        }
        anyhow::ensure!(name != "REJECT", "REJECT cannot carry a diagnostic probe");
        let Some(group) = config.proxy_groups.get(&name) else {
            anyhow::ensure!(tags.contains(&name), "Unknown selected member {name}; its same-version configuration is unavailable; choose a confirmed concrete node");
            break;
        };
        anyhow::ensure!(
            seen.insert(name.clone()),
            "Selection chain contains a cycle"
        );
        anyhow::ensure!(group.mode != Mode::LoadBalance, "Load-balance group {name} has no fixed next-session member; choose a concrete node (the test will not represent the group)");
        let explicit_direct = runtime.is_none() && name == "DIRECT" && group.members == ["DIRECT"];
        let selected = if explicit_direct {
            "DIRECT".to_owned()
        } else {
            let observed = observed_group(&config, runtime.as_ref(), &name)?;
            observed.selected.clone().with_context(|| {
                format!("Group {name} has no confirmed selection; choose a concrete node")
            })?
        };
        let path = if group.mode == Mode::Select {
            groups::member_path(&config.proxy_groups, &name, &selected).with_context(|| {
                format!("Group {name} selects unknown member {selected}; choose a concrete node")
            })?
        } else {
            anyhow::ensure!(group.members.contains(&selected), "Group {name} selection is absent from the same-version configuration; choose a concrete node");
            vec![selected.clone()]
        };
        let outer = name.clone();
        chain.push(SelectionHop {
            group: name.clone(),
            mode: mode(group.mode).into(),
            selected: path[0].clone(),
            source: if explicit_direct {
                "explicit_direct".into()
            } else {
                format!("runtime.selected:{selected}")
            },
            interface: group.interface.clone(),
        });
        // Manual selections may pin a reachable descendant. Preserve every
        // intervening group's binding, exactly as the runtime's member_path does.
        for (index, intermediate) in path[..path.len() - 1].iter().enumerate() {
            anyhow::ensure!(
                seen.insert(intermediate.clone()) && chain.len() < 32,
                "Selection chain contains a cycle or exceeds limit"
            );
            let observed = observed_group(&config, runtime.as_ref(), intermediate)?;
            let inherited = &config.proxy_groups[intermediate];
            anyhow::ensure!(
                inherited.mode != Mode::LoadBalance,
                "Selection crosses load-balance group {intermediate}; choose a concrete node"
            );
            anyhow::ensure!(observed.selected.is_some(), "Intermediate group {intermediate} has no confirmed selection; choose a concrete node");
            chain.push(SelectionHop {
                group: intermediate.clone(),
                mode: mode(inherited.mode).into(),
                selected: path[index + 1].clone(),
                source: format!("outer_selection:{outer}:{selected}"),
                interface: inherited.interface.clone(),
            });
        }
        name = path.last().unwrap().clone();
    }
    let sources = config.subscription_sources();
    anyhow::ensure!(!sources.values().any(|source| source.managed_nodes.contains(&name)
        || chain.iter().any(|hop| source.managed_groups.contains(&hop.group)))
        && !chain.iter().any(|hop| config.proxy_groups[&hop.group].subscription.is_some()),
        "Subscription-managed path has no confirmed effective configuration version; choose a confirmed concrete local node");
    let binding = chain.iter().find_map(|hop| {
        hop.interface
            .as_ref()
            .map(|name| (name.clone(), format!("group:{}", hop.group)))
    });
    let binding = match binding {
        Some(binding) => Some(binding),
        None => match &config.tun {
            Some(tun) => {
                anyhow::ensure!(!tun.detects_interface(), "The configured default interface is automatic and absent from the runtime snapshot; choose an explicitly bound path");
                tun.interface
                    .clone()
                    .map(|name| (name, "config.tun.interface".into()))
            }
            None => None,
        },
    };
    if let Some((interface, _)) = &binding {
        anyhow::ensure!(
            !interface.trim().is_empty()
                && interface.len() <= 256
                && !interface.chars().any(char::is_control),
            "Invalid effective interface"
        );
    }
    let (effective_interface, interface_source) =
        binding.map_or((None, None), |(name, source)| (Some(name), Some(source)));
    Ok(PinnedEgress {
        version: 1,
        requested_tag: requested.into(),
        leaf_tag: name.clone(),
        effective_interface,
        interface_source,
        selection_chain: chain,
        node: config.outbounds.into_iter().find(|node| node.tag() == name),
    })
}

fn mode(mode: Mode) -> &'static str {
    match mode {
        Mode::Select => "select",
        Mode::Auto => "url-test",
        Mode::Fallback => "fallback",
        Mode::LoadBalance => "load-balance",
    }
}

fn observed_group<'a>(
    config: &ProxyConfig,
    runtime: Option<&BTreeMap<&str, &'a astra_model::runtime::Group>>,
    name: &str,
) -> Result<&'a astra_model::runtime::Group> {
    let observed = runtime
        .and_then(|groups| groups.get(name))
        .copied()
        .with_context(|| {
            format!("Missing runtime layer {name}; choose a confirmed concrete node")
        })?;
    let group = config
        .proxy_groups
        .get(name)
        .context("Unknown group in selection path")?;
    anyhow::ensure!(
        observed.mode == mode(group.mode),
        "Group {name} mode differs from the same-version configuration"
    );
    anyhow::ensure!(observed.members.iter().map(|member| &member.name).eq(group.members.iter()), "Group {name} membership is incomplete or differs from the same-version configuration; choose a confirmed concrete node");
    Ok(observed)
}

pub(super) fn same_path(before: &PinnedEgress, after: &PinnedEgress) -> bool {
    before.requested_tag == after.requested_tag
        && before.leaf_tag == after.leaf_tag
        && before.effective_interface == after.effective_interface
        && before.interface_source == after.interface_source
        && before.selection_chain == after.selection_chain
}

#[cfg(test)]
#[path = "../../test/network_selection_test.rs"]
mod tests;
