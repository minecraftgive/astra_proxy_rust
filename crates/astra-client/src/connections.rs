//! Stable destination groups for the connection list; runtime connections stay unchanged.
use astra_model::runtime::Connection;
use std::collections::{HashMap, HashSet};

#[derive(Clone, Debug, Eq, PartialEq, Hash, Ord, PartialOrd)]
pub struct ConnectionTarget {
    pub host: String,
    pub port: u16,
}

impl ConnectionTarget {
    pub fn of(connection: &Connection) -> Self {
        Self {
            host: connection.host.to_ascii_lowercase(),
            port: connection.port,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ConnectionRowKey {
    Target(ConnectionTarget),
    Connection(u64),
}

#[derive(Clone, Debug)]
pub struct ConnectionRow {
    pub key: ConnectionRowKey,
    /// A group carries summed counters here. Its representative ID is never an action target.
    pub connection: Connection,
    pub count: usize,
    pub expanded: bool,
    pub parent: Option<ConnectionTarget>,
    pub child: bool,
    pub fresh: bool,
}

impl ConnectionRow {
    pub fn is_group(&self) -> bool {
        matches!(self.key, ConnectionRowKey::Target(_))
    }

    pub fn label(&self) -> String {
        let target = format!("{}:{}", self.connection.host, self.connection.port);
        if self.is_group() {
            format!(
                "{} {target} ({})",
                if self.expanded { "▾" } else { "▸" },
                self.count,
            )
        } else if self.child {
            format!("  {target}")
        } else {
            target
        }
    }
}

/// Groups are ranked by aggregate traffic; a newly arrived child temporarily pins its group.
/// Filtering happens before this function, so counters describe the displayed connections.
pub fn build_rows(
    connections: Vec<&Connection>,
    expanded: &HashSet<ConnectionTarget>,
    fresh: &HashSet<u64>,
) -> Vec<ConnectionRow> {
    let mut groups: HashMap<ConnectionTarget, Vec<&Connection>> = HashMap::new();
    for connection in connections {
        groups
            .entry(ConnectionTarget::of(connection))
            .or_default()
            .push(connection);
    }
    let mut groups: Vec<_> = groups
        .into_iter()
        .map(|(target, mut connections)| {
            connections.sort_by_key(|c| {
                (
                    std::cmp::Reverse(fresh.contains(&c.id)),
                    std::cmp::Reverse(c.upload.saturating_add(c.download)),
                    c.id,
                )
            });
            let mut summary = connections[0].clone();
            summary.host = target.host.clone();
            summary.upload = 0;
            summary.download = 0;
            summary.elapsed_ms = 0;
            for connection in &connections {
                summary.upload = summary.upload.saturating_add(connection.upload);
                summary.download = summary.download.saturating_add(connection.download);
                summary.elapsed_ms = summary.elapsed_ms.max(connection.elapsed_ms);
            }
            if connections.iter().any(|c| c.outbound != summary.outbound) {
                summary.outbound = crate::i18n::t("Mixed").to_owned();
            }
            if connections.iter().any(|c| c.network != summary.network) {
                summary.network = crate::i18n::t("Mixed").to_owned();
            }
            if connections.iter().any(|c| c.inbound != summary.inbound) {
                summary.inbound = crate::i18n::t("Mixed").to_owned();
            }
            summary.source = None;
            let fresh = connections.iter().any(|c| fresh.contains(&c.id));
            let first_id = connections.iter().map(|c| c.id).min().unwrap();
            (target, summary, connections, fresh, first_id)
        })
        .collect();
    groups.sort_by(|a, b| {
        b.3.cmp(&a.3)
            .then_with(|| {
                b.1.upload
                    .saturating_add(b.1.download)
                    .cmp(&a.1.upload.saturating_add(a.1.download))
            })
            .then_with(|| a.4.cmp(&b.4))
            .then_with(|| a.0.cmp(&b.0))
    });

    let mut rows = Vec::new();
    for (target, summary, connections, group_fresh, _) in groups {
        let grouped = connections.len() > 1;
        if grouped {
            let is_expanded = expanded.contains(&target);
            rows.push(ConnectionRow {
                key: ConnectionRowKey::Target(target.clone()),
                connection: summary,
                count: connections.len(),
                expanded: is_expanded,
                parent: None,
                child: false,
                fresh: group_fresh,
            });
            if !is_expanded {
                continue;
            }
        }
        rows.extend(connections.into_iter().map(|connection| ConnectionRow {
            key: ConnectionRowKey::Connection(connection.id),
            connection: connection.clone(),
            count: 1,
            expanded: false,
            parent: Some(target.clone()),
            child: grouped,
            fresh: fresh.contains(&connection.id),
        }));
    }
    rows
}

#[cfg(test)]
#[path = "test/ui_connections_test.rs"]
mod tests;
